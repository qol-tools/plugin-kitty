//! Structural invariants for plugin-kitty's template registry loader.
//!
//! plugin-kitty's user-owned template registry is the structural collapse
//! for "plugin returns arbitrary command" (security plan card 01) and
//! "template registry tampering" (card 04). Every test in this file
//! pins one invariant that the loader must hold at all times; if any
//! check regresses the entire restore pipeline degrades to an
//! authority-by-plugin shape, which is precisely what the parent design
//! ruled out.
//!
//! These tests are intentionally pure: no live keyring, no live HMAC
//! key handling. HMAC chain wiring lands on a follow-up wave (it is a
//! card-04 defense-in-depth layer on top of this structural shape, not
//! the structural shape itself). The loader's job here is the
//! file-on-disk -> typed Registry mapping with cap-std sandboxing.
//!
//! Refs:
//!   - workspace/docs/superpowers/specs/2026-05-12-terminal-workspace-restore-design.md (Restore templates section)
//!   - workspace/docs/superpowers/plans/2026-05-12-terminal-workspace-restore-security-plan.md (cards 01, 03, 04)
//!
//! Closes: KITTY-1.6, KITTY-1.3 (workspace-state-on-disk pre-reqs).

use cap_std::ambient_authority;
use cap_std::fs::Dir;

use plugin_kitty::registry::{LoadError, Registry};

/// Helper: write a registry file into a fresh tempdir-backed cap-std Dir
/// and return both the Dir and the file name. The caller passes the
/// file name to `Registry::load` so the loader exercises its cap-std
/// rooted reads, not an absolute path.
fn fixture(body: &str) -> (tempfile::TempDir, Dir, &'static str) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let dir = Dir::open_ambient_dir(tmp.path(), ambient_authority()).expect("cap-std Dir");
    std::fs::write(tmp.path().join("plugin-kitty.toml"), body).expect("write fixture");
    (tmp, dir, "plugin-kitty.toml")
}

#[test]
fn registry_parses_template_section_with_argv_and_params() {
    // The canonical shape from the design spec: a [template.<id>] section
    // holds description + argv + per-parameter regex specs. The loader
    // must accept this verbatim and surface the template by id.
    let body = r#"
[template.claude-session]
description = "Resume a Claude Code session from its on-disk transcript."
argv = ["claude", "--resume", "{uuid}"]

[template.claude-session.params.uuid]
regex = "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
required = true
"#;
    let (_tmp, dir, name) = fixture(body);
    let registry = Registry::load(&dir, name).expect("load registry");
    let template = registry
        .get("claude-session")
        .expect("claude-session template must be present");
    assert_eq!(
        template.argv,
        vec!["claude", "--resume", "{uuid}"],
        "argv must be preserved verbatim; the user owns this shape"
    );
    assert!(
        template.params.contains_key("uuid"),
        "params.uuid must be parsed as a slot spec"
    );
    let uuid_spec = &template.params["uuid"];
    assert!(
        uuid_spec.required,
        "required=true round-trip lost; design spec requires the loader to preserve it"
    );
    assert_eq!(
        uuid_spec.regex, "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
        "regex pattern lost in the loader; the slot's structural bound on params is the regex"
    );
}

#[test]
fn unknown_top_level_fields_are_rejected() {
    // deny_unknown_fields keeps a forged or future config key from
    // silently extending the contract. This mirrors qol-plugin-api's
    // RestoreClaim invariant on the other side of the wire.
    let body = r#"
[template.claude-session]
description = "x"
argv = ["claude"]
program = "evil"   # not a real field; loader must refuse
"#;
    let (_tmp, dir, name) = fixture(body);
    match Registry::load(&dir, name) {
        Err(LoadError::Parse(_)) => {}
        other => panic!(
            "loader accepted an unknown top-level template field; \
             returned {other:?} but a malicious config could extend the \
             contract silently. The loader must use deny_unknown_fields."
        ),
    }
}

#[test]
fn argv_must_reference_only_declared_slots() {
    // Every {name} in argv must resolve to a built-in or a declared
    // params entry. An undeclared slot would silently pass nothing
    // through at substitution time, which is a class of bug the loader
    // can rule out at load time.
    let body = r#"
[template.bad]
description = "x"
argv = ["claude", "{undeclared_slot}"]
"#;
    let (_tmp, dir, name) = fixture(body);
    match Registry::load(&dir, name) {
        Err(LoadError::UndeclaredSlot { template, slot }) => {
            assert_eq!(template, "bad", "wrong template id surfaced in error");
            assert_eq!(slot, "undeclared_slot", "wrong slot name surfaced in error");
        }
        other => panic!(
            "loader accepted argv with an undeclared slot; returned {other:?}. \
             Every {{name}} must resolve to a built-in or a declared params entry; \
             this is the load-time invariant that backs runtime substitution safety."
        ),
    }
}

#[test]
fn builtin_slots_are_allowed_without_declaration() {
    // HOME, USER, pane_cwd, pane_title are built-ins per the design
    // spec. The loader must accept argv referencing them without
    // requiring them in `params`.
    let body = r#"
[template.shell-cwd]
description = "open a shell in the pane's cwd"
argv = ["sh", "-l", "-c", "cd {pane_cwd}; exec sh"]
"#;
    let (_tmp, dir, name) = fixture(body);
    let registry = Registry::load(&dir, name).expect("built-in slot {pane_cwd} must load");
    let template = registry.get("shell-cwd").expect("template missing");
    assert!(
        template.argv.iter().any(|a| a.contains("{pane_cwd}")),
        "built-in slot was rewritten or stripped"
    );
}

#[test]
fn missing_file_returns_distinct_error() {
    // A missing registry file is not the same as a parse error. The
    // load path needs to surface a NotFound distinct from Parse so the
    // caller can decide whether to seed defaults vs surface a tampering
    // signal.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let dir = Dir::open_ambient_dir(tmp.path(), ambient_authority()).expect("cap-std Dir");
    match Registry::load(&dir, "absent.toml") {
        Err(LoadError::NotFound) => {}
        other => panic!(
            "loader collapsed `file absent` into another error variant: {other:?}. \
             NotFound must be distinguishable so the caller can decide between \
             seeding defaults and surfacing a tampering signal."
        ),
    }
}

#[test]
fn symlink_pointing_outside_the_sandbox_is_refused() {
    // cap-std rejects symlinks that escape the sandbox. The loader
    // never path-traverses on user input, so a pre-placed symlink
    // under the registry dir cannot redirect reads to ~/.zshrc or any
    // other arbitrary file.
    let outside = tempfile::TempDir::new().expect("outside tempdir");
    std::fs::write(outside.path().join("secret"), "anything").expect("write secret");
    let outside_path = outside.path().join("secret");

    let inside = tempfile::TempDir::new().expect("inside tempdir");
    let link = inside.path().join("plugin-kitty.toml");
    std::os::unix::fs::symlink(&outside_path, &link).expect("create symlink");

    let dir = Dir::open_ambient_dir(inside.path(), ambient_authority()).expect("cap-std Dir");
    let result = Registry::load(&dir, "plugin-kitty.toml");
    assert!(
        result.is_err(),
        "loader read a symlink pointing outside the sandbox; cap-std must have refused \
         the open. Result: {result:?}"
    );
}
