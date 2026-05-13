//! Workspace snapshot + restore orchestrator.
//!
//! Two entry points the binary exposes:
//!
//! - [`snapshot`] runs `kitty @ ls --format=json`, builds one
//!   `PaneSnapshot` per pane, hands each snapshot to every restore-rule
//!   plugin via [`resolve_via_plugin`] (shell-out to
//!   `plugin-claude-sessions resolve` today; will become an AF_UNIX
//!   broker round-trip once `RUNTIME-1` lands), and persists the result
//!   to `~/.cache/qol-tools/plugin-kitty/last-snapshot.json`.
//!
//! - [`restore`] reads the same snapshot file and replays each entry
//!   via `kitty @ launch`. Panes that carry a `claude-session` claim
//!   are launched with `claude --resume <session_id>`; everything else
//!   falls back to a plain shell launch in the original cwd.
//!
//! The argv shape `[claude, --resume, {session_id}]` is hard-coded
//! against the template id rather than read from a signed registry
//! file. The HMAC-signed template registry from the KITTY-1 design is
//! out of scope for the MVP wiring; this module documents the
//! placeholder so the registry path is a localized substitution later.
//!
//! Pane layout: each restored pane is launched as `--type=window` and
//! the tab layout is forced to `grid` first. Six panes land in a 2x3
//! grid; arbitrary counts auto-arrange via kitty's grid layout.
//! Pixel-perfect split ratios are intentionally not preserved (parent
//! design Non-goals).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use qol_plugin_api::restore::{ForegroundProc, PaneSnapshot, RestoreClaim};
use serde::{Deserialize, Serialize};

use crate::kitty::{
    build_launch_argv, parse_ls, KittyWindow, LaunchPlan, LaunchType, PaneLocation,
};
use crate::resolver::SHELL_BASENAMES;

const SNAPSHOT_SUBPATH: &str = ".cache/qol-tools/plugin-kitty/last-snapshot.json";

/// One pane's worth of persisted snapshot state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotEntry {
    pub pane_id: String,
    pub cwd: PathBuf,
    pub title: String,
    /// `None` when no restore-rule plugin claimed this pane; the
    /// restore pass falls back to a plain shell launch in `cwd`.
    #[serde(default)]
    pub claim: Option<RestoreClaim>,
}

/// On-disk shape of the snapshot file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub saved_at: String,
    pub panes: Vec<SnapshotEntry>,
}

/// Run `kitty @ ls --format=json`, resolve claims for every pane, and
/// persist a snapshot. Returns the number of panes captured.
pub fn snapshot() -> Result<usize> {
    let payload = run_kitty(&["@", "ls", "--format=json"])?;
    let kls = parse_ls(&payload).map_err(|e| anyhow!("parse kitty ls: {e}"))?;

    let mut panes = Vec::new();
    for (idx, win) in kls.windows().into_iter().enumerate() {
        let snap = pane_from_window(idx, win);
        let claim = resolve_via_plugin(&snap).ok().flatten();
        panes.push(SnapshotEntry {
            pane_id: snap.pane_id,
            cwd: snap.cwd,
            title: snap.title,
            claim,
        });
    }

    let snapshot = Snapshot {
        saved_at: timestamp_now(),
        panes,
    };
    write_snapshot(&snapshot)?;
    Ok(snapshot.panes.len())
}

/// Read the persisted snapshot and replay each pane via `kitty @ launch`.
/// Returns the number of panes launched.
pub fn restore() -> Result<usize> {
    let snapshot = read_snapshot()?;
    if snapshot.panes.is_empty() {
        return Ok(0);
    }

    // Force a grid layout up front so kitty arranges the new windows
    // into a regular grid rather than a single-column stack.
    let _ = run_kitty(&["@", "goto-layout", "grid"]);

    let mut launched = 0;
    for entry in &snapshot.panes {
        let plan = launch_plan(entry);
        let argv = build_launch_argv(&plan);
        match run_kitty_owned(&argv) {
            Ok(_) => launched += 1,
            Err(err) => eprintln!(
                "plugin-kitty restore: skip pane {pane}: {err}",
                pane = entry.pane_id
            ),
        }
    }
    Ok(launched)
}

