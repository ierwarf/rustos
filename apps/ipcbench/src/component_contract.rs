//! Shared app/host vocabulary for diagnostic component measurements only.
//! This does not change the ordinary 20,000-sample IPC maximum gate.

pub const FILTER: &str = "ipc_components";
pub const BEGIN: &str = "ipcbench: component begin name=";
pub const END: &str = "ipcbench: component end name=";
pub const PRIMITIVE_ITERS: usize = 10_000;

pub const PROBES: &[(&str, u64)] = &[
    ("vmexit_cpuid", 50_000),
    ("tsc_overhead", 50_000),
    ("null_syscall_getpid", PRIMITIVE_ITERS as u64),
    ("null_syscall_gettid", PRIMITIVE_ITERS as u64),
    ("clock_gettime", PRIMITIVE_ITERS as u64),
    ("sched_yield", PRIMITIVE_ITERS as u64),
    ("ipc_try_recv_empty", PRIMITIVE_ITERS as u64),
    ("ipc_rt_intra_process", 20_000),
    ("ipc_rt_intra_process_reply_recv", 20_000),
];

pub fn split_names(probe: &str) -> &'static [&'static str] {
    match probe {
        "ipc_rt_intra_process" => &[
            "ipc_split_call_to_recv",
            "ipc_split_server_body",
            "ipc_split_reply_to_return",
        ],
        "ipc_rt_intra_process_reply_recv" => &[
            "ipc_split_reply_recv_call_to_recv",
            "ipc_split_reply_recv_server_body",
            "ipc_split_reply_recv_reply_to_return",
        ],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_contract_is_closed_unique_and_keeps_full_ipc_samples() {
        for (i, &(name, count)) in PROBES.iter().enumerate() {
            assert!(count > 0 && !name.is_empty());
            assert!(!PROBES[..i].iter().any(|&(other, _)| other == name));
            assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
            if !split_names(name).is_empty() {
                assert_eq!(count, 20_000);
            }
        }
        assert!(split_names("unknown").is_empty());
        assert_eq!(PROBES[0], ("vmexit_cpuid", 50_000));
    }
}
