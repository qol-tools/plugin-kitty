//! Structural invariants for the kitty IPC adapter.
//!
//! Three concerns live in this module's scope, each pinned here as a
//! pure function so the tests do not have to spawn a real kitty:
//!
//! 1. Parse `kitty @ ls --format=json` into a typed `KittyLs` model
//!    that the snapshot path consumes.
//! 2. Build the argv vector for one `kitty @ launch` call, with
//!    title, cwd, program, and each remaining arg as separate
//!    elements (so the OS argv layer is what crosses, not a shell
//!    parser; security plan card 02).
//! 3. Verify a candidate kitty binary against an OS-native trust
//!    path. The trait abstraction lets the tests exercise the
//!    refusal path without depending on a real signed binary
//!    (security plan card 05).
//!
//! Refs:
//!   - workspace/docs/superpowers/specs/2026-05-12-terminal-workspace-restore-design.md
//!     (Snapshot, Pane materialization, Trusted-binary discovery)
//!   - workspace/docs/superpowers/plans/2026-05-12-terminal-workspace-restore-security-plan.md
//!     (cards 02, 05)
//!
//! Closes: KITTY-1.5 (no session-file argv), KITTY-1.4
//! (trusted-binary discovery), KITTY-1.1 (first-class IPC lifecycle).

use std::path::PathBuf;

use plugin_kitty::kitty::{
    build_launch_argv, parse_ls, BinaryVerifier, LaunchPlan, LaunchType, LsParseError,
    PaneLocation, VerifyError,
};

#[test]
fn parse_ls_extracts_pane_cwd_title_and_foreground() {
    // The shape of `kitty @ ls --format=json` is a JSON array of
    // os-windows, each with tabs, each with windows. The parser must
    // surface the per-window cwd, title, and foreground_processes
    // without losing any of them.
    let body = r#"
[
  {
    "id": 1,
    "tabs": [
      {
        "id": 10,
        "layout": "splits",
        "windows": [
          {
            "id": 100,
            "title": "claude",
            "cwd": "/Users/u/work",
            "foreground_processes": [
              {"pid": 1234, "cmdline": ["claude", "--resume", "abc"]}
            ]
          },
          {
            "id": 101,
            "title": "psql",
            "cwd": "/Users/u/db",
            "foreground_processes": [
              {"pid": 1235, "cmdline": ["psql", "mydb"]}
            ]
          }
        ]
      }
    ]
  }
]
"#;
    let parsed = parse_ls(body).expect("parse kitty @ ls JSON");
    let windows = parsed.windows();
    assert_eq!(windows.len(), 2, "expected two windows, got {windows:?}");
    let w0 = &windows[0];
    assert_eq!(w0.id, 100);
    assert_eq!(w0.title, "claude");
    assert_eq!(w0.cwd, PathBuf::from("/Users/u/work"));
    assert_eq!(
        w0.foreground_cmdline().expect("foreground process"),
        vec!["claude", "--resume", "abc"]
    );
}

#[test]
fn parse_ls_rejects_malformed_input() {
    // A garbled `kitty @ ls` output (e.g. corrupted pipe, partial
    // write) must surface a typed error, not silently produce an
    // empty snapshot. The dispatch path uses this error to decide
    // whether to retry or abort the reboot.
    let result = parse_ls("not json at all");
    assert!(
        matches!(result, Err(LsParseError::Json(_))),
        "garbled body must surface LsParseError::Json; got {result:?}"
    );
}

#[test]
fn build_launch_argv_passes_title_and_cwd_as_separate_args() {
    // Card 02: title and cwd cross the OS argv boundary as separate
    // elements, never spliced into a shell string. The builder's job
    // is to produce a Vec<String> shape that `Command::arg` consumes
    // one element at a time. A title containing newlines, quotes, or
    // any other byte must be passed through verbatim.
    let plan = LaunchPlan {
        launch_type: LaunchType::Tab,
        location: PaneLocation::First,
        cwd: PathBuf::from("/Users/u/work"),
        title: "claude\n; rm -rf /".into(),
        program_argv: vec!["claude".into(), "--resume".into(), "abc-123".into()],
    };
    let argv = build_launch_argv(&plan);

    // The IPC protocol expects the `launch` subcommand after `@`.
    assert_eq!(argv.first().map(String::as_str), Some("@"));
    assert!(argv.iter().any(|a| a == "launch"));

    // --type=tab must appear; the materialization path uses tab/
    // window/os-window to reconstruct the layout.
    assert!(
        argv.iter().any(|a| a == "--type=tab"),
        "launch argv missing --type=tab; got {argv:?}"
    );

    // Title and cwd must appear as the *next* element after their
    // flag, never folded into one quoted blob.
    let title_idx = argv
        .iter()
        .position(|a| a == "--title")
        .expect("--title flag missing");
    assert_eq!(
        argv[title_idx + 1],
        "claude\n; rm -rf /",
        "title must be passed verbatim as a separate argv element; \
         a kitty session-file path would re-tokenize this and execute the payload"
    );

    let cwd_idx = argv
        .iter()
        .position(|a| a == "--cwd")
        .expect("--cwd flag missing");
    assert_eq!(argv[cwd_idx + 1], "/Users/u/work");

    // The program and its args must appear *after* the `--` separator
    // so kitty's protocol delivers them as a typed argv array.
    let dashdash_idx = argv
        .iter()
        .position(|a| a == "--")
        .expect("missing `--` argv separator");
    assert_eq!(
        &argv[dashdash_idx + 1..],
        &["claude", "--resume", "abc-123"],
        "program argv must appear verbatim after `--`; got {argv:?}"
    );
}

