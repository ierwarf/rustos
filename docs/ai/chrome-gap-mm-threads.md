# Chrome gap audit: memory, threads, processes, signals, time

Read-only historical audit (2026-11-14, against `7a269bfc`). **Status note (2026-11-25):**
many items catalogued below have since landed (including `madvise` `DONTNEED`/`FREE`/`POPULATE_*`,
10-resource `prlimit64`, `mremap` (`MM_BROKER_OP_MOVE`), `mlock`/`mlock2`/`mincore`/`msync`,
`sysinfo`/`getrusage`/`getres[ug]id`/`setres[ug]id`/`setre[ug]id`/`sched_*`, `SIGBUS` on
truncated file mappings, and `wl_keyboard.keymap` read-only memfd alias), and desktop
Chrome (`--no-sandbox`) now runs cleanly through 60 s KVM holds (see [Session Handoff](session-handoff.md)).
Sandbox items (M1 namespaces, M2 seccomp-bpf) and several SHOULD-FIX syscalls (`timerfd`/`signalfd`/`pidfd`,
process-group `kill(pid <= 0)`, PI futex) remain open.

Original scope note — target:
`/apps/chrome/chrome --ozone-platform=wayland` (Chrome 157 amd64, software
rendering over wl_shm), launched WITHOUT `--no-sandbox` (docs/ai/large-file-io-plan.md:240
forbids adding it as a workaround). Dated evidence, not a proof of the current
checkout. Chrome behavior is from knowledge of the Chromium sources
(`sandbox/linux`, `content/zygote`, `base/`, PartitionAlloc, V8), not re-read
this session; items marked (unverified) depend on that.

Out of scope and NOT listed as gaps: GPU acceleration, libgbm/DRM, >4 vCPU,
`app_buffer` dmabuf (see [Session Handoff](session-handoff.md)). Networking, VFS,
epoll/eventfd/pipe and Wayland protocol gaps belong to other audits.

## 0. How dispatch works (evidence)

- Primary table: `kernel/compat/src/user/syscall/linux.rs:154-508`; fallthrough
  `dispatch_ext.rs:8-29` (pipe/eventfd/pid getters); anything else goes to
  `error_ops::unimplemented_linux_syscall` (`error_ops.rs:39`) which returns ENOSYS.
- A syscall that `rg 'linux_abi::SYS_<NAME>' kernel/compat` does not find is
  `not dispatched`. The full dispatched non-RustOS set is 118 numbers.
- Policy lives in Ring3: `services/syscalld/src/linux_policy.rs`, `services/procd/src/main.rs`.
  Fixes below keep that split (validation in the service, mechanism in ring0).

## 1. Dispatched and adequate (no action)

mmap/munmap/mprotect/brk (VMA split/merge, MAP_FIXED, MAP_FIXED_NOREPLACE,
unknown flags such as NORESERVE/STACK/POPULATE ignored), memfd_create,
futex WAIT/WAKE/WAIT_BITSET/WAKE_BITSET/REQUEUE/CMP_REQUEUE with PRIVATE and
CLOCK_REALTIME, set_tid_address, set/get_robust_list, clone thread path
(CLONE_SETTLS/PARENT_SETTID/CHILD_CLEARTID/CHILD_SETTID/SYSVSEM), fork-shaped
clone, execve/execveat(flags==0), arch_prctl SET_FS/GET_FS, ARCH_CET probe
(EINVAL is the correct "unsupported" answer), sigaltstack, rt_sigaction,
rt_sigprocmask, tkill/tgkill, wait4 (WNOHANG/WUNTRACED/WCONTINUED), sched_yield,
sched_get/setaffinity, nanosleep/clock_nanosleep, clock_gettime for
REALTIME/MONOTONIC/RAW/COARSE/BOOTTIME (`process_time.rs:238-262`), prctl
SET/GET_NAME, NO_NEW_PRIVS, DUMPABLE (`prctl_ops.rs:122`), rseq -> ENOSYS
(glibc tolerates it), getrandom.

## 2. MUST-FIX, in startup order

