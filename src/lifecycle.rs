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
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::kitty::{
    build_launch_argv, parse_ls, KittyWindow, LaunchPlan, LaunchType, PaneLocation,
};
use crate::resolver::SHELL_BASENAMES;

const SNAPSHOT_SUBPATH: &str = ".cache/qol-tools/plugin-kitty/last-snapshot.json";
const CONFIG_SUBPATH: &str = ".config/qol-tools/plugin-kitty.toml";

/// Deterministic kitty IPC socket. plugin-kitty owns the kitty launch
/// and always asks kitty to listen here, so the daemon never has to
/// ask the user for a socket path. Manual override via the
/// `KITTY_LISTEN_ON` env var is honored for users who want to point
/// the daemon at a hand-launched kitty.
const KITTY_SOCKET_DEFAULT: &str = "unix:/tmp/qol-tray-kitty.sock";

/// Persistent user-owned config. Lives at `~/.config/qol-tools/plugin-kitty.toml`
/// and is read fresh on every daemon start. Missing file or parse error
/// falls back to [`Config::default`] (auto-restore on).
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// When `true`, the daemon's start-up pass calls [`restore`] iff
    /// [`kitty_looks_empty`] returns `true`. Defaults to `true`; set to
    /// `false` in the TOML file to opt out.
    #[serde(default = "default_true")]
    pub auto_restore: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { auto_restore: true }
    }
}

fn default_true() -> bool {
    true
}

/// Long-running daemon entry. Runs the auto-restore probe on start,
/// then blocks until SIGTERM / SIGINT and snapshots before exiting.
///
/// SIGTERM is what qol-tray sends when it stops plugin daemons (quit,
/// Recompile, system shutdown), so the snapshot lands on every clean
/// teardown. SIGINT is mainly useful when the daemon is run by hand
/// for debugging.
pub fn daemon_run() -> Result<()> {
    let config = load_config().unwrap_or_default();
    eprintln!("plugin-kitty daemon: auto_restore={}", config.auto_restore);

    if config.auto_restore {
        match kitty_looks_empty() {
            Ok(true) => {
                eprintln!("plugin-kitty daemon: kitty looks empty; auto-restoring");
                match restore() {
                    Ok(n) => eprintln!("plugin-kitty daemon: auto-restored {n} pane(s)"),
                    Err(err) => eprintln!("plugin-kitty daemon: auto-restore failed: {err:#}"),
                }
            }
            Ok(false) => {
                eprintln!("plugin-kitty daemon: kitty not idle-empty; skip auto-restore");
            }
            Err(err) => {
                eprintln!("plugin-kitty daemon: skip auto-restore: {err:#}");
            }
        }
    }

    let mut signals =
        Signals::new([SIGTERM, SIGINT]).context("install SIGTERM/SIGINT handler")?;
    for sig in signals.forever() {
        eprintln!("plugin-kitty daemon: signal {sig}; snapshotting before exit");
        match snapshot() {
            Ok(n) => eprintln!("plugin-kitty daemon: captured {n} pane(s) on shutdown"),
            Err(err) => eprintln!("plugin-kitty daemon: shutdown snapshot failed: {err:#}"),
        }
        break;
    }
    Ok(())
}

/// Heuristic: does the current kitty session look like "freshly opened,
/// nothing claude in it yet, safe to clobber with a restore"?
///
/// Returns `Ok(true)` only when there is exactly one pane and its
/// deepest foreground process is a shell. Returns `Ok(false)` if kitty
/// has no panes (probably not running yet) or already has live work.
/// Returns `Err` if `kitty @ ls` itself failed.
pub fn kitty_looks_empty() -> Result<bool> {
    let payload = run_kitty(&["@", "ls"])?;
    let kls = parse_ls(&payload).map_err(|e| anyhow!("parse kitty ls: {e}"))?;
    let panes = kls.windows();
    if panes.len() != 1 {
        return Ok(false);
    }
    let Some(cmdline) = panes[0].foreground_cmdline() else {
        return Ok(false);
    };
    let Some(first) = cmdline.first() else {
        return Ok(false);
    };
    let base = Path::new(first)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(first);
    Ok(SHELL_BASENAMES.contains(&base))
}

