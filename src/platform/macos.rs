//! macOS boot-id source.
//!
//! macOS has no `/proc/sys/kernel/random/boot_id` equivalent. Instead
//! we read `sysctl kern.boottime` which prints `{ sec = N, usec = N }`
//! followed by a human-readable date. Take the `sec` value: it is the
//! epoch second the kernel booted and is stable for the boot session.

use std::process::Command;

pub fn current_boot_id() -> Option<String> {
    let out = Command::new("sysctl")
        .args(["-n", "kern.boottime"])
        .output()
        .ok()?;
    let stdout = String::from_utf8(out.stdout).ok()?;
    stdout
        .split("sec = ")
        .nth(1)?
        .split(',')
        .next()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
