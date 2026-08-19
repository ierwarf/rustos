//! Generation-bound Linux thread-state synchronization.
//!
//! Scheduler slots are reusable, but a process-state operation may need to
//! inspect or update one thread after releasing the scheduler owner. The
//! fixed lock stays at a stable address for the kernel lifetime; `owner_tid`
//! prevents a stale binding from acquiring authority over a reused slot.

use core::ptr::NonNull;

use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};

#[cfg(not(test))]
use super::runqueue;
use super::{LinuxThreadState, MAX_TASK, ProcessHandle, Scheduler, UserAbi, process_table};

pub(in crate::multitask) struct CurrentLinuxThreadBinding {
    pub(in crate::multitask) process_handle: ProcessHandle,
    pub(in crate::multitask) tid: u64,
    pub(in crate::multitask) abi: UserAbi,
    linux_thread_state: NonNull<LinuxThreadStateLock>,
}

#[derive(Clone, Copy)]
pub(super) struct LinuxThreadStateCell {
    pub(super) owner_tid: Option<u64>,
    pub(super) state: Option<LinuxThreadState>,
}

pub(super) type LinuxThreadStateLock =
    TrackedSpinLock<LinuxThreadStateCell, { LockClass::LinuxThreadState as u8 }>;

pub(super) const fn empty_linux_thread_state_lock() -> LinuxThreadStateLock {
    LinuxThreadStateLock::new(LinuxThreadStateCell {
        owner_tid: None,
        state: None,
    })
}

impl CurrentLinuxThreadBinding {
    pub(in crate::multitask) fn with_thread_state_mut<R>(
        &self,
        f: impl FnOnce(&mut Option<LinuxThreadState>) -> R,
    ) -> Option<R> {
        // SAFETY: production Scheduler storage is initialized once in its
        // static cell and never moved. The binding is created only for that
        // scheduler's admitted current slot. The exact owner TID rejects slot
        // reuse between identity snapshot and lock acquisition.
        let lock = unsafe { self.linux_thread_state.as_ref() };
        // This class is also acquired by scheduler/retirement paths from IRQ
        // context. Keep local interrupts masked for the entire raw-spin guard
        // lifetime; merely masking the acquisition would reopen a same-CPU
        // interrupt deadlock while `f` still owns the guard. Callers already
        // hold the process-generation pin, so this leaf must remain bounded
        // and must not perform IPC or blocking allocation.
        x86_64::instructions::interrupts::without_interrupts(|| {
            let mut cell = lock.lock();
            if cell.owner_tid != Some(self.tid) {
                return None;
            }
            Some(f(&mut cell.state))
        })
    }
}

impl Scheduler {
    pub(super) fn linux_thread_state(&self, slot: usize) -> Option<LinuxThreadState> {
        self.linux_thread_states[slot].lock().state
    }

    pub(super) fn install_linux_thread_state(
        &self,
        slot: usize,
        owner_tid: Option<u64>,
        state: Option<LinuxThreadState>,
    ) {
        assert_eq!(
            owner_tid.is_some(),
            state.is_some(),
            "Linux thread-state invariant: owner and state authority differ"
        );
        let mut cell = self.linux_thread_states[slot].lock();
        cell.owner_tid = owner_tid;
        cell.state = state;
        #[cfg(not(test))]
        runqueue::simd_tls::set_tls_fs_base(
            slot,
            cell.state.map(|state| state.fs_base).unwrap_or(0),
        );
    }

    pub(in crate::multitask) fn current_linux_thread_binding(
        &mut self,
    ) -> Option<CurrentLinuxThreadBinding> {
        let slot = self.current_task_slot();
        let context = self.contexts[slot]?;
        if !context.user_mode {
            return None;
        }

        let abi = context.user_abi?;
        let tid = self.starts[slot].map(|start| start.id)?;
        let process_handle = context.process_handle?;
        let linux_thread_state = NonNull::from(&self.linux_thread_states[slot]);
        Some(CurrentLinuxThreadBinding {
            process_handle,
            tid,
            abi,
            linux_thread_state,
        })
    }

    pub(in crate::multitask) fn queue_linux_signal(
        &mut self,
        process_id: u64,
        task_id: u64,
        signal: u64,
    ) -> bool {
        let Some(slot) = self.find_linux_thread_slot(process_id, task_id) else {
            return false;
        };
        self.queue_linux_signal_to_slot(slot, process_id, signal, 0)
    }

    pub(in crate::multitask) fn queue_linux_process_sigchld(
        &mut self,
        process_id: u64,
        events: u32,
    ) -> bool {
        if events == 0 || events & !rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_MASK != 0 {
            return false;
        }
        let slot = self
            .find_linux_thread_slot(process_id, process_id)
            .or_else(|| {
                (0..MAX_TASK).find(|slot| {
                    !self.retired[*slot]
                        && self.contexts[*slot].is_some_and(|context| {
                            context.user_mode
                                && context.user_abi == Some(UserAbi::Linux)
                                && context.process_id == Some(process_id)
                        })
                })
            });
        let Some(slot) = slot else {
            return false;
        };
        self.queue_linux_signal_to_slot(slot, process_id, rustos_user_abi::linux::SIGCHLD, events)
    }

