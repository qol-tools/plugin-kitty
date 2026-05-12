//! Structural invariants for the per-pane plugin dispatcher.
//!
//! Once the resolver has picked a template id for a pane, plugin-kitty
//! fans out to every plugin whose `restore-rule.templates` declaration
//! includes that id, collects their responses, and either returns the
//! single claim, returns `NoClaim`, or surfaces `Conflict` when more
//! than one plugin claimed the same pane. Conflict resolution is the
//! caller's job (qol-tray priority order or interactive prompt); the
//! dispatcher only types out the three possible shapes.
//!
//! The transport itself is abstracted to a closure so these tests can
//! pin the orchestration logic without standing up a real AF_UNIX
//! broker. The closure plays the role qol-runtime's broker plays in
//! production.
//!
//! Refs:
//!   - workspace/docs/superpowers/specs/2026-05-12-terminal-workspace-restore-design.md
//!     (HTTP capability contract section: dispatch -> RestoreClaim,
//!     plugins answering with 200/204)
//!   - workspace/docs/superpowers/plans/2026-05-12-terminal-workspace-restore-security-plan.md
//!     (cards 02, 10: timeouts and pure orchestration are layered on
//!     top of this typed surface)
//!
//! Closes: KITTY-1.7, KITTY-1.6 (template-bounded claim flow).

use std::collections::BTreeMap;
use std::path::PathBuf;

use qol_plugin_api::restore::{ForegroundProc, PaneSnapshot, RestoreClaim};

use plugin_kitty::dispatcher::{dispatch_pane, ClaimResponse, DispatchOutcome, PluginRegistration};

fn snapshot() -> PaneSnapshot {
    PaneSnapshot {
        pane_id: "p0".into(),
        cwd: PathBuf::from("/home/u"),
        title: "t".into(),
        foreground: vec![ForegroundProc {
            pid: 1234,
            exe: "claude".into(),
            argv: vec!["claude".into()],
            cwd: PathBuf::from("/home/u"),
        }],
    }
}

fn claim(template_id: &str, params: &[(&str, &str)]) -> RestoreClaim {
    RestoreClaim {
        template_id: template_id.into(),
        params: params
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect::<BTreeMap<_, _>>(),
        env: vec![],
    }
}

#[test]
fn single_plugin_with_matching_template_yields_claimed() {
    // The happy path: one plugin advertises `claude-session`, the
    // resolver picked that id, the plugin returns a claim. The
    // dispatcher returns `Claimed` carrying the plugin id and the
    // claim.
    let plugins = vec![PluginRegistration {
        id: "plugin-claude-sessions".into(),
        templates: vec!["claude-session".into()],
    }];
    let transport = |plugin_id: &str, _: &PaneSnapshot| -> ClaimResponse {
        assert_eq!(plugin_id, "plugin-claude-sessions");
        ClaimResponse::Claim(claim("claude-session", &[("uuid", "abc-123")]))
    };
    match dispatch_pane(&plugins, "claude-session", &snapshot(), transport) {
        DispatchOutcome::Claimed { plugin_id, claim } => {
            assert_eq!(plugin_id, "plugin-claude-sessions");
            assert_eq!(claim.template_id, "claude-session");
        }
        other => panic!(
            "expected Claimed when exactly one plugin claims the pane; got {other:?}. \
             The dispatcher must surface the typed result, not the plugin's raw return."
        ),
    }
}

#[test]
fn no_plugin_advertises_template_yields_no_claim() {
    // If no plugin's `restore-rule.templates` declaration contains
    // the resolved id, the dispatcher must short-circuit to NoClaim
    // without invoking any plugin transport. The caller falls back
    // to a plain shell launch for the pane.
    let plugins = vec![PluginRegistration {
        id: "plugin-psql-sessions".into(),
        templates: vec!["psql-session".into()],
    }];
    let transport = |_: &str, _: &PaneSnapshot| -> ClaimResponse {
        panic!(
            "transport was invoked on a plugin whose templates did not include \
             the resolved id; the dispatcher must filter before sending"
        );
    };
    assert!(matches!(
        dispatch_pane(&plugins, "claude-session", &snapshot(), transport),
        DispatchOutcome::NoClaim
    ));
}

