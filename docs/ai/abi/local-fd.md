# Kernel-local pipe and eventfd (`pipe`/`pipe2`/`eventfd`/`eventfd2`)

Linux syscalls 22, 293, 284, 290. Modules: `kernel/ps/src/user/{pipe,eventfd,local_wait}.rs`
(objects), `kernel/compat/src/user/syscall/linux/service_ops/local_fd.rs` (syscalls, blocking, user copy),
ABI constants in `libs/rustos-user-abi/src/linux/local_fd.rs`.

## Owner decision

Kernel-local objects (like memfd), not a Ring3 service and not the
`vfsd` stream machinery. Evidence: the object is anonymous, has no name,
permission or path policy, so no policy lives in ring0; the data path is a
bounded buffer plus a counter. A Ring3 service would add an IPC round trip to
every read/write/wake (Chrome's message-loop wake is an eventfd write), and the
descriptor lifecycle (fork/exec/dup/close/exit) already has a generic
acquire/release mechanism (`ServiceHandleRef`) that these kinds plug into.
This deviates from the A1 note "pipe/eventfd are a shared-memory ring + waitset
in a ring-3 service"; revisit only if pipes need names (FIFOs) or policy.

## Semantics

- `pipe2` flags: `O_CLOEXEC`, `O_NONBLOCK`; `O_DIRECT` and unknown bits are
  `EINVAL`. Both fds are installed in one transaction; a copy-out fault rolls back.
- Capacity 64 KiB (`F_GETPIPE_SZ`). `F_SETPIPE_SZ` rounds up to a power of two
  pages, at most 64 KiB (above: `EPERM`), `EBUSY` if it would drop below the
  buffered bytes. Writes <= `PIPE_BUF` (4096) are atomic; larger writes may be partial.
- Read: data first, then EOF (0) once all writers closed; empty with writers is
  `EAGAIN`/block. Write: `EPIPE` + `SIGPIPE` (queued via `queue_linux_signal`,
  disposition decided at syscall return) when no reader; zero-length write never signals.
- Wrong-end read/write `EBADF`; `lseek`/`pread` `ESPIPE`; `fstat` `S_IFIFO`;
  `FIONREAD`, `FIONBIO`; other ioctls `ENOTTY`.
- `O_NONBLOCK` is stored on the open description (shared by dup/fork), per Linux.
- eventfd: 64-bit counter, `EFD_SEMAPHORE/CLOEXEC/NONBLOCK`; read needs >= 8
  bytes (`EINVAL`); write of `u64::MAX` is `EINVAL`; counter max `u64::MAX-1`,
  overflow blocks/`EAGAIN`. fstat reports an anon inode (not FIFO).
- Readiness: pipe read end `EPOLLIN` (data), `EPOLLHUP` (no writer); write end
  `EPOLLOUT` (free >= `PIPE_BUF`), `EPOLLERR` (no reader). eventfd `EPOLLIN` if
  counter > 0, `EPOLLOUT` if below max.

## Mechanism

- Handle kinds `KernelHandle::{Pipe, EventFd}` (`HANDLE_KIND_PIPE=12`,
  `HANDLE_KIND_EVENTFD=13`). Non-transferable: no `SCM_RIGHTS`.
- Descriptor references are explicit (`try_acquire`/`release`); the last release
  closes the end, wakes peers and unregisters from the `local_wait` registry
  (4096 slots, `LockClass::LocalFdObject`, leaf lock; raw sections are bounded,
  allocation-free, no wake or user copy inside).
- Waits use the waitset with `WAITSET_PROVIDER_KERNEL = 5`, keyed by object token and
  generation; epoll/poll read readiness straight from the registry. `wake_local_fd_object`
  skips the waiter slab when no KERNEL observer exists.
- Memory: pipe pages are charged as resident pages to the creating Job's charge
  group (`kernel_mm::api::charge`), outside the guard; the buffer is freed when the
  reader closes. eventfd allocates nothing beyond the object. Fd count is bounded by
  the per-process fd cap; a global object cap comes from the registry size (`EMFILE`/`ENFILE`).

## Read atomicity

`pipe` reads are copy-then-commit (`PipeShared::read_with`): bytes are peeked,
copied to the user buffer with no lock held, and consumed only if the copy
succeeds. A concurrent reader that consumed during the copy invalidates the
peek and the read restarts, so a fault never loses data and data is never
delivered twice.

## Known gaps

- (resolved) pipe reads are copy-then-commit: a copy-out fault consumes nothing.
- (resolved) eventfd keeps separate IN/OUT edge generations.
- Multi-iovec `writev` is not atomic. `readv` (19) is unsupported (`ENOSYS`) for every fd kind: a correct one must deliver the first iovec with blocking semantics and continue only with immediately available data, which the existing vectored helper (`writev_via_write`, a plain per-chunk loop) cannot express.
- No `splice`/`tee`, packet mode (`O_DIRECT`), named FIFOs, or fd passing.
- Wake path scans the generic waiter slab while any KERNEL waiter exists.
- Verified by host tests only; the in-guest checker `apps/localfdprobe` (`cargo xtask kvm-smoke --local-fd-probe`, a session program armed by a private contract) has never been run.