### M1. Sandbox bring-up: user namespaces, `clone` namespace flags, `unshare`, `setns`, uid/gid maps, chroot
- Chrome: browser calls `Credentials::CanCreateProcessInNewUserNS` (a throwaway
  `clone(CLONE_NEWUSER|SIGCHLD)`), then launches the zygote with
  `CLONE_NEWUSER|CLONE_NEWPID|CLONE_NEWNET` (`NamespaceSandbox`), the child writes
  `/proc/self/{uid_map,gid_map,setgroups}`, then `chroot`/`unshare(CLONE_FS|CLONE_NEWNET)`,
  `setresuid`, `capget/capset` and drops caps. Without any of it: "No usable sandbox!" fatal.
- RustOS: `valid_process_fork_plan` accepts only `SIGCHLD|CHILD_SETTID|CHILD_CLEARTID` and
  requires `ptid==0,tls==0` (`services/procd/src/main.rs:652-665`), so every NEW*/VFORK/VM/
  PARENT_SETTID/SETTLS fork is EINVAL. `unshare`, `setns`, `chroot`, `pivot_root`, `capget`,
  `capset`, `setresuid/setresgid/setgroups` are `not dispatched`. clone3 rejects
  `CLONE_PIDFD`, `set_tid`, `CLONE_INTO_CGROUP` with ENOSYS (`process_time.rs:556-564`).
- Linux: new user ns grants full caps inside it, maps via uid_map, NEWPID gives child pid 1
  and a fresh pid view, NEWNET an empty net stack, NEWNS a private mount tree.
- Sketch (needs a user decision, see section 6): model the namespace set as a per-process
  `NamespaceSet` ids owned by `procd` (policy) + `rootd` identity; `procd` admits the flags
  in `valid_process_fork_plan`, `RustosProcForkBrokerArgs` carries them, `vfsd` keeps the
  per-process root (chroot) and `/proc/self/{uid_map,gid_map,setgroups}` in
  `vfsd/src/proc_fs.rs`; add `unshare/setns/chroot/capget/capset/setres*` dispatch in
  `dispatch_ext.rs` forwarding to `syscalld`/`procd`. pid translation (NEWPID) must be done in
  `procd` (getpid/getppid/waitpid/kill/tgkill/clone-return are all pid-visible).

### M2. seccomp-bpf: `prctl(PR_SET_SECCOMP)`, `seccomp()`, SIGSYS delivery
- Chrome: every sandboxed process installs a classic-BPF filter
  (`sandbox/linux/seccomp-bpf`) with `PR_SET_NO_NEW_PRIVS` then `seccomp(SECCOMP_SET_MODE_FILTER,
  TSYNC)` (EINVAL falls back to prctl). Disallowed calls are answered by a SIGSYS trap handler
  (`seccomp-bpf/trap.cc`) that needs `si_code=SYS_SECCOMP`, `si_syscall`, `si_arch`,
  `si_call_addr`, and writes the emulated result into `uc_mcontext.gregs[REG_RAX]`.
- RustOS: `SYS_SECCOMP` `not dispatched`; `PR_SET_SECCOMP/PR_GET_SECCOMP` -> EINVAL
  (`prctl_ops.rs:215-228` test list). `LinuxSigInfo::for_signal` only sets si_signo
  (`support.rs:67-72`).
- Sketch: store a `Vec<cBPF program>` per thread group in `ProcessSecurityContext` (already holds
  no_new_privs/dumpable); evaluate in `dispatch_linux_syscall` before the match
  (`linux.rs:154-165`) using a small cBPF interpreter crate (data = `seccomp_data`:
  nr, arch=AUDIT_ARCH_X86_64, ip, args). Actions: ALLOW, ERRNO, TRAP (queue SIGSYS with the
  siginfo above), KILL_PROCESS, TRACE->ENOSYS, LOG. Requires M3 for the TRAP path. Fork/clone
  must inherit; execve keeps the filter; TSYNC applies to all threads.

### M3. Synchronous signals from faults and siginfo content
- Chrome: SIGSYS (M2); V8 WebAssembly trap handler and `base::debug` use SA_SIGINFO
  SIGSEGV handlers that read `si_addr` and rewrite `uc_mcontext.rip`; crashpad/breakpad rely
  on SIGSEGV/SIGBUS/SIGILL/SIGFPE; PartitionAlloc guard-page crashes depend on it.
- RustOS: hardware faults retire the task and report a wait status only
  (`lifecycle_broker_ops.rs:74`, `linux.rs:578-595`); signals are delivered only on syscall
  return (`linux.rs:161,514`, `support.rs:95`), not on interrupt return, so `tgkill` to a thread
  spinning in user mode is not delivered until it enters the kernel. siginfo has no
  si_code/si_addr/si_pid/si_uid/si_value (`support.rs:67`). `SA_RESTART`, `SA_NODEFER`,
  `SA_RESETHAND` are not honoured anywhere (`rg SA_RESTART kernel` is empty; Chrome loops
  on EINTR, so this part is SHOULD).
