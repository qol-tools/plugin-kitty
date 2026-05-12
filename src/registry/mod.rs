//! User-owned template registry.
//!
//! The registry's job is to load the on-disk TOML file (in a
//! cap-std-rooted directory the user owns) into a typed [`Registry`].
//! Every other restore-pipeline component pulls templates by id from
//! this struct; no other path may introduce a template, which is what
//! makes "user owns program identity" structurally true.
//!
//! HMAC signing (security plan card 04) and audit-chain wiring land
//! on a follow-up wave; the load surface defined here is the
//! pre-requisite for both.
//!
//! See:
//! - `workspace/docs/superpowers/specs/2026-05-12-terminal-workspace-restore-design.md`
//!   (Restore templates section)
//! - `workspace/docs/superpowers/plans/2026-05-12-terminal-workspace-restore-security-plan.md`
//!   (cards 01, 03, 04)
//! - `docs/adr/KITTY-1-build-plugin-kitty-terminal-lifecycle.md`

use std::collections::BTreeMap;
use std::io::Read;

use cap_std::fs::Dir;
use serde::Deserialize;

/// Built-in slot names that may appear in `argv` without being
/// declared in a template's `params` section. The names match the
/// design spec verbatim; adding one is a deliberate API change.
pub const BUILTIN_SLOTS: &[&str] = &["HOME", "USER", "pane_cwd", "pane_title"];

/// One parameter slot declared on a template.
///
/// Each declared slot carries a regex that bounds the values a plugin
/// may push through it. The regex is parsed by the substitution layer
/// (next concern), not at load time, so an invalid pattern is reported
/// at the closest-to-use call site, not buried in a load error.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ParamSpec {
    pub regex: String,
    #[serde(default)]
    pub required: bool,
}

/// One template entry in the registry.
///
/// `argv` is preserved verbatim from the on-disk TOML; substitution
/// happens elsewhere. `params` maps slot name to `ParamSpec`. The
/// `dangerous` flag (`sh`/`bash`/`eval`/...) is computed by a separate
/// pass and not represented here.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub description: String,
    pub argv: Vec<String>,
    #[serde(default)]
    pub pre_check: Option<Vec<String>>,
    #[serde(default)]
    pub params: BTreeMap<String, ParamSpec>,
}

/// The on-disk shape: `[template.<id>]` sections.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    #[serde(default)]
    template: BTreeMap<String, Template>,
}

/// In-memory typed view of the user's template registry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registry {
    templates: BTreeMap<String, Template>,
}

/// Reasons the registry loader refused to produce a `Registry`.
///
/// The variants are intentionally distinct so the caller can branch
/// on `NotFound` (seed defaults) vs `Parse` (surface a tampering
/// signal) vs `UndeclaredSlot` (point the user at the exact slot).
#[derive(Debug)]
pub enum LoadError {
    /// The registry file did not exist in the cap-std-rooted dir.
    NotFound,
    /// I/O error reading the registry file (other than `NotFound`).
    Io(std::io::Error),
    /// TOML parse error, including `deny_unknown_fields` violations
    /// from a forged or future config key.
    Parse(toml::de::Error),
    /// `argv` referenced a slot that is neither a built-in nor a
    /// declared `params` entry. Surface name + slot so the user can
    /// fix the registry without guessing.
    UndeclaredSlot { template: String, slot: String },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::NotFound => write!(f, "template registry file not found"),
            LoadError::Io(e) => write!(f, "template registry I/O error: {e}"),
            LoadError::Parse(e) => write!(f, "template registry parse error: {e}"),
            LoadError::UndeclaredSlot { template, slot } => write!(
                f,
                "template `{template}` references undeclared slot `{{{slot}}}` in argv; \
                 add it to `[template.{template}.params.{slot}]` or use a built-in"
            ),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LoadError::Io(e) => Some(e),
            LoadError::Parse(e) => Some(e),
            _ => None,
        }
    }
}

impl Registry {
    /// Load the registry from `file_name` inside `dir`.
    ///
    /// `dir` is a `cap_std::fs::Dir` opened by the caller; every
    /// read in this function is rooted in that handle. A symlink
    /// whose target escapes the sandbox returns an error from the
    /// underlying open call without any user-controlled path entering
    /// a `std::fs::*` call.
    pub fn load(dir: &Dir, file_name: &str) -> Result<Self, LoadError> {
        let mut file = match dir.open(file_name) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(LoadError::NotFound);
            }
            Err(e) => return Err(LoadError::Io(e)),
        };
        let mut body = String::new();
        file.read_to_string(&mut body).map_err(LoadError::Io)?;

        let parsed: RegistryFile = toml::from_str(&body).map_err(LoadError::Parse)?;

        for (id, template) in &parsed.template {
            for arg in &template.argv {
                for slot in argv_slots(arg) {
                    if !is_builtin(&slot) && !template.params.contains_key(&slot) {
                        return Err(LoadError::UndeclaredSlot {
                            template: id.clone(),
                            slot,
                        });
                    }
                }
            }
            if let Some(pre_check) = &template.pre_check {
                for arg in pre_check {
                    for slot in argv_slots(arg) {
                        if !is_builtin(&slot) && !template.params.contains_key(&slot) {
                            return Err(LoadError::UndeclaredSlot {
                                template: id.clone(),
                                slot,
                            });
                        }
                    }
                }
            }
        }

        Ok(Registry {
            templates: parsed.template,
        })
    }

    /// Lookup by template id; `None` if the id is absent.
    pub fn get(&self, id: &str) -> Option<&Template> {
        self.templates.get(id)
    }

    /// Iterate `(id, template)` pairs. Used by the dispatcher to walk
    /// the registry when resolving claims.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Template)> {
        self.templates.iter()
    }

    /// Number of templates currently loaded.
    pub fn len(&self) -> usize {
        self.templates.len()
    }

    /// True iff no templates are loaded.
    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }
}

/// True iff `name` matches a built-in slot. Built-ins are case-sensitive
/// per the spec ("HOME" not "home").
fn is_builtin(name: &str) -> bool {
    BUILTIN_SLOTS.contains(&name)
}

/// Extract `{name}` placeholders from one argv element.
///
/// The grammar is deliberately small: a `{` opens a slot, the next
/// `}` closes it, and the contents must be a non-empty run of
/// `[A-Za-z0-9_]`. Anything else (e.g. unbalanced braces) yields no
/// slots; that case is allowed at load time because the substitution
/// pass surfaces it with better context.
fn argv_slots(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && is_slot_char(bytes[end]) {
                end += 1;
            }
            if end > start && end < bytes.len() && bytes[end] == b'}' {
                // Safe: we only advanced while is_slot_char held, all ASCII.
                out.push(std::str::from_utf8(&bytes[start..end]).unwrap().to_string());
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn is_slot_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
