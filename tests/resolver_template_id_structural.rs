//! Structural invariants for template-id resolution.
//!
//! Given a `PaneSnapshot` from `qol_plugin_api`, the resolver picks
//! one template id from the registry or returns `None` (the dispatcher
//! reads `None` as a 204 / no-opinion). Resolution is a pure function:
//! same snapshot + same registry -> same id, no I/O, no clock.
//!
//! The matching surface here is intentionally narrow: a template
//! declares which foreground executables it covers (`match_exe`), and
//! the resolver picks the template whose `match_exe` list contains the
//! deepest non-shell foreground process's exe basename. Ambiguity at
//! template-build time (two templates claiming the same exe) is a
//! registry error caught at load time; the resolver itself never
//! arbitrates.
//!
//! Refs:
//!   - workspace/docs/superpowers/specs/2026-05-12-terminal-workspace-restore-design.md
//!     (foreground = deepest non-shell process)
//!   - workspace/docs/superpowers/plans/2026-05-12-terminal-workspace-restore-security-plan.md
//!     (card 08: process spoofing is bounded; resolver does not need to
//!     prove identity, only pick a template id)
//!
//! Closes: KITTY-1.7 (generic bridge entry point), KITTY-1.5 (rejects
//! the session-file path: resolver returns ids, not commands).

use std::collections::BTreeMap;
use std::path::PathBuf;

use cap_std::ambient_authority;
use cap_std::fs::Dir;

use qol_plugin_api::restore::{ForegroundProc, PaneSnapshot};

use plugin_kitty::registry::Registry;
use plugin_kitty::resolver::{resolve_template_id, RegistryBuildError};

/// Build a one-template registry from an inline TOML body. Reuses the
/// real loader so tests pin the same parsing path the daemon uses.
fn registry_from(body: &str) -> Registry {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let dir = Dir::open_ambient_dir(tmp.path(), ambient_authority()).expect("cap-std Dir");
    std::fs::write(tmp.path().join("plugin-kitty.toml"), body).expect("write fixture");
    Registry::load(&dir, "plugin-kitty.toml").expect("load fixture registry")
}

fn pane_with_foreground(execs: &[&str]) -> PaneSnapshot {
    PaneSnapshot {
        pane_id: "p0".into(),
        cwd: PathBuf::from("/home/u"),
        title: "t".into(),
        foreground: execs
            .iter()
            .enumerate()
            .map(|(i, exe)| ForegroundProc {
                pid: 1000 + i as u32,
                exe: (*exe).to_string(),
                argv: vec![(*exe).to_string()],
                cwd: PathBuf::from("/home/u"),
            })
            .collect(),
    }
}

#[test]
fn pane_with_claude_foreground_resolves_to_claude_session() {
    // Canonical case: the pane runs `claude` directly. The registry
    // owns the `claude-session` template, which declares `match_exe`
    // covering `claude`. The resolver must return that id.
    let registry = registry_from(
        r#"
[template.claude-session]
description = "Resume a Claude Code session from its on-disk transcript."
argv = ["claude", "--resume", "{uuid}"]
match_exe = ["claude"]

[template.claude-session.params.uuid]
regex = "^[0-9a-f-]{36}$"
required = true
"#,
    );
    let snapshot = pane_with_foreground(&["claude"]);
    assert_eq!(
        resolve_template_id(&registry, &snapshot).as_deref(),
        Some("claude-session"),
        "claude foreground must resolve to the only template that claims it"
    );
}

#[test]
fn pane_without_matching_exe_returns_none() {
    // No template's `match_exe` covers the pane's foreground exe.
    // The resolver returns None; the dispatcher reads None as 204
    // (the pane falls back to a plain shell launch).
    let registry = registry_from(
        r#"
[template.claude-session]
description = "x"
argv = ["claude", "--resume", "{uuid}"]
match_exe = ["claude"]

[template.claude-session.params.uuid]
regex = "^.+$"
required = true
"#,
    );
    let snapshot = pane_with_foreground(&["vim"]);
    assert_eq!(
        resolve_template_id(&registry, &snapshot),
        None,
        "vim foreground must resolve to None: 204 / no-opinion, not a forced template"
    );
}

