//! Owner: original Current banks/interval and exact target UID metadata loans.
//! Boundary: masked same-CPU Current -> TLS/idle/runtime; pinned target -> snapshot.
//! Lifecycle: original physical/MM/UID/funding units survive the last effect/read.
//! Concurrency: no raw leaf spans another owner or external work; target close wins admission.
//! Failure: missing or closing custody refuses, never falls back to a Scheduler catalog.
//! Forbidden: scalar Current/UID as lifetime, freshly resolved donor as executing funding.
//! Evidence: born-resource regressions; native CPU/ACK inputs are not hardware acceptance.
use super::{current_terminal::OriginalCurrent, stack_resources, task_directory,
    runqueue, current_identity as identity, linux_thread_state};
use nucleus_core::util::lockdep::DispatchPreemption;

fn scope<R>(f: impl FnOnce(&DispatchPreemption<'_>) -> R) -> R {
    let mut scope = DispatchPreemption::enter();
    scope.mask_irqs();
    f(&scope)
}

pub(in crate::multitask) fn runtime() -> Option<crate::multitask::SchedulingContextRuntimeSnapshot> {
    scope(runtime_in)
}
fn runtime_in(scope: &DispatchPreemption<'_>) -> Option<crate::multitask::SchedulingContextRuntimeSnapshot> {
    let current = OriginalCurrent::borrow(scope)?;
    let result = current.runtime_snapshot();
    current.check();
    result
}

pub(in crate::multitask) fn set_tls(value: u64) {
    scope(|scope| { let _ = set_tls_in(scope, value); });
}
fn set_tls_in(scope: &DispatchPreemption<'_>, value: u64) -> bool {
    let Some(current) = OriginalCurrent::borrow(scope) else { return false; };
    let context = current.context();
    if !context.user_mode || context.user_abi != Some(crate::user::abi::UserAbi::Linux) {
        return false;
    }
    let (slot, id) = current.member();
    // The process/thread state accepted this cache value before calling here.
    // Qualify the same Linux cell as well; no stale UID may touch the new cache.
    if !linux_thread_state::exact_state(slot, id).is_some_and(|state| state.fs_base == value) {
        return false;
    }
    runqueue::simd_tls::set_tls_fs_base(slot, value);
    current.check();
    true
}

pub(in crate::multitask) fn mark_root_idle() -> bool {
    scope(mark_root_idle_in)
}
pub(super) fn mark_root_idle_in(scope: &DispatchPreemption<'_>) -> bool {
    let Some(current) = OriginalCurrent::borrow(scope) else { return false; };
    if current.member() != (super::ROOT_TASK_SLOT, 0) || current.context().user_mode {
        return false;
    }
    runqueue::set_idle_slot(super::ROOT_TASK_SLOT, true);
    current.check();
    true
}

struct Target {
    pin: identity::execution::BindingPin,
    context: super::TaskContext,
}
impl Target {
    fn capture(id: u64) -> Option<Self> {
        task_directory::candidates(id).find_map(|slot| {
            let pin = identity::execution::pin_donation(slot, id)?;
            let context = stack_resources::context::read(slot)?;
            if stack_resources::task_start::read(slot)?.id != id || !context.user_mode {
                return None;
            }
            Some(Self { pin, context })
        })
    }
    fn linux(&self, pid: u64) -> Option<crate::multitask::LinuxThreadSnapshot> {
        if self.context.process_id != Some(pid)
            || self.context.user_abi != Some(crate::user::abi::UserAbi::Linux) {
            return None;
        }
        Some(crate::multitask::LinuxThreadSnapshot {
            process_id: pid,
            thread_id: self.pin.id(),
            console_session: self.context.console_session,
            user_stack: self.context.user_stack,
            thread_state: linux_thread_state::exact_state(self.pin.slot(), self.pin.id())?,
        })
    }
}

pub(in crate::multitask) fn alive(id: u64) -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| alive_in(id))
}
fn alive_in(id: u64) -> bool {
    task_directory::candidates(id).any(|slot| identity::observe_user_liveness(slot, id))
}
pub(in crate::multitask) fn log_ids(id: u64) -> Option<(u64, u64)> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let target = Target::capture(id)?;
        Some((target.context.process_id?, target.pin.id()))
    })
}
pub(in crate::multitask) fn linux(pid: u64, id: u64) -> Option<crate::multitask::LinuxThreadSnapshot> {
    x86_64::instructions::interrupts::without_interrupts(|| Target::capture(id)?.linux(pid))
}

#[cfg(test)]
mod tests;
