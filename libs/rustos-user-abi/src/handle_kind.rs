//! Kernel handle-kind codes reported in refusal diagnostics.
//!
//! The milestone log carries integers, not strings, so a diagnostic that says
//! "the handle behind this fd was the wrong kind" names the kind with one of
//! these codes. They are published numbers: never renumber or reuse one. A
//! kind that is removed moves to [`RETIRED_HANDLE_KIND_CODES`] and its number
//! stays reserved. An earlier hand-assigned table gave `DeviceBuffer` and `Job`
//! overlapping codes while both were being added; [`HANDLE_KINDS`] plus the
//! uniqueness test below is what now prevents that.

pub const HANDLE_KIND_CONSOLE: u64 = 1;
pub const HANDLE_KIND_DEVICE: u64 = 2;
pub const HANDLE_KIND_EPOLL: u64 = 3;
pub const HANDLE_KIND_INET_SOCKET: u64 = 4;
pub const HANDLE_KIND_MEMFD: u64 = 5;
pub const HANDLE_KIND_REMOTE_VFS: u64 = 6;
pub const HANDLE_KIND_SOCKET: u64 = 7;
pub const HANDLE_KIND_VFS_DIRECTORY: u64 = 8;
pub const HANDLE_KIND_DISPLAY_SURFACE: u64 = 9;
pub const HANDLE_KIND_JOB: u64 = 10;
pub const HANDLE_KIND_DEVICE_BUFFER: u64 = 11;
pub const HANDLE_KIND_PIPE: u64 = 12;
pub const HANDLE_KIND_EVENTFD: u64 = 13;

/// Codes that were published and then retired. They stay reserved forever.
pub const RETIRED_HANDLE_KIND_CODES: &[u64] = &[];

/// Every live handle-kind code. A new `pub const HANDLE_KIND_*` must be added
/// here; the test below rejects a declaration missing from this table.
pub const HANDLE_KINDS: &[(&str, u64)] = &[
    ("CONSOLE", HANDLE_KIND_CONSOLE),
    ("DEVICE", HANDLE_KIND_DEVICE),
    ("EPOLL", HANDLE_KIND_EPOLL),
    ("INET_SOCKET", HANDLE_KIND_INET_SOCKET),
    ("MEMFD", HANDLE_KIND_MEMFD),
    ("REMOTE_VFS", HANDLE_KIND_REMOTE_VFS),
    ("SOCKET", HANDLE_KIND_SOCKET),
    ("VFS_DIRECTORY", HANDLE_KIND_VFS_DIRECTORY),
    ("DISPLAY_SURFACE", HANDLE_KIND_DISPLAY_SURFACE),
    ("JOB", HANDLE_KIND_JOB),
    ("DEVICE_BUFFER", HANDLE_KIND_DEVICE_BUFFER),
    ("PIPE", HANDLE_KIND_PIPE),
    ("EVENTFD", HANDLE_KIND_EVENTFD),
];

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    #[test]
    fn handle_kind_codes_are_unique_nonzero_and_not_retired() {
        for (index, (name, code)) in HANDLE_KINDS.iter().enumerate() {
            assert_ne!(*code, 0, "HANDLE_KIND_{name} uses reserved code 0");
            assert!(
                !RETIRED_HANDLE_KIND_CODES.contains(code),
                "HANDLE_KIND_{name}={code} reuses a retired code"
            );
            for (other, other_code) in &HANDLE_KINDS[index + 1..] {
                assert_ne!(code, other_code, "HANDLE_KIND_{name} and HANDLE_KIND_{other} collide");
            }
        }
    }

    #[test]
    fn every_declared_handle_kind_is_in_the_table() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/handle_kind.rs")).unwrap();
        let declared = text
            .lines()
            .filter(|line| line.starts_with("pub const HANDLE_KIND_") && line.contains(": u64 ="))
            .count();
        assert_eq!(declared, HANDLE_KINDS.len(), "HANDLE_KINDS is out of step with the declarations");
    }
}
