//! kitty IPC adapter.
//!
//! Three pure surfaces live here. The orchestrator in
//! `dispatcher.rs` plus a future `lifecycle.rs` will compose them
//! with real I/O; the surfaces themselves stay free of `Command`
//! spawns and live filesystem reads so they can be tested without a
//! running kitty.
//!
//! - [`parse_ls`]: typed `kitty @ ls --format=json` parser. Surfaces
//!   per-window cwd, title, and foreground cmdline.
//! - [`build_launch_argv`]: builds the argv vector for one
//!   `kitty @ launch` call. Title, cwd, program, and each remaining
//!   arg are separate elements; the `--` separator guarantees kitty's
//!   typed protocol receives them as an argv array, not a re-tokenized
//!   string (security plan card 02).
//! - [`BinaryVerifier`]: trait abstraction over the platform-specific
//!   trust paths (apple-codesign on macOS, package-manager integrity
//!   on Linux). [`VerifyError`] variants stay distinguishable so
//!   callers can branch on the cause (card 05).
//!
//! See `docs/adr/KITTY-1-build-plugin-kitty-terminal-lifecycle.md`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

// ---------------------------------------------------------------
// kitty @ ls parser
// ---------------------------------------------------------------

/// Parsed `kitty @ ls --format=json` payload.
///
/// kitty emits a flat array of os-windows; each os-window has tabs,
/// each tab has windows. The flatten helper [`KittyLs::windows`]
/// returns the leaf windows (the panes) so the snapshot path does
/// not have to walk three nested levels.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct KittyLs(pub Vec<OsWindow>);

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct OsWindow {
    pub id: u64,
    pub tabs: Vec<Tab>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Tab {
    pub id: u64,
    #[serde(default)]
    pub layout: String,
    pub windows: Vec<KittyWindow>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct KittyWindow {
    pub id: u64,
    pub title: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub foreground_processes: Vec<ForegroundProcess>,
}

impl KittyWindow {
    /// The deepest foreground process's cmdline, if any.
    pub fn foreground_cmdline(&self) -> Option<&[String]> {
        self.foreground_processes
            .last()
            .map(|p| p.cmdline.as_slice())
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ForegroundProcess {
    pub pid: u32,
    pub cmdline: Vec<String>,
}

impl KittyLs {
    /// Flatten os-windows -> tabs -> windows into a single list.
    /// Callers that need tab grouping use the raw fields instead.
    pub fn windows(&self) -> Vec<&KittyWindow> {
        self.0
            .iter()
            .flat_map(|os| os.tabs.iter().flat_map(|t| t.windows.iter()))
            .collect()
    }
}

/// Reasons [`parse_ls`] refused a payload.
#[derive(Debug)]
pub enum LsParseError {
    Json(serde_json::Error),
}

impl std::fmt::Display for LsParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LsParseError::Json(e) => write!(f, "kitty @ ls JSON parse error: {e}"),
        }
    }
}

impl std::error::Error for LsParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LsParseError::Json(e) => Some(e),
        }
    }
}

/// Parse a `kitty @ ls --format=json` payload.
pub fn parse_ls(body: &str) -> Result<KittyLs, LsParseError> {
    serde_json::from_str(body).map_err(LsParseError::Json)
}

// ---------------------------------------------------------------
// kitty @ launch argv builder
// ---------------------------------------------------------------

/// Where this launch lands in the existing kitty workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchType {
    Window,
    Tab,
    OsWindow,
}

impl LaunchType {
    fn as_flag(self) -> &'static str {
        match self {
            LaunchType::Window => "--type=window",
            LaunchType::Tab => "--type=tab",
            LaunchType::OsWindow => "--type=os-window",
        }
    }
}

/// Position the new pane takes inside its tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneLocation {
    First,
    Last,
    Hsplit,
    Vsplit,
}

impl PaneLocation {
    fn as_flag(self) -> &'static str {
        match self {
            PaneLocation::First => "--location=first",
            PaneLocation::Last => "--location=last",
            PaneLocation::Hsplit => "--location=hsplit",
            PaneLocation::Vsplit => "--location=vsplit",
        }
    }
}

/// Inputs to [`build_launch_argv`].
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub launch_type: LaunchType,
    pub location: PaneLocation,
    pub cwd: PathBuf,
    pub title: String,
    pub program_argv: Vec<String>,
}

/// Build the argv vector for one `kitty @ launch` invocation.
///
/// The caller passes each element to `Command::arg(...)`; there is
/// no shell parser between this vector and kitty. Title, cwd, and
/// every program arg cross the OS argv boundary as separate slots,
/// so newlines, semicolons, quotes, and dollar signs can appear in
/// titles or args without escaping or re-tokenization.
pub fn build_launch_argv(plan: &LaunchPlan) -> Vec<String> {
    let mut argv = Vec::with_capacity(plan.program_argv.len() + 8);
    argv.push("@".into());
    argv.push("launch".into());
    argv.push(plan.launch_type.as_flag().into());
    argv.push(plan.location.as_flag().into());
    argv.push("--cwd".into());
    argv.push(plan.cwd.to_string_lossy().into_owned());
    argv.push("--title".into());
    argv.push(plan.title.clone());
    argv.push("--".into());
    argv.extend(plan.program_argv.iter().cloned());
    argv
}

// ---------------------------------------------------------------
// binary verifier
// ---------------------------------------------------------------

/// Refusal reasons for [`BinaryVerifier::verify`].
///
/// Variants stay distinguishable so the caller can branch:
/// `BinaryAbsent` -> offer install path; `SignatureMismatch` ->
/// refuse to operate and show banner; `UnsupportedPlatform` ->
/// surface the explicit-override docs path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// The candidate path did not exist.
    BinaryAbsent { path: PathBuf },
    /// The candidate exists but failed the platform's trust check
    /// (codesign on macOS, package-manager integrity on Linux).
    SignatureMismatch { reason: String },
    /// No trust path exists for this OS yet (e.g. Windows).
    UnsupportedPlatform,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::BinaryAbsent { path } => {
                write!(f, "kitty binary not found at {path:?}")
            }
            VerifyError::SignatureMismatch { reason } => {
                write!(f, "kitty binary failed trust check: {reason}")
            }
            VerifyError::UnsupportedPlatform => write!(
                f,
                "no OS-native trust path for kitty on this platform; \
                 use an explicit-override registry entry to proceed"
            ),
        }
    }
}

impl std::error::Error for VerifyError {}

/// Trait abstraction over OS-native trust paths for the kitty binary.
///
/// Production impls live in `kitty/platform/{macos,linux}.rs` (one
/// per OS, strategy-pattern compartmentalization). Tests use a stub
/// impl to pin the refusal contract without depending on a real
/// signed binary on the host.
pub trait BinaryVerifier {
    fn verify(&self, candidate: &Path) -> Result<(), VerifyError>;
}
