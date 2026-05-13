//! Platform-isolated helpers used by the lifecycle layer.
//!
//! Per qol-arch-code: cfg(target_os) lives only here as a thin alias
//! over per-OS submodules. Each OS file owns its own implementation and
//! has no cfg attributes internally.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::current_boot_id;
#[cfg(target_os = "macos")]
pub use macos::current_boot_id;
