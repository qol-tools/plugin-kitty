//! Template-id resolution.
//!
//! Pure mapping from `PaneSnapshot` to `Option<&str>` template id. The
//! resolver does not arbitrate, does not run plugins, and never
//! touches I/O: same snapshot plus same registry yields the same id.
//!
//! Matching surface is intentionally tiny: each template declares a
//! `match_exe` list of foreground executable basenames. The resolver
//! picks the deepest non-shell foreground process from the snapshot
//! and looks up that exe in an index built from the registry. Any
//! exe declared by two templates is a registry-build error so the
//! resolver itself never has to choose between them.
//!
//! See `docs/adr/KITTY-1-build-plugin-kitty-terminal-lifecycle.md`.

use std::collections::BTreeMap;

use qol_plugin_api::restore::{ForegroundProc, PaneSnapshot};

use crate::registry::Registry;

/// The set of shell names treated as "not a foreground program" when
/// walking the foreground chain. Matches the design spec's list
/// verbatim; adding one is a deliberate change.
pub const SHELL_BASENAMES: &[&str] = &["bash", "zsh", "fish", "sh", "nu"];

/// Reasons `ResolverIndex::build` refused to produce an index.
#[derive(Debug)]
pub enum RegistryBuildError {
    /// Two or more templates declare the same exe in `match_exe`.
    /// The resolver would otherwise have to coin-flip between them per
    /// reboot; surfacing this at build time forces the user to fix
    /// their registry instead.
    DuplicateMatchExe { exe: String, templates: Vec<String> },
}

impl std::fmt::Display for RegistryBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryBuildError::DuplicateMatchExe { exe, templates } => write!(
                f,
                "exe `{exe}` is declared as match_exe in multiple templates: {templates:?}. \
                 Each foreground executable may resolve to at most one template; \
                 remove the duplicate from all but one of these entries."
            ),
        }
    }
}

impl std::error::Error for RegistryBuildError {}

/// Index of exe basename -> template id built from a `Registry`.
///
/// `build` is fallible because the registry may declare the same exe
/// under two templates. The resolver function takes the registry
/// directly (not the index) for the common path; callers wanting
/// startup-time conflict detection use `build` on top of the same
/// registry and propagate the error.
#[derive(Debug, Clone, Default)]
pub struct ResolverIndex {
    by_exe: BTreeMap<String, String>,
}

impl ResolverIndex {
    /// Build an exe -> template id index, surfacing duplicates as
    /// `DuplicateMatchExe`.
    pub fn build(registry: &Registry) -> Result<Self, RegistryBuildError> {
        let mut by_exe: BTreeMap<String, String> = BTreeMap::new();
        for (id, template) in registry.iter() {
            for exe in &template.match_exe {
                if let Some(existing) = by_exe.get(exe) {
                    return Err(RegistryBuildError::DuplicateMatchExe {
                        exe: exe.clone(),
                        templates: vec![existing.clone(), id.clone()],
                    });
                }
                by_exe.insert(exe.clone(), id.clone());
            }
        }
        Ok(ResolverIndex { by_exe })
    }

    /// Lookup the template id for a foreground exe basename.
    pub fn get(&self, exe: &str) -> Option<&str> {
        self.by_exe.get(exe).map(String::as_str)
    }
}

/// Resolve a template id for one pane.
///
/// Returns `None` when no template claims the pane's deepest non-shell
/// foreground exe. The dispatcher reads `None` as a 204 / no-opinion
/// and falls back to a plain shell launch for that pane.
pub fn resolve_template_id(registry: &Registry, snapshot: &PaneSnapshot) -> Option<String> {
    let deepest = deepest_non_shell(&snapshot.foreground)?;
    for (id, template) in registry.iter() {
        if template.match_exe.iter().any(|e| e == &deepest.exe) {
            return Some(id.clone());
        }
    }
    None
}

/// Pick the deepest non-shell foreground process.
///
/// `foreground` is ordered shallow-to-deep per `qol_plugin_api`'s
/// `PaneSnapshot` contract. Walking it in reverse and stopping at the
/// first non-shell yields the deepest user-visible program. If every
/// element is a shell, the chain has nothing the resolver can act on
/// and we return `None`.
fn deepest_non_shell(foreground: &[ForegroundProc]) -> Option<&ForegroundProc> {
    foreground.iter().rev().find(|p| !is_shell(&p.exe))
}

fn is_shell(exe: &str) -> bool {
    SHELL_BASENAMES.contains(&exe)
}