/// Build the kitty @ launch plan for one snapshot entry. The claim, if
/// present and recognized, supplies the program argv; otherwise we
/// fall back to a plain shell launch in the original cwd.
fn launch_plan(entry: &SnapshotEntry) -> LaunchPlan {
    let program_argv = entry
        .claim
        .as_ref()
        .and_then(template_argv)
        .unwrap_or_else(default_shell_argv);
    LaunchPlan {
        launch_type: LaunchType::Window,
        location: PaneLocation::Last,
        cwd: entry.cwd.clone(),
        title: entry.title.clone(),
        program_argv,
    }
}

/// Map a `claude-session` claim to its argv. New templates extend this
/// match; until the registry is signed-on-disk this is the single
/// authoritative substitution table.
fn template_argv(claim: &RestoreClaim) -> Option<Vec<String>> {
    match claim.template_id.as_str() {
        "claude-session" => {
            let uuid = claim.params.get("session_id")?;
            Some(vec!["claude".to_string(), "--resume".to_string(), uuid.clone()])
        }
        _ => None,
    }
}

fn default_shell_argv() -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    vec![shell]
}

/// Shell out to `plugin-claude-sessions resolve` with the snapshot on
/// stdin. Empty stdout = no claim; any parse failure is logged and
/// treated as no claim so a single broken plugin can't poison the
/// snapshot pass.
fn resolve_via_plugin(snapshot: &PaneSnapshot) -> Result<Option<RestoreClaim>> {
    let body = serde_json::to_vec(snapshot)?;
    let mut child = Command::new("plugin-claude-sessions")
        .arg("resolve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn plugin-claude-sessions")?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("no stdin on plugin-claude-sessions child"))?
        .write_all(&body)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        eprintln!(
            "plugin-claude-sessions resolve exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let claim: RestoreClaim = serde_json::from_str(trimmed)?;
    Ok(Some(claim))
}

/// Build a `PaneSnapshot` from one kitty `KittyWindow`. The contract
/// requires the `foreground` chain ordered shallow-to-deep; we mirror
/// kitty's ordering verbatim.
fn pane_from_window(idx: usize, win: &KittyWindow) -> PaneSnapshot {
    let foreground = win
        .foreground_processes
        .iter()
        .map(|p| ForegroundProc {
            pid: p.pid,
            exe: deepest_basename(&p.cmdline),
            argv: p.cmdline.clone(),
            cwd: win.cwd.clone(),
        })
        .collect();
    PaneSnapshot {
        pane_id: format!("pane-{idx}"),
        cwd: win.cwd.clone(),
        title: win.title.clone(),
        foreground,
    }
}

/// Extract the deepest non-shell basename from an argv. Falls back to
/// the basename of argv[0] when every element is a shell so the field
/// is never empty.
fn deepest_basename(argv: &[String]) -> String {
    let basename = |s: &str| {
        Path::new(s)
            .file_name()
            .map(|os| os.to_string_lossy().into_owned())
            .unwrap_or_else(|| s.to_string())
    };
    if let Some(first) = argv.first() {
        let base = basename(first);
        if !SHELL_BASENAMES.contains(&base.as_str()) {
            return base;
        }
        basename(first)
    } else {
        String::new()
    }
}

fn run_kitty(args: &[&str]) -> Result<String> {
    let out = Command::new("kitty").args(args).output().context("spawn kitty")?;
    if !out.status.success() {
        bail!(
            "kitty {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?)
}

/// Variant of [`run_kitty`] that takes owned strings, used after
/// `build_launch_argv` which already produces `Vec<String>`.
fn run_kitty_owned(args: &[String]) -> Result<()> {
    let out = Command::new("kitty").args(args).output().context("spawn kitty")?;
    if !out.status.success() {
        bail!(
            "kitty launch failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn snapshot_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("$HOME unset")?;
    Ok(PathBuf::from(home).join(SNAPSHOT_SUBPATH))
}

fn write_snapshot(snapshot: &Snapshot) -> Result<()> {
    let path = snapshot_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(snapshot)?;
    fs::write(&path, body).with_context(|| format!("write snapshot to {path:?}"))?;
    Ok(())
}

fn read_snapshot() -> Result<Snapshot> {
    let path = snapshot_path()?;
    let body = fs::read(&path).with_context(|| format!("read snapshot at {path:?}"))?;
    Ok(serde_json::from_slice(&body)?)
}

/// ISO-8601 UTC timestamp without external date crates. The snapshot
/// file is for human debugging; second precision is enough.
fn timestamp_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch:{secs}")
}