- Sketch: exception path (hal hook that today sets `user_fault_vector`) builds a
  `LinuxRtSigFrame` for a registered handler (SIGSEGV code SEGV_MAPERR/ACCERR with CR2,
  SIGBUS, SIGFPE, SIGILL, SIGTRAP) and only kills the process when the signal is blocked,
  ignored or default; extend `LinuxSigInfo` to a typed struct; deliver pending signals also on
  the timer/IPI interrupt-return path. Owner: `kernel/compat/src/user/syscall/linux/support.rs`,
  `kernel/ps/src/multitask/retirement.rs`, exception entry in `kernel/executive`.

### M4. `madvise` is validate-only; `MADV_DONTNEED/FREE` do not zero or release pages
- Chrome/glibc: PartitionAlloc decommits with `madvise(DONTNEED)` and assumes the next touch
  reads zeros; glibc `heap_trim`/`malloc_trim` and V8 page release do the same.
- RustOS: `handle_madvise` returns 0 after checking the advice number
  (`services/syscalld/src/linux_policy.rs:686-740`). Stale data survives, memory is never
  returned, and `MADV_WIPEONFORK` (PartitionAlloc/`base::RandBytes` reseed and zygote hardening
  use it, unverified) silently does nothing - a fake success path.
- Linux: DONTNEED on private anon drops pages (zero-fill on touch); on file-private reverts to
  file contents; shared mappings keep backing. FREE is lazy but must not corrupt.
  WIPEONFORK child sees zeros; DONTFORK child lacks the range.
- Sketch: add `MM_BROKER_OP_ZAP_RANGE` (unmap frames, keep VMA) in
  `mm_broker_ops.rs`, call it from `handle_madvise` for DONTNEED/FREE/DONTNEED_LOCKED on the
  VMA-checked subrange; return ENOMEM for unmapped holes, EINVAL for locked/huge misuse; store
  per-VMA `wipe_on_fork`/`dont_fork` bits in `vma_policy.rs` and apply them in the fork
  broker; return EINVAL (not 0) for advice with no implementation (HUGEPAGE/MERGEABLE may
  stay 0 - they are hints).

### M5. Per-resource `prlimit64`
- Chrome: `base::IncreaseFdLimitTo` (RLIMIT_NOFILE), `ResourceLimits` in the zygote
  (RLIMIT_AS/NPROC/CORE), `LaunchProcess` `CloseSuperfluousFds`, glibc `sysconf(_SC_OPEN_MAX)`.
- RustOS: any resource except RLIMIT_STACK returns EINVAL (`linux_policy.rs:254-257`).
- Sketch: table of (cur,max) per resource in the existing `policy_db`, defaults NOFILE
  1024/4096 (match what vfsd actually enforces), AS/DATA/CORE RLIM_INFINITY, NPROC =
  process limit; enforce NOFILE in the fd allocator and AS in `handle_mmap`.

### M6. CPU-time clocks and `getrusage`
- Chrome: `base::ThreadTicks::Now()` = `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` (task scheduler,
  tracing, `ProcessMetrics::GetCumulativeCPUUsage` also reads `CLOCK_PROCESS_CPUTIME_ID` /
  `getrusage`); a failed call is `NOTREACHED` (fatal in recent Chrome, unverified). glibc
  `pthread_getcpuclockid` yields the dynamic clock ids `(tid<<3)|6`.
- RustOS: `CLOCK_THREAD_CPUTIME_ID` (3) is served from per-thread CPU time the scheduler
  charges at every interval settlement (`runqueue/execution_time.rs`), readable by
  `clock_gettime`/`clock_getres` only (never a sleep clock, as on Linux). Clock ids
  0,1,4,5,6,7 are as before; 2 (process CPU time) and dynamic ids are still EINVAL; `getrusage`, `times`, `clock_getres` are `not dispatched`.
- Sketch: scheduler already tracks per-task runtime ticks; export per-thread and per-process
  CPU nanoseconds from `kernel/ps`, serve ids 2/3 and `~tid` ids in
  `write_clock_timespec`; add `SYS_GETRUSAGE` (RUSAGE_SELF/THREAD/CHILDREN: utime/stime from
  the same counters, maxrss from the job-group frame charge, zeros elsewhere is honest only
  if documented) and `SYS_CLOCK_GETRES`. wait4's rusage pointer is validated and then ignored.