    pub(super) fn transfer_pending_process_sigchld(&mut self, retiring_slot: usize) {
        let Some(context) = self.contexts[retiring_slot] else {
            return;
        };
        let Some(state) = self.linux_thread_state(retiring_slot) else {
            return;
        };
        let Some(process_id) = context.process_id else {
            return;
        };
        let Some((process_id, events)) = (state.pending_sigchld_events != 0)
            .then_some((process_id, state.pending_sigchld_events))
        else {
            return;
        };
        let target = (0..MAX_TASK)
            .filter(|slot| *slot != retiring_slot && !self.retired[*slot])
            .filter(|slot| {
                self.contexts[*slot].is_some_and(|context| {
                    context.user_mode
                        && context.user_abi == Some(UserAbi::Linux)
                        && context.process_id == Some(process_id)
                        && self.linux_thread_state(*slot).is_some()
                })
            })
            .min_by_key(|slot| {
                (
                    self.starts[*slot].map(|start| start.id) != Some(process_id),
                    *slot,
                )
            });
        let Some(target) = target else {
            return;
        };
        if !self.queue_linux_signal_to_slot(
            target,
            process_id,
            rustos_user_abi::linux::SIGCHLD,
            events,
        ) {
            return;
        }
        let sigchld_bit =
            crate::user::sysops::linux::linux_signal_bit(rustos_user_abi::linux::SIGCHLD)
                .expect("SIGCHLD must have a pending-signal bit");
        let mut retiring_cell = self.linux_thread_states[retiring_slot].lock();
        if let Some(state) = retiring_cell.state.as_mut() {
            state.pending_sigchld_events = 0;
            state.pending_signals &= !sigchld_bit;
        }
    }

    fn queue_linux_signal_to_slot(
        &mut self,
        slot: usize,
        process_id: u64,
        signal: u64,
        sigchld_events: u32,
    ) -> bool {
        if signal == 0 {
            return true;
        }
        let Some(signal_bit) = crate::user::sysops::linux::linux_signal_bit(signal) else {
            return false;
        };
        if signal == rustos_user_abi::linux::SIGCONT || signal == rustos_user_abi::linux::SIGKILL {
            self.continue_linux_process(process_id);
        }
        let should_wake = {
            if self.contexts[slot].is_none() {
                return false;
            }
            let Some(owner_tid) = self.starts[slot].map(|start| start.id) else {
                return false;
            };
            let mut cell = self.linux_thread_states[slot].lock();
            if cell.owner_tid != Some(owner_tid) {
                return false;
            }
            let Some(thread_state) = cell.state.as_mut() else {
                return false;
            };
            if signal == rustos_user_abi::linux::SIGCHLD {
                thread_state.pending_sigchld_events |= sigchld_events;
            } else if sigchld_events != 0 {
                return false;
            }
            thread_state.pending_signals |= signal_bit;
            // The hint must be raised after the state it advertises and while
            // the scheduler lock still excludes the syncing reader.
            super::super::current_identity::raise_signal_pending(slot);
            thread_state.signal_mask & signal_bit == 0
        };

        // Signals participate in the same arm/commit/wake protocol as IPC,
        // futexes, and timers. Directly setting `blocked=false, ready=true`
        // leaves `wake_armed` set; the subsequent commit can then put the
        // task to sleep after the signal has already been consumed.
        !should_wake || self.wake_task_slot(slot)
    }

    pub(in crate::multitask) fn stop_current_linux_process(&mut self, signal: u64) -> bool {
        let current = self.current_task_slot();
        let Some(process_id) = self.contexts[current].and_then(|context| {
            (context.user_mode && context.user_abi == Some(UserAbi::Linux))
                .then_some(context.process_id)
                .flatten()
        }) else {
            return false;
        };
        let mut changed = false;
        for slot in 0..MAX_TASK {
            if self.retired[slot]
                || !self.contexts[slot].is_some_and(|context| {
                    context.user_mode
                        && context.user_abi == Some(UserAbi::Linux)
                        && context.process_id == Some(process_id)
                })
            {
                continue;
            }
            let slot_changed = !self.job_stopped[slot];
            changed |= slot_changed;
            self.job_stopped[slot] = true;
            if slot_changed {
                self.request_runqueue_owner_reschedule(slot);
            }
        }
        if changed {
            let _ = process_table::note_process_stopped(process_id, signal);
            if let Some(parent_process_id) = process_table::parent_process_id_of(process_id)
                && parent_process_id != 0
            {
                let _ = self.queue_linux_process_sigchld(
                    parent_process_id,
                    rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_STOP,
                );
            }
        }
        changed
    }

    fn continue_linux_process(&mut self, process_id: u64) -> bool {
        let mut changed = false;
        for slot in 0..MAX_TASK {
            if self.retired[slot]
                || !self.contexts[slot].is_some_and(|context| {
                    context.user_mode
                        && context.user_abi == Some(UserAbi::Linux)
                        && context.process_id == Some(process_id)
                })
            {
                continue;
            }
            let slot_changed = self.job_stopped[slot];
            changed |= slot_changed;
            self.job_stopped[slot] = false;
            if slot_changed {
                self.request_runqueue_owner_reschedule(slot);
            }
        }
        if changed {
            let _ = process_table::note_process_continued(process_id);
            if let Some(parent_process_id) = process_table::parent_process_id_of(process_id)
                && parent_process_id != 0
            {
                let _ = self.queue_linux_process_sigchld(
                    parent_process_id,
                    rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_CONTINUE,
                );
            }
        }
        changed
    }

    pub(super) fn find_linux_thread_slot(&self, process_id: u64, thread_id: u64) -> Option<usize> {
        for slot in 0..MAX_TASK {
            if self.retired[slot] {
                continue;
            }
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !context.user_mode || context.user_abi != Some(UserAbi::Linux) {
                continue;
            }
            if self.starts[slot].map(|start| start.id) != Some(thread_id) {
                continue;
            }
            if context.process_id == Some(process_id) {
                return Some(slot);
            }
        }
        None
    }
}