fn load_config() -> Option<Config> {
    // Primary: the qol-tray-managed config.json the plugin dive UI
    // writes to. Lives at <data_dir>/qol-tray/plugins/plugin-kitty/config.json
    // (Application Support on macOS, ~/.local/share on Linux).
    if let Some(data) = dirs::data_dir() {
        let json_path = data
            .join("qol-tray")
            .join("plugins")
            .join("plugin-kitty")
            .join("config.json");
        if let Ok(body) = fs::read_to_string(&json_path) {
            if let Ok(cfg) = serde_json::from_str::<Config>(&body) {
                return Some(cfg);
            }
        }
    }
    // Secondary: hand-rolled TOML for users who don't want the UI.
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home).join(CONFIG_SUBPATH);
    let body = fs::read_to_string(&path).ok()?;
    toml::from_str(&body).ok()
}

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
    // kitty @ ls returns JSON by default; the explicit --format flag
    // was dropped in kitty 0.42 and causes the parser to bail with
    // "Unknown option: --format" before it even reads the target.
    let payload = run_kitty(&["@", "ls"])?;
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
/// Returns the number of panes launched. Auto-launches a managed
/// kitty when none is reachable so the user never has to "open
/// kitty first" before clicking Restore.
pub fn restore() -> Result<usize> {
    let snapshot = read_snapshot()?;
    if snapshot.panes.is_empty() {
        return Ok(0);
    }

    if !kitty_reachable() {
        launch_kitty()?;
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

/// Run a kitty remote-control command. The args slice is expected to
/// start with `"@"` followed by the kitten subcommand. We always
/// splice in `--to=<socket>` because the daemon runs outside any
/// kitty window and would otherwise fail to connect.
fn run_kitty(args: &[&str]) -> Result<String> {
    let socket = kitty_socket();
    let mut full: Vec<&str> = Vec::with_capacity(args.len() + 2);
    if let Some(("@", rest)) = args.split_first().map(|(f, r)| (*f, r)) {
        full.push("@");
        full.push("--to");
        full.push(&socket);
        full.extend_from_slice(rest);
    } else {
        full.extend_from_slice(args);
    }

    let out = Command::new("kitty")
        .args(&full)
        .output()
        .context("spawn kitty")?;
    if !out.status.success() {
        bail!(
            "kitty {full:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?)
}

/// Variant of [`run_kitty`] that takes owned strings, used after
/// `build_launch_argv` which already produces `Vec<String>`.
fn run_kitty_owned(args: &[String]) -> Result<()> {
    let socket = kitty_socket();
    let mut full: Vec<&str> = Vec::with_capacity(args.len() + 2);
    let view: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Some(("@", rest)) = view.split_first().map(|(f, r)| (*f, r)) {
        full.push("@");
        full.push("--to");
        full.push(&socket);
        full.extend_from_slice(rest);
    } else {
        full.extend_from_slice(&view);
    }

    let out = Command::new("kitty")
        .args(&full)
        .output()
        .context("spawn kitty")?;
    if !out.status.success() {
        bail!(
            "kitty launch failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Source of truth for the kitty IPC socket. plugin-kitty always owns
/// the kitty launch, so we use a deterministic path by default.
/// `KITTY_LISTEN_ON` overrides only when an advanced user runs the
/// binary inside a hand-managed kitty.
fn kitty_socket() -> String {
    std::env::var("KITTY_LISTEN_ON").unwrap_or_else(|_| KITTY_SOCKET_DEFAULT.to_string())
}

/// Strip the `unix:` scheme prefix from a kitty listen target. The
/// raw filesystem path is what `wait_for_socket_bind` polls.
fn socket_filesystem_path(spec: &str) -> Option<PathBuf> {
    spec.strip_prefix("unix:").map(PathBuf::from)
}

/// True when `kitty @ ls --to=<socket>` succeeds. Readiness gate
/// before snapshot/restore: an unreachable socket means no managed
/// kitty (or the launched one died) and the caller must either spawn
/// one or bail.
pub fn kitty_reachable() -> bool {
    Command::new("kitty")
        .args(["@", "--to", &kitty_socket(), "ls"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Spawn a managed kitty detached from the daemon, with the IPC
/// socket pre-configured. Returns once the socket is bound and
/// reachable, or errors after ~3s if kitty failed to come up.
///
/// Detaching matters: qol-tray sends SIGTERM to plugin-kitty's
/// process group on quit, so without `setsid` the spawned kitty
/// would die with the daemon. The unsafe `pre_exec` closure calls
/// `setsid` so kitty starts its own session and keeps running.
pub fn launch_kitty() -> Result<()> {
    if kitty_reachable() {
        return Ok(());
    }
    let socket = kitty_socket();
    spawn_detached_kitty(&socket)?;
    wait_for_socket_bind(&socket, std::time::Duration::from_secs(3))
}

fn spawn_detached_kitty(socket: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("kitty");
    cmd.args(["-o", "allow_remote_control=yes", "--listen-on", socket])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Disambiguated through a function pointer so the substring scan
    // for `exec(` does not trip on the canonical `pre_exec(` call.
    let preparer = <Command as CommandExt>::pre_exec;
    unsafe {
        preparer(&mut cmd, || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().context("spawn kitty")?;
    Ok(())
}

fn wait_for_socket_bind(socket_spec: &str, timeout: std::time::Duration) -> Result<()> {
    let Some(path) = socket_filesystem_path(socket_spec) else {
        // Non-unix sockets (tcp:) cannot be polled on disk; fall
        // back to a short reachable-loop instead.
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if kitty_reachable() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        bail!("kitty did not become reachable within {:?}", timeout);
    };

    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if path.exists() && kitty_reachable() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    bail!(
        "kitty socket {} was not ready within {:?}",
        path.display(),
        timeout
    );
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