### M7. Task-count and waiter-table ceilings
- Chrome (software mode) runs about 40-70 threads in the browser, 15-25 per renderer/utility/
  network/GPU process: a single window easily needs 150-250 live threads across 6-8 processes.
- RustOS: `MAX_TASK = 256` for the whole machine (`kernel/ps/src/multitask/scheduler.rs:188`),
  128 threads/process, 64 processes, ~2 MiB kernel stack per thread, memory-bound at ~100-120
  threads on a 2048M VM (2026-11-14 measurement). The futex waiter table is a global
  256-slot array whose overflow returns `EBUSY` (`futex_thread.rs:25,58,787-794`), an errno
  Linux futex never returns; glibc treats it as fatal in pthread_cond/mutex slow paths.
- Sketch: measure real kernel stack depth first (handoff says to) and shrink; raise MAX_TASK
  together with the kernel-stack allocation; make the futex table a hashed bucket array sized
  from MAX_TASK with per-bucket locks (keep the compare+publish linearization point at
  `futex_thread.rs:420-440`) and return ENOMEM on exhaustion. Add a kvm-smoke case that spawns
  200 blocked threads.

### M8. User address space is one PML4 slot (512 GiB)
- Chrome: PartitionAlloc reserves its address pools up front (regular/BRP ~16 GiB each plus a
  4 GiB configurable pool), V8 reserves the 4 GiB pointer-compression cage and a 1 TiB sandbox
  (falls back to smaller "partially reserved" sandboxes, unverified), all as PROT_NONE
  `MAP_NORESERVE` mappings, and requires them to cost nothing until committed.
- RustOS: `USER_SPACE_BASE..END_EXCLUSIVE` is one PML4 index = 512 GiB
  (`kernel/mm/src/memory/address_space.rs:62-63`); `handle_mmap` charges/accounts from
  `mmap_next` with `brk_limit = user_range_end - 2 MiB` (`linux_policy.rs:~850,~780`) and I
  did not find a no-charge PROT_NONE reservation path. A 1 TiB reservation is ENOMEM; several
  hundred GiB of PROT_NONE may also exhaust VMA/frame accounting.
- Sketch: either widen user space to multiple PML4 slots (architectural, owner mm + proc broker
  ABI, `PROC_BROKER_USER_SPACE_*`) or prove with a probe that Chrome's real reservation
  sequence succeeds in 512 GiB and that PROT_NONE reservations are uncharged. Do this probe
  before building more: it decides whether the renderer can start at all.

### M9. `mmap`/`mprotect` W^X rule and shared-exec corner cases
- `validate_prot` rejects any `PROT_WRITE|PROT_EXEC` (`linux_policy.rs:1403-1407`). V8 and
  SwiftShader Reactor (software Vulkan/ANGLE in the GPU process) normally flip RW->RX, but the
  Reactor and some V8 configurations use double-mapped memfd (RW view + RX view of the same
  memfd, MAP_SHARED). Verify that a shared memfd mapping with PROT_EXEC is allowed and that
  mprotect RW->RX->RW on the same range is atomic w.r.t. the other thread's fetches. If
  RWX is requested, Linux allows it; returning EINVAL (not EACCES/ENOMEM) breaks runtimes that
  feature-probe. Owner: `linux_policy.rs`, `mm_broker_ops.rs`. Probe before deciding policy.

### M10. Remaining startup syscalls that glibc/Chrome call unconditionally and are `not dispatched`
- `sysinfo` (glibc `sysconf(_SC_PHYS_PAGES/_AVPHYS_PAGES)`; `base::SysInfo::AmountOfPhysicalMemory`
  is `NOTREACHED` on failure and low-end-device detection then misbehaves),
  `getcpu`/`sched_getcpu` (rseq is ENOSYS and there is no vDSO, so glibc falls to the syscall),
  `gettimeofday`, `time` (glibc without vDSO issues the syscall), `getrusage`, `times`,
  `getpriority`/`setpriority`, `sched_getparam/setparam/getscheduler/setscheduler/
  get_priority_max/min`, `getgroups`, `getresuid/getresgid`, `capget`, `readv/pread`-family
  belong to the VFS audit. Chrome tolerates setpriority/sched_set* failing (thread types) as long
  as the errno is EPERM/EACCES, but not ENOSYS-as-crash in `NOTREACHED` paths.