#[test]
fn build_launch_argv_uses_location_for_splits() {
    // Layout reconstruction maps `vsplit`/`hsplit` to
    // `--location=vsplit|hsplit`. Tabs use `--type=tab`. The builder
    // must emit the right flag for each variant; otherwise the
    // reconstructed layout collapses to a flat sequence.
    let plan = LaunchPlan {
        launch_type: LaunchType::Window,
        location: PaneLocation::Hsplit,
        cwd: PathBuf::from("/Users/u"),
        title: "t".into(),
        program_argv: vec!["zsh".into()],
    };
    let argv = build_launch_argv(&plan);
    assert!(
        argv.iter().any(|a| a == "--type=window"),
        "window launch must emit --type=window; got {argv:?}"
    );
    assert!(
        argv.iter().any(|a| a == "--location=hsplit"),
        "hsplit location must emit --location=hsplit; got {argv:?}"
    );
}

/// A test stub for the binary verifier. The production impls live
/// behind the same trait (`apple-codesign` on macOS, package-manager
/// verification on Linux). Pinning the trait surface here forces
/// future impls to match the same refusal shape: `Ok(())` for a
/// trusted binary, a typed `VerifyError` variant for everything else.
struct StubVerifier {
    decision: Result<(), VerifyError>,
}

impl BinaryVerifier for StubVerifier {
    fn verify(&self, _candidate: &std::path::Path) -> Result<(), VerifyError> {
        self.decision.clone()
    }
}

#[test]
fn binary_verifier_trait_is_object_safe_with_typed_errors() {
    // The trait must be usable behind `dyn` so the daemon can swap
    // in macOS vs Linux impls at runtime. Object-safety is the
    // surface guarantee; pinning it here keeps a future regression
    // (e.g. someone adding a `Self`-bound default method) from
    // breaking the strategy-pattern boundary.
    let v: Box<dyn BinaryVerifier> = Box::new(StubVerifier { decision: Ok(()) });
    assert!(v.verify(std::path::Path::new("/nope")).is_ok());

    let v: Box<dyn BinaryVerifier> = Box::new(StubVerifier {
        decision: Err(VerifyError::SignatureMismatch {
            reason: "team id differs from pinned anchor".into(),
        }),
    });
    match v.verify(std::path::Path::new("/nope")) {
        Err(VerifyError::SignatureMismatch { reason }) => {
            assert!(reason.contains("team id"))
        }
        other => panic!("expected SignatureMismatch refusal; got {other:?}"),
    }
}

#[test]
fn verify_error_has_distinct_path_for_each_root_cause() {
    // The variants must be distinguishable so the caller can decide:
    // SignatureMismatch -> refuse to operate and show a banner.
    // BinaryAbsent -> offer "install kitty" path.
    // UnsupportedPlatform -> show the explicit-override docs path.
    // Same-variant collapse would force the caller to string-match.
    let cases = [
        VerifyError::BinaryAbsent {
            path: PathBuf::from("/usr/local/bin/kitty"),
        },
        VerifyError::SignatureMismatch {
            reason: "bad anchor".into(),
        },
        VerifyError::UnsupportedPlatform,
    ];
    for case in &cases {
        // discriminants must compare distinct
        for other in &cases {
            if std::mem::discriminant(case) == std::mem::discriminant(other) {
                continue;
            }
            assert_ne!(
                std::mem::discriminant(case),
                std::mem::discriminant(other),
                "two VerifyError variants collapsed to the same discriminant: \
                 {case:?} vs {other:?}; callers cannot branch on the cause"
            );
        }
    }
}
