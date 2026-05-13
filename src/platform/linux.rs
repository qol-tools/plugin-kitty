//! Linux boot-id source.
//!
//! `/proc/sys/kernel/random/boot_id` is a UUID the kernel regenerates on
//! every boot. Read once at daemon start; cheap and stable for the
//! whole boot session.

pub fn current_boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