- Sketch: add one `process_info_ops.rs` with `sysinfo`, `getcpu`, `gettimeofday`, `time`,
  `times`, `getpriority/setpriority` (store a nice value in `LinuxThreadState`, real
  scheduler mapping optional, EACCES for lowering nice), `sched_*` (SCHED_OTHER only; EPERM for
  FIFO/RR), `getgroups`, `getres[ug]id`. Register in `dispatch_ext.rs`.

## 3. SHOULD-FIX (Chrome runs, degraded or less safe)

| Item | Chrome use | RustOS state | Sketch |
|---|---|---|---|
| `mremap` | glibc realloc of mmap'd chunks, PartitionAlloc direct-map realloc; both fall back to alloc+copy | `not dispatched` | `mm_broker` op: grow in place if next VMA free, else MAYMOVE re-map frames; honour MREMAP_FIXED/DONTUNMAP -> EINVAL |
| `kill(0)`, `kill(-pgid)`, `kill(-1)` | `base::KillProcessGroup`, zygote cleanup | ENOSYS, and `-1` is mapped to self (`futex_thread.rs:1220-1232`) | procd resolves pgid membership (`getpgid/setpgid` exist); `-1` = all permitted except init and self |
| `waitpid(0)`, `__WALL/__WCLONE/__WNOTHREAD/WEXITED` | `WaitForExit`, zygote reaper | EINVAL for pid==0, pid<-1, unknown options (`procd/src/main.rs:670-674`); `waitid` not dispatched | add pgid waits in `multitask::wait_for_child`, accept __WALL; add `waitid` (P_PID/P_ALL/P_PGID, WNOWAIT) |
| `futex` PI + WAKE_OP + `futex_waitv` | PI mutexes for audio/RT threads, glibc `sem_post`/old condvars | ENOSYS (`futex_thread.rs:139-165`) | `LOCK_PI/UNLOCK_PI/TRYLOCK_PI` with owner-tid word and priority boost via scheduler hint; `WAKE_OP` is a small atomic-op wrapper over existing wake |
| `rt_sigtimedwait`, `rt_sigsuspend`, `rt_sigpending`, `rt_sigqueueinfo`, `rt_tgsigqueueinfo`, `signalfd4` | `sigwait` in helper threads, `SIGCHLD` handling in zygote/crashpad, `base::Process` | `not dispatched` | procd already owns per-thread pending sets: expose pending/wait/queue ops; signalfd as a kernel-local fd next to `eventfd` (`kernel/ps/src/user/`) |
| `SA_RESTART`, `SA_NODEFER`, `SA_RESETHAND`, `SA_NOCLDSTOP` | wide, mostly benign in Chrome since it loops on EINTR | ignored (`rg SA_RESTART kernel` empty) | apply in `install_rt_signal_frame` (`support.rs:332`) and syscall-restart logic |
| `membarrier` | not required by Chrome 157 itself; glibc/jemalloc-style allocators optionally | `not dispatched` -> ENOSYS is a legal answer | QUERY returns 0 mask; keep ENOSYS until an SMP witness needs it |
| `mlock/mlock2/munlock/mlockall`, `mincore`, `msync` | crypto key pages, `base::SharedMemory` | `not dispatched` | mlock family may return EPERM (RLIMIT_MEMLOCK=0) honestly; `msync` MS_SYNC on file-shared maps needs vfsd writeback; `mincore` from VMA+frame presence |
| `pidfd_open/pidfd_send_signal/pidfd_getfd`, `CLONE_PIDFD` | newer `base::Process` waiters, systemd-style helpers | not dispatched / clone3 ENOSYS | after M1: a kernel-local pidfd tied to procd's process record, readable on exit (poll/epoll) |
| `timerfd_*`, `timer_create`, `setitimer/getitimer`, `inotify` | libwayland-less paths, `FilePathWatcher`, X11 fallback timers | `not dispatched` | timerfd as kernel-local fd over scheduler deadlines; inotify belongs to vfsd audit |
| `PR_SET_PDEATHSIG`, `PR_SET_VMA`, `PR_SET_TIMERSLACK`, `PR_CAPBSET_READ/DROP`, `PR_SET_CHILD_SUBREAPER`, `PR_GET_TID_ADDRESS` | PartitionAlloc names anon VMAs (ignores failure), zygote uses PDEATHSIG, sandbox reads/drops capbset | all EINVAL (`prctl_ops.rs:215-228`) | PDEATHSIG: store in `LinuxThreadState`, procd raises it in the exit path; PR_SET_VMA: name field in `vma_policy.rs`, shown in `/proc/self/maps`; capbset pairs with M1 |
| `arch_prctl(ARCH_SET_GS/GET_GS)` | not used by glibc/Chrome | EINVAL (`futex_thread.rs:1210-1217`) | none needed; keep EINVAL |
| `CLOCK_PROCESS/THREAD_CPUTIME_ID` in `clock_nanosleep`, `clock_getres` | rare | see M6 | with M6 |
| SIGBUS on truncated file mapping, `MAP_GROWSDOWN` | rare | n/a | skip |
| `sched_setaffinity` effect on `sched_getaffinity` count | `base::SysInfo::NumberOfProcessors` uses `sysconf` = affinity mask | present | verify mask reflects online vCPUs |
| `execveat(AT_EMPTY_PATH)`, `execve` with `CLONE_VM` after vfork | `fexecve` by glibc `posix_spawn`, zygote re-exec via `/proc/self/exe` | `unsupported_execveat_flags(flags != 0)` (`procd/src/main.rs:577`) | accept `AT_EMPTY_PATH` with an fd; vfork below |
| `vfork` and `clone(CLONE_VM\|CLONE_VFORK\|SIGCHLD)` | glibc `posix_spawn`/`system`/`popen` (xdg-open, helper processes), `base::LaunchProcess` when `options.clone_flags` set | `SYS_VFORK` not dispatched; clone path EINVAL (`procd/src/main.rs:652`) | implement vfork as fork-with-wait (COW fork already exists) completing when child execs/exits - semantically a superset and documented as such; true shared-VM vfork can follow |