#[test]
fn empty_foreground_returns_none() {
    // A pane with no foreground process (e.g. a shell at the prompt
    // with no children) has nothing to resolve against. The resolver
    // must surface None without panicking on the empty slice.
    let registry = registry_from(
        r#"
[template.claude-session]
description = "x"
argv = ["claude"]
match_exe = ["claude"]
"#,
    );
    let snapshot = pane_with_foreground(&[]);
    assert_eq!(
        resolve_template_id(&registry, &snapshot),
        None,
        "empty foreground must yield None without indexing past the end"
    );
}

#[test]
fn deepest_non_shell_foreground_wins() {
    // The design spec defines `foreground` as "the deepest non-shell
    // process; shells are bash, zsh, fish, sh, nu". When the
    // foreground chain is [shell, claude], the claude leaf is what
    // resolves, not the shell at index 0.
    let registry = registry_from(
        r#"
[template.claude-session]
description = "x"
argv = ["claude"]
match_exe = ["claude"]
"#,
    );
    let snapshot = pane_with_foreground(&["zsh", "claude"]);
    assert_eq!(
        resolve_template_id(&registry, &snapshot).as_deref(),
        Some("claude-session"),
        "the deepest non-shell process must drive resolution; shells are skipped"
    );
}

#[test]
fn resolution_is_deterministic_across_template_order() {
    // Insertion order from TOML must not change the chosen id.
    // Templates carrying disjoint `match_exe` lists must each resolve
    // to their own id; reordering the registry must not flip results.
    let body_a = r#"
[template.claude-session]
description = "x"
argv = ["claude"]
match_exe = ["claude"]

[template.psql-session]
description = "y"
argv = ["psql"]
match_exe = ["psql"]
"#;
    let body_b = r#"
[template.psql-session]
description = "y"
argv = ["psql"]
match_exe = ["psql"]

[template.claude-session]
description = "x"
argv = ["claude"]
match_exe = ["claude"]
"#;
    let reg_a = registry_from(body_a);
    let reg_b = registry_from(body_b);
    let snap_claude = pane_with_foreground(&["claude"]);
    let snap_psql = pane_with_foreground(&["psql"]);
    assert_eq!(
        resolve_template_id(&reg_a, &snap_claude),
        resolve_template_id(&reg_b, &snap_claude),
        "claude resolution flipped with template order in TOML; resolver must be order-free"
    );
    assert_eq!(
        resolve_template_id(&reg_a, &snap_psql),
        resolve_template_id(&reg_b, &snap_psql),
        "psql resolution flipped with template order in TOML; resolver must be order-free"
    );
}

#[test]
fn duplicate_match_exe_across_templates_fails_at_build() {
    // Two templates claiming the same foreground exe is a registry
    // mistake the resolver itself must not have to arbitrate. The
    // resolver-side builder (the call that wraps a Registry for
    // resolution) must refuse on duplicate `match_exe` entries so the
    // user sees the conflict at startup instead of getting a coin
    // flip per reboot.
    let registry = registry_from(
        r#"
[template.claude-session]
description = "x"
argv = ["claude"]
match_exe = ["claude"]

[template.claude-experimental]
description = "y"
argv = ["claude"]
match_exe = ["claude"]
"#,
    );
    match plugin_kitty::resolver::ResolverIndex::build(&registry) {
        Err(RegistryBuildError::DuplicateMatchExe { exe, templates }) => {
            assert_eq!(exe, "claude", "wrong exe surfaced in error");
            let names: BTreeMap<_, _> = templates.into_iter().map(|t| (t, ())).collect();
            assert!(
                names.contains_key("claude-session") && names.contains_key("claude-experimental"),
                "DuplicateMatchExe must list both conflicting template ids; got {names:?}"
            );
        }
        other => panic!(
            "ResolverIndex::build accepted duplicate match_exe entries; returned {other:?}. \
             The resolver must not arbitrate at runtime; the conflict belongs at build time."
        ),
    }
}
