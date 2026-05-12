//! plugin-kitty: terminal lifecycle plugin for qol-tray.
//!
//! Owns the user-facing template registry, kitty IPC integration, and
//! is the dispatcher that asks other plugins to claim panes after a
//! workspace reboot. The structural invariant that backs every other
//! security control: authority over which programs may run lives in
//! this crate's user-owned template registry, never in plugin returns
//! (see `qol_plugin_api::restore::RestoreClaim`).
//!
//! See `docs/adr/KITTY-1-build-plugin-kitty-terminal-lifecycle.md`.

pub mod dispatcher;
pub mod kitty;
pub mod registry;
pub mod resolver;
