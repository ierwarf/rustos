# errno sweep review (address_space -> user_pointer errno)

Scope: 119 changed sites in `kernel/compat/src/user/syscall/linux/**` (working tree vs HEAD;
the handoff says 174 originally, the ipc files are already reverted). Review is read-only.

Functions: `user_pointer_error_to_linux_errno` = OutOfFrames->ENOMEM, everything else (incl.
AddressOutOfRange/AddressOverflow) -> EFAULT. `address_space_error_to_linux_errno` maps
AddressOutOfRange/Overflow/NotPageAligned/AlreadyMapped/ZeroSized -> EINVAL.

## Summary

| verdict | count |
|---|---|
| CORRECT (Linux syscall, Linux returns EFAULT) | 55 |
| SUSPECT-(c) mixed error classes | 1 |
| NATIVE-UNPINNED (RustOS-native syscall, no doc/test pins errno) | 63 |

No SUSPECT-(b) with a *documented or tested* EINVAL promise was found: `rg` over docs/ai/abi/**,
docs/ai/contracts-abi.md, docs/architecture, services/**, libs/** finds no EINVAL/-22 promise for any
broker/native struct-pointer op. The only pinned contract is IPC (`apps/ipcbench/src/uaccess_selftest.rs:52`
`[EFAULT,EFAULT,EFAULT,EINVAL,EINVAL]`; `apps/ipcbench/src/ipc_service.rs:42` `register_call(ptr=0)==-22`),
already reverted, and `ipc_ops/**` still uses the old function (grep-verified).

## SUSPECT rows

| file:line | function | verdict | evidence | action |
|---|---|---|---|---|
| service_ops/../syscalld_ops.rs:586 | syscall_linux_memfd_create | SUSPECT (c) | `read_current_user_c_string(name_ptr,249)` (kernel/ps/src/user/sysops/usermem.rs:479-500) returns `AddressOverflow` when no NUL within 249 bytes; Linux memfd_create gives EINVAL for names >249 (EFAULT only for bad pointer). New mapper turns this into EFAULT. A genuine pointer wrap also yields AddressOverflow, so the helper cannot distinguish. | revert this site to `address_space_error_to_linux_errno` (or split the over-long case into an explicit EINVAL) |

### NATIVE-UNPINNED (63 sites, policy decision, not a proven break)

All call `usermem::{read,write,copy,validate}_current_user_*` on a struct/args pointer of a RustOS-native
syscall. None has a doc or test pinning EINVAL; pointer-fault -> EFAULT is the better errno and matches the
IPC *non-kernel-range* rows. Risk is only a behavior change for kernel-range pointers (EINVAL->EFAULT),
which differs from the IPC contract, so decide once whether native brokers should follow the IPC rule.
Recommendation: keep EFAULT, and add one sentence to docs/ai/abi/core.md stating native non-IPC syscalls
return EFAULT for any bad user pointer while IPC syscalls keep EINVAL for kernel-range pointers; add a
kernel unit test beside `user_pointer_errno_tests` for one broker. If the owner instead wants uniformity
with IPC, revert all 63 mechanically (file list below).

Native files (rows): app_buffer_broker_ops(2) checkpoint_journal_ops(3) debug_ops(2) device_broker_ops(2)
early_system_broker_ops(4) entropy_broker_ops(1) frame_source_ops(2) input_broker_ops(6) job_ops(13)
job_policy_ops(3) library_map_probe_ops(3) library_object_ops(3) lifecycle_broker_ops(4) net_broker_ops(6)
offload_ops(2) pager_fault_ops(3) scheduler_ops(1) service_policy_ops(1) smp_qualification_ops(1)
waitset_broker_ops(1).

Notes inside the native set worth a second look:
- job_ops.rs:433-450 / 481-562 / 592-636: copy-in, `validate_current_user_write_buffer` pre-check, and
  copy-out all mapped to EFAULT. Copy-out failure after a committed mutation (comment at job_policy_ops.rs ~105
  "copyout fault is reported but does not roll back") previously also gave EINVAL for kernel-range; no caller
  inspects the value (grep of services/** and libs/** finds no `== -22`/`== -14` on broker results).
- net_broker_ops.rs:218-237: packet tx/rx buffers; `broker_packet_rx` validates first so frame is not lost;
  errno class unchanged in behavior.
- pager_fault_ops.rs:80: `write_dispatch` failure rejects the rendezvous before returning; errno only reported.
- debug_ops.rs:43: `SYS_RUSTOS_DEBUG_PRINT` is write-like; its own null-pointer branch still returns EINVAL
  (debug_ops.rs ~36), so null->EINVAL but unmapped->EFAULT: harmless, but worth documenting.

## CORRECT rows (Linux syscalls, Linux returns EFAULT)

| file:line | syscall/function | evidence | action |
|---|---|---|---|
| memory_ops.rs:66 | syscall_linux_sigaltstack | sigaltstack(2) ss/old_ss: EFAULT on bad pointer | keep |
| memory_ops.rs:80 | syscall_linux_sigaltstack | sigaltstack(2) ss/old_ss: EFAULT on bad pointer | keep |
| memory_ops.rs:142 | syscall_linux_sigaltstack | sigaltstack(2) ss/old_ss: EFAULT on bad pointer | keep |
| service_ops/futex_thread.rs:262 | clone_linux_thread | clone parent_tid/child_tid/ctid: EFAULT | keep |
| service_ops/futex_thread.rs:269 | clone_linux_thread | clone parent_tid/child_tid/ctid: EFAULT | keep |
| service_ops/futex_thread.rs:368 | clone_linux_thread | clone parent_tid/child_tid/ctid: EFAULT | keep |
| service_ops/futex_thread.rs:434 | futex_wait | futex word load (`atomic_load_user_u32`); alignment is checked earlier with EINVAL (`futex_impl`, `uaddr & 0x3`), so only pointer faults reach here: EFAULT | keep |
| service_ops/futex_thread.rs:508 | futex_wait_deadline_tick | futex timeout timespec: EFAULT | keep |
| service_ops/futex_thread.rs:1197 | syscall_linux_arch_prctl | ARCH_GET_FS/GS out pointer: EFAULT | keep |
| service_ops/futex_thread.rs:1336 | syscall_linux_get_robust_list | get_robust_list head/len ptrs: EFAULT | keep |
| service_ops/futex_thread.rs:1339 | syscall_linux_get_robust_list | get_robust_list head/len ptrs: EFAULT | keep |
| service_ops/local_memfd_io.rs:101 | write | memfd write(2) source buffer: EFAULT (partial count preserved by completed_or_error) | keep |
| service_ops/local_memfd_io.rs:127 | read_with | memfd read(2) destination: EFAULT | keep |
| service_ops/local_memfd_io.rs:144 | read_with | memfd read(2) destination: EFAULT | keep |
| service_ops/poll_epoll.rs:231 | collect_poll_readiness | poll/ppoll pollfd array: EFAULT | keep |
| service_ops/poll_epoll.rs:277 | collect_poll_readiness | poll/ppoll pollfd array: EFAULT | keep |
| service_ops/poll_epoll.rs:702 | syscall_linux_ppoll | ppoll timeout timespec: EFAULT | keep |
| service_ops/poll_epoll.rs:742 | with_temporary_wait_signal_mask | ppoll/epoll_pwait sigmask: EFAULT | keep |
| service_ops/poll_epoll.rs:876 | syscall_linux_epoll_ctl | epoll_ctl event ptr: EFAULT | keep |
| service_ops/poll_epoll.rs:967 | syscall_linux_epoll_wait | epoll_wait events buffer: EFAULT | keep |
| service_ops/poll_epoll.rs:1623 | write_epoll_ready_events | epoll_wait event copy-out: EFAULT | keep |
| service_ops/process_time.rs:85 | syscall_linux_rt_sigaction | rt_sigaction act/oact: EFAULT | keep |
| service_ops/process_time.rs:95 | syscall_linux_rt_sigaction | rt_sigaction act/oact: EFAULT | keep |
| service_ops/process_time.rs:115 | syscall_linux_rt_sigaction | rt_sigaction act/oact: EFAULT | keep |
| service_ops/process_time.rs:140 | syscall_linux_rt_sigprocmask | rt_sigprocmask set/oldset: EFAULT | keep |
| service_ops/process_time.rs:149 | syscall_linux_rt_sigprocmask | rt_sigprocmask set/oldset: EFAULT | keep |
| service_ops/process_time.rs:169 | syscall_linux_rt_sigprocmask | rt_sigprocmask set/oldset: EFAULT | keep |
| service_ops/process_time.rs:279 | syscall_linux_nanosleep | nanosleep req: EFAULT | keep |
| service_ops/process_time.rs:291 | syscall_linux_clock_gettime | clock_gettime tp: EFAULT | keep |
| service_ops/process_time.rs:304 | write_clock_timespec | clock_gettime/vDSO-style tp write: EFAULT | keep |
| service_ops/process_time.rs:501 | syscall_linux_clock_nanosleep | clock_nanosleep req: EFAULT | keep |
| service_ops/process_time.rs:556 | syscall_linux_clone3 | clone3 clone_args copy-in: EFAULT | keep |
| service_ops/vfs_meta/inet_datagram.rs:14 | address_from_user | sendto/connect sockaddr: EFAULT | keep |
| service_ops/vfs_meta/inet_datagram.rs:21 | populate_send | sendto/sendmsg buffer, msghdr, iov: EFAULT | keep |
| service_ops/vfs_meta/inet_datagram.rs:46 | populate_send | sendto/sendmsg buffer, msghdr, iov: EFAULT | keep |
| service_ops/vfs_meta/inet_datagram.rs:96 | consume_receive | recvfrom/recvmsg buffer, msghdr, name, control: EFAULT | keep |
| service_ops/vfs_meta/inet_datagram.rs:107 | consume_receive | recvfrom/recvmsg buffer, msghdr, name, control: EFAULT | keep |
| service_ops/vfs_meta/inet_datagram.rs:123 | consume_receive | recvfrom/recvmsg buffer, msghdr, name, control: EFAULT | keep |
| service_ops/vfs_meta/inet_datagram.rs:131 | consume_receive | recvfrom/recvmsg buffer, msghdr, name, control: EFAULT | keep |
| service_ops/vfs_meta/inet_datagram.rs:135 | consume_receive | recvfrom/recvmsg buffer, msghdr, name, control: EFAULT | keep |
| support.rs:274 | read_rt_signal_frame | rt_sigreturn frame read: bad frame -> EFAULT (Linux SIGSEGV) | keep |
| support.rs:366 | install_rt_signal_frame | signal frame push: EFAULT (Linux SIGSEGV) | keep |
| syscalld_ops.rs:31 | syscall_linux_syscalld_uname | uname(2) buf: EFAULT | keep |
| syscalld_ops.rs:42 | syscall_linux_syscalld_uname | uname(2) buf: EFAULT | keep |
| syscalld_ops.rs:58 | syscall_linux_syscalld_prlimit64 | prlimit64 new/old limit: EFAULT | keep |
| syscalld_ops.rs:68 | syscall_linux_syscalld_prlimit64 | prlimit64 new/old limit: EFAULT | keep |
| syscalld_ops.rs:86 | syscall_linux_syscalld_prlimit64 | prlimit64 new/old limit: EFAULT | keep |
| syscalld_ops.rs:106 | syscall_linux_syscalld_sched_getaffinity | sched_getaffinity mask: EFAULT | keep |
| syscalld_ops.rs:133 | syscall_linux_syscalld_sched_getaffinity | sched_getaffinity mask: EFAULT | keep |
| syscalld_ops.rs:150 | syscall_linux_syscalld_sched_setaffinity | sched_setaffinity mask: EFAULT | keep |
| syscalld_ops.rs:424 | syscall_linux_syscalld_getrandom | getrandom buf: EFAULT | keep |
| syscalld_ops.rs:435 | syscall_linux_wait4 | wait4 wstatus/rusage: EFAULT | keep |
| syscalld_ops.rs:443 | syscall_linux_wait4 | wait4 wstatus/rusage: EFAULT | keep |
| syscalld_ops.rs:481 | syscall_linux_wait4 | wait4 wstatus/rusage: EFAULT | keep |
| syscalld_ops.rs:489 | syscall_linux_wait4 | wait4 wstatus/rusage: EFAULT | keep |

## Observations outside the changed set

- `service_ops/vfs_meta.rs` stat/statx/fstatat `validate_current_user_write_buffer` sites still use the old mapper
  (EINVAL for kernel-range). Linux returns EFAULT there; these are the same class the sweep targeted and were
  skipped (185 old-mapper uses remain tree-wide, not audited here). Consider converting the Linux-only ones.
- The working tree also contains unrelated edits inside these files (futex EBUSY->ENOMEM, alignment-after-op check,
  memfd rights EACCES, AT_EMPTY_PATH statx/newfstatat for memfd, process-fault sealing). They are outside the
  sweep and untouched by this review.

## Tests / probes

- `cargo test -p kernel-compat --lib`: 352 passed, 0 failed, 0 ignored (the sweep adds no test pinning broker
  errnos; only `error_ops.rs user_pointer_errno_tests` covers the mapper itself).
- `rg "EINVAL|-22|EFAULT|-14" apps/*/src tools/xtask/src -l` hits: apps/ipcbench/src/{uaccess_selftest,ipc_service,
  ipc_probes}.rs (the only ones asserting pointer errnos; all IPC syscalls, unaffected while ipc_ops stay reverted;
  `ipc_probes.rs:455` pins `-22` on a bad IPC argument, `ipc_service.rs:62` pins `-14` for an unreadable mapped page),
  apps/wayclick/src/main.rs (libc::EINVAL for its own input validation, unrelated), and tools/xtask/src/**
  (matches are `DEFAULT`/`-14`-substring option text, not errno assertions; kvm/tests*.rs do not exercise these
  syscalls). Nothing in apps/ or tools/ pins an errno for any changed Linux or broker site. KVM-level coverage that
  could notice a change: Chrome/Linux userland retrying on EFAULT vs EINVAL (futex, epoll, recvmsg), not run here.