## 4. OUT-OF-SCOPE or intentionally deferred

- GPU acceleration, libgbm, DRM, dmabuf, vsync via KMS: stated out of scope
  (see [Session Handoff](session-handoff.md)). `--ozone-platform=wayland` software path uses wl_shm
  and memfd, both present.
- Crashpad/breakpad handler: Chrome continues without it; needs `ptrace`, `PR_SET_PTRACER`,
  `SO_PASSCRED`. Recommend launching with `--disable-breakpad` in the package
  (`apps/chrome/RUSTOS.package.toml:21`) rather than implementing ptrace; ask the user.
- `perf_event_open`, `bpf`, `userfaultfd`, `io_uring`, `process_vm_readv`, `kcmp`, `ptrace`:
  Chrome probes and tolerates ENOSYS/EPERM.
- >4 vCPU scaling, NUMA calls (`get_mempolicy`, `mbind`): Chrome tolerates ENOSYS.
- `futex_waitv`, `rseq` fast path, `membarrier` as listed: optional accelerators.

## 5. Suggested order of work

1. Probe, do not build: M8 (does Chrome's reservation sequence fit?) and M7 (thread
   count) decide whether a renderer can exist at all; each is a small guest probe.
2. Cheap honesty fixes in one batch: M4 madvise, M5 prlimit, M6 clocks+getrusage, M10
   syscall set, kill/wait extensions, futex EBUSY->ENOMEM, `LinuxSigInfo` struct.
3. Signals from faults and interrupt-return delivery (M3), then seccomp (M2).
4. Namespaces (M1): largest item; do it after M2/M3 because the sandbox needs all three.
5. Re-run the real Chrome and record the next blocker in `docs/ai/session-handoff.md`.

## 6. Decision needed from the user

Without `--no-sandbox` Chrome will not start unless M1+M2 (and M3 for SIGSYS) exist, which
is the largest cross-subsystem change in this audit. Recommendation: implement them
(policy in `procd`/`syscalld`, no ring0 policy); do not fake `unshare`/`seccomp` success.
The user chose `--no-sandbox` as an interim launch argument on 2026-11-21 (checked-in
manifest template); the real sandbox (M1+M2+M3) remains open.

## 7. Not verified

No guest run, build or test was executed. Chrome-side behavior (marked unverified or
described from memory) must be confirmed with strace-equivalent logging from
`error_ops::unimplemented_linux_syscall` (it already records the first ENOSYS per number)
on the first real launch.