#[test]
fn plugin_returning_no_opinion_is_skipped() {
    // A plugin that advertised the template but, looking at this
    // specific pane, has nothing to claim (204 / NoOpinion) must not
    // count as a claimant. With one advertising plugin returning
    // NoOpinion, the outcome is NoClaim.
    let plugins = vec![PluginRegistration {
        id: "plugin-claude-sessions".into(),
        templates: vec!["claude-session".into()],
    }];
    let transport = |_: &str, _: &PaneSnapshot| -> ClaimResponse { ClaimResponse::NoOpinion };
    assert!(matches!(
        dispatch_pane(&plugins, "claude-session", &snapshot(), transport),
        DispatchOutcome::NoClaim
    ));
}

#[test]
fn multiple_plugin_claims_for_the_same_pane_surface_conflict() {
    // Two plugins advertising the same template both claim the same
    // pane. The dispatcher must surface `Conflict` carrying both
    // claims so the caller can apply priority order or prompt the
    // user; it must never silently pick one.
    let plugins = vec![
        PluginRegistration {
            id: "plugin-claude-sessions".into(),
            templates: vec!["claude-session".into()],
        },
        PluginRegistration {
            id: "plugin-claude-experimental".into(),
            templates: vec!["claude-session".into()],
        },
    ];
    let transport = |plugin_id: &str, _: &PaneSnapshot| -> ClaimResponse {
        ClaimResponse::Claim(claim("claude-session", &[("uuid", plugin_id)]))
    };
    match dispatch_pane(&plugins, "claude-session", &snapshot(), transport) {
        DispatchOutcome::Conflict { claims } => {
            assert_eq!(
                claims.len(),
                2,
                "Conflict must carry every claiming plugin's response; got {claims:?}"
            );
            let ids: Vec<&str> = claims.iter().map(|(p, _)| p.as_str()).collect();
            assert!(
                ids.contains(&"plugin-claude-sessions")
                    && ids.contains(&"plugin-claude-experimental"),
                "Conflict did not include both plugins: {ids:?}"
            );
        }
        other => panic!(
            "expected Conflict when two plugins both claim the pane; got {other:?}. \
             Silently picking one would mask user-visible duplication and let an \
             attacker plugin shadow the intended one."
        ),
    }
}

#[test]
fn dispatcher_does_not_send_to_plugins_that_did_not_declare_the_template() {
    // Two plugins; one advertises the resolved id, one does not.
    // The unadvertising plugin's transport must never be invoked.
    let plugins = vec![
        PluginRegistration {
            id: "plugin-claude-sessions".into(),
            templates: vec!["claude-session".into()],
        },
        PluginRegistration {
            id: "plugin-window-actions".into(),
            templates: vec!["window-action".into()],
        },
    ];
    let mut visited: Vec<String> = Vec::new();
    {
        let transport = |plugin_id: &str, _: &PaneSnapshot| -> ClaimResponse {
            visited.push(plugin_id.to_string());
            ClaimResponse::NoOpinion
        };
        let _ = dispatch_pane(&plugins, "claude-session", &snapshot(), transport);
    }
    assert_eq!(
        visited,
        vec!["plugin-claude-sessions"],
        "dispatcher invoked a plugin whose templates did not include the resolved id. \
         The pre-filter is the structural bound that keeps unrelated plugins from \
         seeing the snapshot at all."
    );
}

#[test]
fn dispatcher_surfaces_claim_with_mismatched_template_id_as_no_claim() {
    // A plugin returning a claim for a *different* template id than
    // the dispatch round asked about must not be accepted: the claim
    // is dropped. This is the structural backstop against a plugin
    // trying to "upgrade" a pane to a richer template the dispatcher
    // had not selected.
    let plugins = vec![PluginRegistration {
        id: "plugin-claude-sessions".into(),
        templates: vec!["claude-session".into()],
    }];
    let transport = |_: &str, _: &PaneSnapshot| -> ClaimResponse {
        // resolver picked claude-session; plugin tries to claim a
        // different template id.
        ClaimResponse::Claim(claim("evil-template", &[]))
    };
    assert!(matches!(
        dispatch_pane(&plugins, "claude-session", &snapshot(), transport),
        DispatchOutcome::NoClaim
    ));
}
