//! Exact-target lifecycle marker decoding and benchmark evidence rendering.

use super::{hex_field, milestone_name};

#[derive(Clone, Copy)]
pub(super) struct LifecycleIdentity {
    pub(super) process_slot: u32,
    pub(super) process_generation: u32,
    pub(super) mm_generation: u32,
    pub(super) transaction_id: u32,
}

pub(super) struct LifecycleTotal {
    pub(super) name: String,
    pub(super) count: u128,
    pub(super) first: LifecycleIdentity,
    pub(super) last: LifecycleIdentity,
}

/// Lifecycle milestones carry the complete exact-target identity in the two
/// fixed debugcon arguments. Keep the decoded fields in benchmark evidence so
/// a successful latency row cannot be mistaken for a stale PID-only event.
pub(super) fn parse_lifecycle_milestone(line: &str) -> Option<(String, LifecycleIdentity)> {
    let name = milestone_name(line)?;
    if !name.starts_with("lifecycle-") {
        return None;
    }
    let process = hex_field(line, "arg0=")?;
    let mm_transaction = hex_field(line, "arg1=")?;
    let identity = LifecycleIdentity {
        process_slot: process as u32,
        process_generation: (process >> 32) as u32,
        mm_generation: (mm_transaction >> 32) as u32,
        transaction_id: mm_transaction as u32,
    };
    if identity.process_slot == 0
        || identity.process_generation == 0
        || identity.transaction_id == 0
    {
        return None;
    }
    Some((name.to_owned(), identity))
}

pub(super) fn render_lifecycle(lifecycle: &[LifecycleTotal]) -> String {
    if lifecycle.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\nlifecycle exact-target markers (first..last process slot:generation/mm-generation:transaction):\n",
    );
    out.push_str(&format!(
        "  {:<40} {:>8}  {}\n",
        "stage", "count", "identity"
    ));
    out.push_str(&format!("  {}\n", "-".repeat(94)));
    for total in lifecycle {
        let identity = |value: LifecycleIdentity| {
            format!(
                "{}:{}/{}:{}",
                value.process_slot,
                value.process_generation,
                value.mm_generation,
                value.transaction_id
            )
        };
        out.push_str(&format!(
            "  {:<40} {:>8}  {}..{}\n",
            total.name,
            total.count,
            identity(total.first),
            identity(total.last),
        ));
    }
    out
}

pub(super) fn required_lifecycle_stages(probe: &str) -> &'static [&'static str] {
    const SPAWN: &[&str] = &["lifecycle-spawn-reserve", "lifecycle-spawn-publish"];
    const FORK_EXIT: &[&str] = &[
        "lifecycle-spawn-reserve",
        "lifecycle-spawn-publish",
        "lifecycle-exit-seal",
        "lifecycle-reap-queued",
        "lifecycle-reap-complete",
    ];
    const FORK_EXEC: &[&str] = &[
        "lifecycle-spawn-reserve",
        "lifecycle-spawn-publish",
        "lifecycle-exec-reserve",
        "lifecycle-exec-publish",
        "lifecycle-exit-seal",
        "lifecycle-reap-queued",
        "lifecycle-reap-complete",
    ];
    match probe {
        "spawn_activation_to_first_turn" => SPAWN,
        "exit_retire_to_reap" | "fork_exit_wait" => FORK_EXIT,
        "fork_exec_exit_wait" | "exec_replace_single_thread" => FORK_EXEC,
        _ => &[],
    }
}

pub(super) fn lifecycle_trace_holds(lifecycle: &[LifecycleTotal], probe: &str) -> bool {
    required_lifecycle_stages(probe)
        .iter()
        .all(|required| lifecycle.iter().any(|total| total.name == *required))
}
