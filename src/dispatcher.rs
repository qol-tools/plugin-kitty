//! Per-pane plugin dispatcher.
//!
//! The dispatcher is one of plugin-kitty's two "owner" responsibilities
//! (the registry is the other): given a resolved template id and a
//! pane snapshot, walk every plugin that has declared the id under
//! `restore-rule.templates`, ask it for a claim, and return one of
//! three typed shapes:
//!
//! - `Claimed { plugin_id, claim }`: exactly one advertising plugin
//!   returned a claim with the resolved id. The caller materializes
//!   the pane from `claim.template_id` (which equals the dispatched
//!   id) + `claim.params`.
//! - `NoClaim`: zero advertising plugins, or every advertiser returned
//!   `NoOpinion`. The caller falls back to a plain shell launch.
//! - `Conflict { claims }`: two or more advertising plugins each
//!   returned a claim. The dispatcher refuses to choose: the caller
//!   applies qol-tray priority order, or surfaces the conflict to the
//!   user.
//!
//! The transport itself is abstracted to a closure so the orchestration
//! logic can be tested without standing up the full AF_UNIX broker.
//! In production the closure wraps qol-runtime's broker client.
//!
//! See `docs/adr/KITTY-1-build-plugin-kitty-terminal-lifecycle.md`.

use qol_plugin_api::restore::{PaneSnapshot, RestoreClaim};

/// One plugin's restore-rule capability declaration, flattened to
/// the two fields the dispatcher needs: plugin id (echoed back in
/// outcomes) and the template ids the plugin advertises.
#[derive(Debug, Clone)]
pub struct PluginRegistration {
    pub id: String,
    pub templates: Vec<String>,
}

/// One plugin's response to a per-pane `/restore-rule` round-trip.
#[derive(Debug, Clone)]
pub enum ClaimResponse {
    /// A typed claim against the dispatched template id.
    Claim(RestoreClaim),
    /// 204 / no-opinion. The plugin advertised the template but has
    /// nothing to claim for this specific pane.
    NoOpinion,
}

/// Outcome of dispatching one pane.
#[derive(Debug, Clone)]
pub enum DispatchOutcome {
    Claimed {
        plugin_id: String,
        claim: RestoreClaim,
    },
    NoClaim,
    Conflict {
        /// All plugins that returned a claim, in registration order.
        /// The caller decides whether to apply priority or to prompt.
        claims: Vec<(String, RestoreClaim)>,
    },
}

/// Dispatch one pane to every advertising plugin and collapse the
/// responses into a `DispatchOutcome`.
///
/// `template_id` is the id the resolver picked for this pane.
/// `transport(plugin_id, snapshot)` is the closure that performs the
/// real RPC (over qol-runtime's AF_UNIX broker in production, or a
/// test stub in unit tests). Plugins whose `templates` list does not
/// contain `template_id` are filtered out before the transport is
/// invoked: this is the structural bound that keeps unrelated plugins
/// from ever seeing the pane snapshot.
///
/// Claims whose `template_id` does not equal the dispatched id are
/// dropped silently (treated as `NoOpinion`): a plugin must claim
/// the template the resolver picked, not an "upgrade" of its own
/// choosing.
pub fn dispatch_pane<F>(
    plugins: &[PluginRegistration],
    template_id: &str,
    snapshot: &PaneSnapshot,
    mut transport: F,
) -> DispatchOutcome
where
    F: FnMut(&str, &PaneSnapshot) -> ClaimResponse,
{
    let mut claims: Vec<(String, RestoreClaim)> = Vec::new();
    for plugin in plugins {
        if !plugin.templates.iter().any(|t| t == template_id) {
            continue;
        }
        match transport(&plugin.id, snapshot) {
            ClaimResponse::Claim(claim) if claim.template_id == template_id => {
                claims.push((plugin.id.clone(), claim));
            }
            // Mismatched template id is dropped: a plugin must not be
            // able to upgrade or replace the dispatched template by
            // returning a different id.
            ClaimResponse::Claim(_) | ClaimResponse::NoOpinion => {}
        }
    }
    match claims.len() {
        0 => DispatchOutcome::NoClaim,
        1 => {
            let (plugin_id, claim) = claims.into_iter().next().expect("len checked above");
            DispatchOutcome::Claimed { plugin_id, claim }
        }
        _ => DispatchOutcome::Conflict { claims },
    }
}
