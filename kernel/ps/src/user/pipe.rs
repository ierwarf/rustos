//! Kernel-local anonymous pipe: bounded byte buffer, two open descriptions.
//!
//! - **Owner:** `syscalld` is not involved (an anonymous pipe has no name,
//!   path, credential or namespace to decide); ring0 owns the exact buffer,
//!   end state and descriptor lifetime, compat owns blocking, user copy and
//!   wakeups. Resource policy is the creating Job's resident-page charge.
//! - **Boundary:** Sizes and flags are untrusted until checked by compat; this
//!   module only sees kernel byte slices.
//! - **Lifecycle:** `PipeHandle::new_pair` creates one shared buffer and the
//!   read and write open descriptions (one descriptor reference each). Dup,
//!   fork and `fcntl(F_DUPFD)` retain the *description* through compat's
//!   `ServiceHandleRef`; the final release of an end closes that end, which is
//!   what makes the peer see EOF (`read` returns 0) or `EPIPE`.
//! - **Concurrency:** One `LocalFdObject` leaf guards the buffer. It is
//!   allocation-free: pages are charged and allocated before the guard is
//!   taken and released after it is dropped. Every state change returns the
//!   `LocalWake` the caller must publish after unlocking; no wake, user copy
//!   or second lock happens inside the guard.
//! - **Failure:** Allocation or charge refusal leaves buffer and charge intact
//!   (`NoMemory`); a write with no reader is `BrokenPipe`; a full or empty
//!   pipe reports `WouldBlock` with the generation observed under the guard so
//!   a waiter registered at that generation cannot lose a wake.
//! - **Forbidden:** Packet mode (`O_DIRECT`), splice/tee, unbounded buffering,
//!   per-byte locking, and charging pages that were not allocated.
//! - **Accounting:** each buffer page (4 KiB) is one page of the creating
//!   Job's resident-page charge group (`charge_group`, fixed at creation; none
//!   means unaccounted), reserved before the page is allocated and refunded
//!   exactly once when the page is freed. Pages exist only while data does; an
//!   empty pipe holds none. Capacity is 64 KiB by default and at most 64 KiB.
//! - **Evidence:** `vfs-open-description`, `memory-charge-conservation`,
//!   `userspace-wait-set`.
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
#[cfg(test)]
use core::sync::atomic::{AtomicU64, Ordering};

use kernel_mm::api::charge;
use nucleus_core::util::lockdep::{LockClass, TrackedSpinLock};

use crate::user::linux as linux_abi;
use crate::user::local_wait::{
    self, LocalDescription, LocalReadiness, LocalRelease, LocalWaitable, LocalWake,
};

/// Writes of at most this many bytes are atomic: all or nothing, never
/// interleaved with another writer's bytes.
pub const PIPE_BUF: usize = linux_abi::PIPE_BUF;
pub const PIPE_PAGE_SIZE: usize = 4096;
/// Default and maximum capacity. `F_SETPIPE_SZ` may shrink it, never grow it.
pub const PIPE_MAX_CAPACITY: usize = 64 * 1024;
const _: () = assert!(PIPE_BUF == PIPE_PAGE_SIZE);
/// A buffer of `PIPE_MAX_CAPACITY` bytes spans one extra page when its head
/// page has been partly consumed.
const MAX_PAGES: usize = PIPE_MAX_CAPACITY / PIPE_PAGE_SIZE + 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipeRole {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipeError {
    /// The operation cannot proceed now. A waiter registered at `generation`
    /// is woken by the next published generation for its end's token.
    WouldBlock { generation: u64 },
    /// Write with no reader (`EPIPE`).
    BrokenPipe,
    /// Page charge or allocation refused.
    NoMemory,
    /// The registry or token space is exhausted.
    TooManyObjects,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipeCapacityError {
    /// Larger than `PIPE_MAX_CAPACITY` (`EPERM`, as above `pipe-max-size`).
    TooLarge,
    /// More data is buffered than the requested capacity holds (`EBUSY`).
    Busy,
}

/// Bytes moved and the wake they published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PipeProgress {
    pub bytes: usize,
    pub wake: Option<LocalWake>,
}

/// One 4 KiB buffer page. Dropping it refunds its charge exactly once.
#[derive(Debug)]
struct PipePage {
    data: Vec<u8>,
    start: usize,
    end: usize,
    group: Option<usize>,
}

impl PipePage {
    fn new(group: Option<usize>) -> Result<Self, PipeError> {
        if group.is_some_and(|group| !charge::try_charge(group, 1)) {
            return Err(PipeError::NoMemory);
        }
        let mut data = Vec::new();
        if data.try_reserve_exact(PIPE_PAGE_SIZE).is_err() {
            if let Some(group) = group {
                charge::uncharge(group, 1);
            }
            return Err(PipeError::NoMemory);
        }
        data.resize(PIPE_PAGE_SIZE, 0);
        Ok(Self {
            data,
            start: 0,
            end: 0,
            group,
        })
    }
}

impl Drop for PipePage {
    fn drop(&mut self) {
        if let Some(group) = self.group {
            charge::uncharge(group, 1);
        }
    }
}

/// Pages released under the guard, dropped (refunded and freed) after it.
struct Graveyard {
    pages: [Option<PipePage>; MAX_PAGES],
    len: usize,
}

impl Graveyard {
    fn new() -> Self {
        Self {
            pages: [const { None }; MAX_PAGES],
            len: 0,
        }
    }

    fn bury(&mut self, page: PipePage) {
        self.pages[self.len] = Some(page);
        self.len += 1;
    }
}

struct PipeState {
    pages: VecDeque<PipePage>,
    len: usize,
    capacity: usize,
    reader_open: bool,
    writer_open: bool,
    /// Total bytes ever consumed; identifies the buffer head for
    /// copy-then-commit reads.
    read_total: u64,
    /// Advances when data arrives or the last writer closes.
    read_generation: u64,
    /// Advances when space appears, capacity grows or the last reader closes.
    write_generation: u64,
}

impl PipeState {
    fn tail_room(&self) -> usize {
        self.pages
            .back()
            .map_or(0, |page| PIPE_PAGE_SIZE - page.end)
    }

    /// Fresh pages an `n`-byte append needs beyond the tail page's room.
    fn pages_needed(&self, n: usize) -> usize {
        n.saturating_sub(self.tail_room()).div_ceil(PIPE_PAGE_SIZE)
    }

    /// Appends `src`, taking any needed pages from `staged`.
    fn append(&mut self, src: &[u8], staged: &mut Vec<PipePage>) {
        debug_assert!(staged.len() >= self.pages_needed(src.len()));
        let mut copied = 0;
        while copied < src.len() {
            if self.tail_room() == 0 {
                let page = staged.pop().expect("pipe append: staged pages were preflighted");
                debug_assert!(self.pages.len() < MAX_PAGES);
                self.pages.push_back(page);
            }
            let page = self.pages.back_mut().expect("tail page exists");
            let take = (src.len() - copied).min(PIPE_PAGE_SIZE - page.end);
            page.data[page.end..page.end + take].copy_from_slice(&src[copied..copied + take]);
            page.end += take;
            copied += take;
        }
        self.len += src.len();
    }

    /// Drops up to `max_bytes` already-peeked buffered bytes, burying drained pages.
    fn discard(&mut self, max_bytes: usize, grave: &mut Graveyard) -> usize {
        let mut discarded = 0;
        while discarded < max_bytes {
            let Some(front) = self.pages.front_mut() else {
                break;
            };
            let take = (front.end - front.start).min(max_bytes - discarded);
            front.start += take;
            discarded += take;
            if front.start == front.end {
                let page = self.pages.pop_front().expect("front page exists");
                grave.bury(page);
            }
        }
        self.len -= discarded;
        self.read_total = self.read_total.wrapping_add(discarded as u64);
        discarded
    }

    /// Moves up to `dest.len()` buffered bytes out, burying drained pages.
    fn consume(&mut self, dest: &mut [u8], grave: &mut Graveyard) -> usize {
        let mut copied = 0;
        while copied < dest.len() {
            let Some(front) = self.pages.front_mut() else {
                break;
            };
            let take = (front.end - front.start).min(dest.len() - copied);
            dest[copied..copied + take]
                .copy_from_slice(&front.data[front.start..front.start + take]);
            front.start += take;
            copied += take;
            if front.start == front.end {
                let page = self.pages.pop_front().expect("front page exists");
                grave.bury(page);
            }
        }
        self.len -= copied;
        self.read_total = self.read_total.wrapping_add(copied as u64);
        copied
    }

    /// Copies up to `dest.len()` buffered bytes without consuming them.
    fn peek(&self, dest: &mut [u8]) -> usize {
        let mut copied = 0;
        for page in &self.pages {
            if copied == dest.len() {
                break;
            }
            let take = (page.end - page.start).min(dest.len() - copied);
            dest[copied..copied + take]
                .copy_from_slice(&page.data[page.start..page.start + take]);
            copied += take;
        }
        copied
    }
}

/// Failure of [`PipeShared::read_with`]: the pipe's own error, or the
/// caller's copy-out error (in which case nothing was consumed).
#[derive(Debug, Eq, PartialEq)]
pub enum PipeReadError<E> {
    Pipe(PipeError),
    CopyOut(E),
}

/// State shared by the two ends of one pipe.
pub struct PipeShared {
    charge_group: Option<usize>,
    read_token: u64,
    write_token: u64,
    state: TrackedSpinLock<PipeState, { LockClass::LocalFdObject as u8 }>,
}

impl Drop for PipeShared {
    fn drop(&mut self) {
        // Free all PipePages (and refund their page charges) before releasing
        // the job charge-group slot hold.
        self.state.lock().pages.clear();
        if let Some(group) = self.charge_group {
            charge::release(group);
        }
    }
}

impl PipeShared {
    fn wake(&self, role: PipeRole, generation: u64) -> LocalWake {
        LocalWake {
            token: match role {
                PipeRole::Read => self.read_token,
                PipeRole::Write => self.write_token,
            },
            generation,
        }
    }

    /// Writes as much of `src` as the pipe's rules allow. A write of at most
    /// `PIPE_BUF` bytes is atomic: it takes all of `src` or reports
    /// `WouldBlock`. A larger one takes whatever fits (at least one byte) or
    /// reports `WouldBlock`. `src` must be non-empty (callers short-circuit
    /// zero-length writes, which Linux completes without touching the pipe).
    pub fn try_write(&self, src: &[u8]) -> Result<PipeProgress, PipeError> {
        debug_assert!(!src.is_empty());
        let mut staged: Vec<PipePage> = Vec::new();
        loop {
            let mut state = self.state.lock();
            if !state.reader_open {
                return Err(PipeError::BrokenPipe);
            }
            let free = state.capacity - state.len;
            let want = if src.len() <= PIPE_BUF {
                if free < src.len() {
                    return Err(PipeError::WouldBlock {
                        generation: state.write_generation,
                    });
                }
                src.len()
            } else {
                if free == 0 {
                    return Err(PipeError::WouldBlock {
                        generation: state.write_generation,
                    });
                }
                src.len().min(free)
            };
            let need = state.pages_needed(want);
            if need <= staged.len() {
                state.append(&src[..want], &mut staged);
                state.read_generation += 1;
                let wake = self.wake(PipeRole::Read, state.read_generation);
                drop(state);
                // Unused staged pages are refunded here, outside the guard.
                drop(staged);
                return Ok(PipeProgress {
                    bytes: want,
                    wake: Some(wake),
                });
            }
            drop(state);
            let missing = need - staged.len();
            staged
                .try_reserve(missing)
                .map_err(|_| PipeError::NoMemory)?;
            for _ in 0..missing {
                staged.push(PipePage::new(self.charge_group)?);
            }
        }
    }

    /// Reads up to `dest.len()` bytes. An empty pipe with no writer is EOF
    /// (`Ok` with 0 bytes); with a writer it is `WouldBlock`.
    pub fn try_read(&self, dest: &mut [u8]) -> Result<PipeProgress, PipeError> {
        let mut grave = Graveyard::new();
        let mut state = self.state.lock();
        if dest.is_empty() {
            return Ok(PipeProgress { bytes: 0, wake: None });
        }
        if state.len == 0 {
            if !state.writer_open {
                return Ok(PipeProgress { bytes: 0, wake: None });
            }
            return Err(PipeError::WouldBlock {
                generation: state.read_generation,
            });
        }
        let copied = state.consume(dest, &mut grave);
        let wake = (copied != 0).then(|| {
            state.write_generation += 1;
            self.wake(PipeRole::Write, state.write_generation)
        });
        drop(state);
        drop(grave);
        Ok(PipeProgress {
            bytes: copied,
            wake,
        })
    }

    /// Copy-then-commit read. Bytes are peeked, handed to `copy_out` (the
    /// user copy, run with no lock held) and consumed only if it succeeds, so
    /// a copy fault leaves every byte in the pipe. If another reader consumed
    /// bytes while `copy_out` ran, the peek is stale and the read restarts
    /// (the caller's buffer is rewritten before the call returns).
    pub fn read_with<E>(
        &self,
        scratch: &mut [u8],
        mut copy_out: impl FnMut(&[u8]) -> Result<(), E>,
    ) -> Result<PipeProgress, PipeReadError<E>> {
        if scratch.is_empty() {
            return Ok(PipeProgress { bytes: 0, wake: None });
        }
        loop {
            let (peeked, epoch) = {
                let state = self.state.lock();
                if state.len == 0 {
                    if !state.writer_open {
                        return Ok(PipeProgress { bytes: 0, wake: None });
                    }
                    return Err(PipeReadError::Pipe(PipeError::WouldBlock {
                        generation: state.read_generation,
                    }));
                }
                (state.peek(scratch), state.read_total)
            };
            copy_out(&scratch[..peeked]).map_err(PipeReadError::CopyOut)?;
            let mut grave = Graveyard::new();
            let mut state = self.state.lock();
            if state.read_total != epoch {
                continue;
            }
            // Only readers remove bytes, so an unchanged `read_total` means the
            // peeked bytes are still the head of the buffer.
            let taken = state.discard(peeked, &mut grave);
            debug_assert_eq!(taken, peeked);
            state.write_generation += 1;
            let wake = self.wake(PipeRole::Write, state.write_generation);
            drop(state);
            drop(grave);
            return Ok(PipeProgress {
                bytes: taken,
                wake: Some(wake),
            });
        }
    }

    /// Bytes buffered right now (`FIONREAD`).
    pub fn bytes_available(&self) -> usize {
        self.state.lock().len
    }

    pub fn capacity(&self) -> usize {
        self.state.lock().capacity
    }

    /// `F_SETPIPE_SZ`: rounds up to a power-of-two page count like Linux.
    pub fn set_capacity(&self, requested: usize) -> Result<(usize, Option<LocalWake>), PipeCapacityError> {
        let pages = requested
            .div_ceil(PIPE_PAGE_SIZE)
            .max(1)
            .checked_next_power_of_two()
            .ok_or(PipeCapacityError::TooLarge)?;
        let bytes = pages
            .checked_mul(PIPE_PAGE_SIZE)
            .filter(|bytes| *bytes <= PIPE_MAX_CAPACITY)
            .ok_or(PipeCapacityError::TooLarge)?;
        let mut state = self.state.lock();
        if bytes < state.len {
            return Err(PipeCapacityError::Busy);
        }
        let grew = bytes > state.capacity;
        state.capacity = bytes;
        let wake = grew.then(|| {
            state.write_generation += 1;
            self.wake(PipeRole::Write, state.write_generation)
        });
        Ok((bytes, wake))
    }

    fn readiness(&self, role: PipeRole) -> LocalReadiness {
        let state = self.state.lock();
        let mut events = 0;
        let generation = match role {
            PipeRole::Read => {
                if state.len > 0 {
                    events |= linux_abi::EPOLLIN;
                }
                if !state.writer_open {
                    events |= linux_abi::EPOLLHUP;
                }
                state.read_generation
            }
            PipeRole::Write => {
                // Writable means a `PIPE_BUF` write cannot block.
                if state.capacity - state.len >= PIPE_BUF {
                    events |= linux_abi::EPOLLOUT;
                }
                if !state.reader_open {
                    events |= linux_abi::EPOLLERR;
                }
                state.write_generation
            }
        };
        LocalReadiness::uniform(events, generation)
    }

    /// Final close of one end: the peer's next operation observes it.
    fn close_end(&self, role: PipeRole) -> Option<LocalWake> {
        let mut grave = Graveyard::new();
        let mut state = self.state.lock();
        let wake = match role {
            PipeRole::Read => {
                state.reader_open = false;
                // Nobody can read the buffered bytes any more; free them now.
                while let Some(page) = state.pages.pop_front() {
                    grave.bury(page);
                }
                state.len = 0;
                state.write_generation += 1;
                self.wake(PipeRole::Write, state.write_generation)
            }
            PipeRole::Write => {
                state.writer_open = false;
                state.read_generation += 1;
                self.wake(PipeRole::Read, state.read_generation)
            }
        };
        drop(state);
        drop(grave);
        Some(wake)
    }
}

struct PipeEnd {
    shared: Arc<PipeShared>,
    role: PipeRole,
    description: LocalDescription,
}

impl Drop for PipeEnd {
    /// Safety net: the last strong reference vanished without a final
    /// descriptor release (a path that dropped a handle without settling its
    /// `ServiceHandleRef`). Close the end anyway so the peer still observes
    /// EOF/`EPIPE` and the registry slot is reclaimed. No wake can be
    /// published from drop glue; a peer already blocked is released by its
    /// next scan or signal, which is why every normal path releases first.
    fn drop(&mut self) {
        if !self.description.is_closed() {
            local_wait::unregister(self.description.token());
            let _ = self.shared.close_end(self.role);
        }
    }
}

impl LocalWaitable for PipeEnd {
    fn readiness(&self) -> Option<LocalReadiness> {
        (!self.description.is_closed()).then(|| self.shared.readiness(self.role))
    }
}

/// One end of a pipe as held by descriptor tables. Cloning is a transient
/// kernel snapshot; only [`PipeHandle::try_acquire_descriptor_reference`] and
/// [`PipeHandle::release_descriptor_reference`] change descriptor ownership.
#[derive(Clone)]
pub struct PipeHandle {
    end: Arc<PipeEnd>,
}

impl core::fmt::Debug for PipeHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PipeHandle")
            .field("token", &self.token_id())
            .field("role", &self.end.role)
            .finish()
    }
}

impl PipeHandle {
    /// Creates the `(read, write)` ends. Both start with one descriptor
    /// reference that the caller must publish (or release on failure).
    pub fn new_pair(
        charge_group: Option<usize>,
        nonblocking: bool,
    ) -> Result<(Self, Self), PipeError> {
        let read_token = local_wait::allocate_token().ok_or(PipeError::TooManyObjects)?;
        let write_token = local_wait::allocate_token().ok_or(PipeError::TooManyObjects)?;
        let mut pages = VecDeque::new();
        pages
            .try_reserve_exact(MAX_PAGES)
            .map_err(|_| PipeError::NoMemory)?;
        if let Some(group) = charge_group {
            charge::retain(group);
        }
        let shared = Arc::new(PipeShared {
            charge_group,
            read_token,
            write_token,
            state: TrackedSpinLock::new(PipeState {
                pages,
                len: 0,
                capacity: PIPE_MAX_CAPACITY,
                reader_open: true,
                writer_open: true,
                read_total: 0,
                read_generation: 1,
                write_generation: 1,
            }),
        });
        let read = Self {
            end: Arc::new(PipeEnd {
                shared: shared.clone(),
                role: PipeRole::Read,
                description: LocalDescription::new(read_token, nonblocking),
            }),
        };
        let write = Self {
            end: Arc::new(PipeEnd {
                shared,
                role: PipeRole::Write,
                description: LocalDescription::new(write_token, nonblocking),
            }),
        };
        let read_object: Arc<dyn LocalWaitable> = read.end.clone();
        if !local_wait::register(read_token, Arc::downgrade(&read_object)) {
            return Err(PipeError::TooManyObjects);
        }
        let write_object: Arc<dyn LocalWaitable> = write.end.clone();
        if !local_wait::register(write_token, Arc::downgrade(&write_object)) {
            local_wait::unregister(read_token);
            return Err(PipeError::TooManyObjects);
        }
        Ok((read, write))
    }

    pub fn role(&self) -> PipeRole {
        self.end.role
    }

    pub fn shared(&self) -> &PipeShared {
        &self.end.shared
    }

    pub fn token_id(&self) -> u64 {
        self.end.description.token()
    }

    pub fn path(&self) -> alloc::string::String {
        alloc::format!("pipe:[{}]", self.end.shared.read_token)
    }

    /// Inode number reported by `fstat`: stable per pipe, shared by both ends.
    pub fn inode(&self) -> u64 {
        self.end.shared.read_token
    }

    /// `O_RDONLY`/`O_WRONLY` plus the shared `O_NONBLOCK` bit.
    pub fn status_flags(&self) -> u64 {
        let access = match self.end.role {
            PipeRole::Read => linux_abi::O_RDONLY,
            PipeRole::Write => linux_abi::O_WRONLY,
        };
        access
            | if self.end.description.nonblocking() {
                linux_abi::O_NONBLOCK
            } else {
                0
            }
    }

    pub fn nonblocking(&self) -> bool {
        self.end.description.nonblocking()
    }

    pub fn set_nonblocking(&self, value: bool) {
        self.end.description.set_nonblocking(value);
    }

    pub fn mark_epoll_registered(&self) {
        self.end.description.mark_epoll_registered();
    }

    pub fn epoll_registered(&self) -> bool {
        self.end.description.epoll_registered()
    }

    pub fn try_acquire_descriptor_reference(&self) -> bool {
        self.end.description.try_acquire()
    }

    /// Drops one fd-table reference. On the last one the end closes, the
    /// registry forgets it, and the peer-visible wake is returned for the
    /// caller to publish after all locks are released.
    pub fn release_descriptor_reference(&self) -> LocalRelease {
        if !self.end.description.release() {
            return LocalRelease {
                last: false,
                wake: None,
            };
        }
        local_wait::unregister(self.end.description.token());
        LocalRelease {
            last: true,
            wake: self.end.shared.close_end(self.end.role),
        }
    }
}

impl PartialEq for PipeHandle {
    fn eq(&self, other: &Self) -> bool {
        self.token_id() == other.token_id()
    }
}

impl Eq for PipeHandle {}

/// Monotonic source for tests that need unique charge groups.
#[cfg(test)]
static TEST_GROUP: AtomicU64 = AtomicU64::new(54);

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (PipeHandle, PipeHandle) {
        PipeHandle::new_pair(None, false).unwrap()
    }

    fn read_all(pipe: &PipeHandle, max: usize) -> Vec<u8> {
        let mut out = alloc::vec![0_u8; max];
        let progress = pipe.shared().try_read(&mut out).unwrap();
        out.truncate(progress.bytes);
        out
    }

    fn group() -> usize {
        let group = TEST_GROUP.fetch_add(1, Ordering::Relaxed) as usize;
        assert!(group < charge_groups(), "test charge groups exhausted");
        charge::init_group(group, None);
        group
    }

    fn charge_groups() -> usize {
        kernel_mm::api::charge::CHARGE_GROUPS
    }

    #[test]
    fn copy_out_fault_leaves_every_byte_readable() {
        let (r, w) = pair();
        let data: Vec<u8> = (0..6000_u32).map(|n| (n % 241) as u8).collect();
        w.shared().try_write(&data[..3000]).unwrap();
        w.shared().try_write(&data[3000..]).unwrap();
        let mut scratch = [0u8; 5000];
        let result = r.shared().read_with(&mut scratch, |_| Err::<(), u8>(14));
        assert_eq!(result, Err(PipeReadError::CopyOut(14)));
        assert_eq!(r.shared().bytes_available(), 6000, "a faulting copy consumes nothing");
        let mut seen = Vec::new();
        let progress = r
            .shared()
            .read_with(&mut scratch, |bytes| -> Result<(), u8> {
                seen.extend_from_slice(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(progress.bytes, 5000);
        assert!(progress.wake.is_some());
        assert_eq!(seen, data[..5000]);
        assert_eq!(read_all(&r, 4096), data[5000..], "the remainder follows in order");
    }

    #[test]
    fn read_restarts_when_another_reader_consumed_during_the_copy() {
        let (r, w) = pair();
        w.shared().try_write(b"abcdefgh").unwrap();
        let mut scratch = [0u8; 4];
        let mut calls = 0;
        let mut last = Vec::new();
        let other = r.clone();
        let progress = r
            .shared()
            .read_with(&mut scratch, |bytes| -> Result<(), u8> {
                calls += 1;
                if calls == 1 {
                    let mut steal = [0u8; 4];
                    assert_eq!(other.shared().try_read(&mut steal).unwrap().bytes, 4);
                }
                last = bytes.to_vec();
                Ok(())
            })
            .unwrap();
        assert_eq!((calls, progress.bytes), (2, 4));
        assert_eq!(last, b"efgh", "the stale peek is replaced, never delivered twice");
        assert_eq!(r.shared().bytes_available(), 0);
    }

    #[test]
    fn data_round_trips_in_order_across_page_boundaries() {
        let (r, w) = pair();
        let data: Vec<u8> = (0..10_000_u32).map(|n| (n % 251) as u8).collect();
        let mut sent = 0;
        while sent < data.len() {
            let chunk = &data[sent..(sent + 3000).min(data.len())];
            sent += w.shared().try_write(chunk).unwrap().bytes;
        }
        assert_eq!(r.shared().bytes_available(), 10_000);
        let mut got = Vec::new();
        loop {
            let part = read_all(&r, 777);
            if part.is_empty() {
                break;
            }
            got.extend_from_slice(&part);
            if got.len() == data.len() {
                break;
            }
        }
        assert_eq!(got, data);
    }

    #[test]
    fn empty_pipe_blocks_until_every_writer_closes_then_reads_eof() {
        let (r, w) = pair();
        let mut buf = [0_u8; 8];
        let observed = match r.shared().try_read(&mut buf) {
            Err(PipeError::WouldBlock { generation }) => generation,
            other => panic!("empty pipe with a writer must block: {other:?}"),
        };
        let release = w.release_descriptor_reference();
        assert!(release.last);
        let wake = release.wake.expect("closing the writer wakes the reader");
        assert_eq!(wake.token, r.token_id());
        assert!(wake.generation > observed, "the waiter registered at {observed} is woken");
        assert_eq!(r.shared().try_read(&mut buf).unwrap().bytes, 0, "EOF");
        assert_eq!(r.shared().try_read(&mut buf).unwrap().bytes, 0, "EOF is sticky");
    }

    #[test]
    fn buffered_data_is_delivered_before_eof() {
        let (r, w) = pair();
        w.shared().try_write(b"tail").unwrap();
        assert!(w.release_descriptor_reference().last);
        assert_eq!(read_all(&r, 16), b"tail");
        assert_eq!(read_all(&r, 16), b"");
    }

    #[test]
    fn dup_keeps_the_end_open_until_the_last_descriptor_closes() {
        let (r, w) = pair();
        assert!(w.try_acquire_descriptor_reference());
        let first = w.release_descriptor_reference();
        assert!(!first.last && first.wake.is_none());
        let mut buf = [0_u8; 1];
        assert!(matches!(
            r.shared().try_read(&mut buf),
            Err(PipeError::WouldBlock { .. })
        ));
        assert!(w.release_descriptor_reference().last);
        assert_eq!(r.shared().try_read(&mut buf).unwrap().bytes, 0);
    }

    #[test]
    fn write_without_a_reader_is_a_broken_pipe_and_wakes_nobody_else() {
        let (r, w) = pair();
        let release = r.release_descriptor_reference();
        let wake = release.wake.expect("closing the reader wakes the writer");
        assert_eq!(wake.token, w.token_id());
        assert_eq!(w.shared().try_write(b"x"), Err(PipeError::BrokenPipe));
    }

    #[test]
    fn writes_up_to_pipe_buf_are_all_or_nothing() {
        let (r, w) = pair();
        let fill = alloc::vec![7_u8; PIPE_MAX_CAPACITY - 100];
        assert_eq!(w.shared().try_write(&fill).unwrap().bytes, fill.len());
        let atomic = alloc::vec![1_u8; 200];
        assert!(matches!(
            w.shared().try_write(&atomic),
            Err(PipeError::WouldBlock { .. })
        ), "200 bytes do not fit in the 100 free: nothing is written");
        assert_eq!(r.shared().bytes_available(), fill.len());
        let fits = alloc::vec![2_u8; 100];
        assert_eq!(w.shared().try_write(&fits).unwrap().bytes, 100);
        assert_eq!(w.shared().capacity() - r.shared().bytes_available(), 0);
    }

    #[test]
    fn writes_larger_than_pipe_buf_are_partial_and_never_zero() {
        let (r, w) = pair();
        let big = alloc::vec![9_u8; PIPE_MAX_CAPACITY + 5000];
        assert_eq!(w.shared().try_write(&big).unwrap().bytes, PIPE_MAX_CAPACITY);
        assert!(matches!(
            w.shared().try_write(&big),
            Err(PipeError::WouldBlock { .. })
        ));
        assert_eq!(read_all(&r, 10).len(), 10);
        assert_eq!(w.shared().try_write(&big).unwrap().bytes, 10, "only the freed space");
    }

    #[test]
    fn reading_space_wakes_the_writer_with_a_fresh_generation() {
        let (r, w) = pair();
        let big = alloc::vec![0_u8; PIPE_MAX_CAPACITY];
        w.shared().try_write(&big).unwrap();
        let observed = match w.shared().try_write(b"x") {
            Err(PipeError::WouldBlock { generation }) => generation,
            other => panic!("full pipe must block: {other:?}"),
        };
        let mut buf = [0_u8; 1];
        let wake = r.shared().try_read(&mut buf).unwrap().wake.unwrap();
        assert_eq!(wake.token, w.token_id());
        assert!(wake.generation > observed);
    }

    #[test]
    fn readiness_tracks_data_space_and_peer_close() {
        let (r, w) = pair();
        let initial = r.end.readiness().unwrap();
        assert_eq!(initial.events, 0);
        assert_eq!(w.end.readiness().unwrap().events, linux_abi::EPOLLOUT);
        w.shared().try_write(b"hi").unwrap();
        let readable = r.end.readiness().unwrap();
        assert_eq!(readable.events, linux_abi::EPOLLIN);
        assert!(readable.generation > initial.generation);
        let big = alloc::vec![0_u8; PIPE_MAX_CAPACITY - 2 - PIPE_BUF + 1];
        w.shared().try_write(&big).unwrap();
        assert_eq!(w.end.readiness().unwrap().events, 0, "less than PIPE_BUF free");
        assert!(w.release_descriptor_reference().last);
        assert_eq!(
            r.end.readiness().unwrap().events,
            linux_abi::EPOLLIN | linux_abi::EPOLLHUP
        );
        assert!(local_wait::readiness_of(r.token_id()).is_some());
    }

    #[test]
    fn write_end_reports_error_once_the_reader_is_gone() {
        let (r, w) = pair();
        assert!(r.release_descriptor_reference().last);
        assert_eq!(
            w.end.readiness().unwrap().events,
            linux_abi::EPOLLOUT | linux_abi::EPOLLERR
        );
        assert!(local_wait::readiness_of(r.token_id()).is_none(), "closed ends leave the registry");
    }

    #[test]
    fn capacity_rounds_to_a_power_of_two_and_refuses_unfit_requests() {
        let (_r, w) = pair();
        assert_eq!(w.shared().capacity(), PIPE_MAX_CAPACITY);
        assert_eq!(w.shared().set_capacity(1).unwrap().0, 4096);
        assert_eq!(w.shared().set_capacity(5000).unwrap().0, 8192);
        assert_eq!(w.shared().set_capacity(PIPE_MAX_CAPACITY + 1), Err(PipeCapacityError::TooLarge));
        w.shared().try_write(&[1_u8; 4096]).unwrap();
        w.shared().try_write(&[1_u8; 4096]).ok();
        assert_eq!(w.shared().set_capacity(4096), Err(PipeCapacityError::Busy));
        let (bytes, wake) = w.shared().set_capacity(PIPE_MAX_CAPACITY).unwrap();
        assert_eq!(bytes, PIPE_MAX_CAPACITY);
        assert!(wake.is_some(), "growth wakes a blocked writer");
    }

    #[test]
    fn buffer_pages_are_charged_while_data_exists_and_refunded_exactly_once() {
        let group = group();
        charge::set_limit(group, 2);
        let (r, w) = PipeHandle::new_pair(Some(group), false).unwrap();
        assert_eq!(charge::usage(group), 0, "an empty pipe holds no pages");
        w.shared().try_write(&[1_u8; 100]).unwrap();
        assert_eq!(charge::usage(group), 1);
        w.shared().try_write(&[2_u8; 100]).unwrap();
        assert_eq!(charge::usage(group), 1, "small writes share the tail page");
        let mut sink = [0_u8; 200];
        assert_eq!(r.shared().try_read(&mut sink).unwrap().bytes, 200);
        assert_eq!(charge::usage(group), 0, "a drained page is refunded");
        // A write needing three pages is refused whole and leaves no charge.
        assert_eq!(
            w.shared().try_write(&alloc::vec![0_u8; 3 * PIPE_PAGE_SIZE]),
            Err(PipeError::NoMemory)
        );
        assert_eq!(charge::usage(group), 0, "a refused write refunds its staged pages");
        w.shared().try_write(&alloc::vec![3_u8; 2 * PIPE_PAGE_SIZE]).unwrap();
        assert_eq!(charge::usage(group), 2);
        assert!(r.release_descriptor_reference().last);
        assert_eq!(charge::usage(group), 0, "closing the reader frees buffered pages");
        charge::set_limit(group, kernel_mm::api::charge::UNLIMITED);
    }

    #[test]
    fn dropping_the_last_handles_refunds_without_an_explicit_close() {
        let group = group();
        let (r, w) = PipeHandle::new_pair(Some(group), false).unwrap();
        w.shared().try_write(&[1_u8; PIPE_PAGE_SIZE * 3]).unwrap();
        assert_eq!(charge::usage(group), 3);
        let read_token = r.token_id();
        drop((r, w));
        assert_eq!(charge::usage(group), 0);
        assert!(local_wait::readiness_of(read_token).is_none());
    }

    #[test]
    fn a_dropped_end_without_release_still_closes_for_the_peer() {
        let (r, w) = pair();
        let write_token = w.token_id();
        drop(w);
        assert!(local_wait::readiness_of(write_token).is_none());
        let mut buf = [0_u8; 1];
        assert_eq!(r.shared().try_read(&mut buf).unwrap().bytes, 0, "EOF");
    }

    #[test]
    fn status_flags_are_per_description_and_shared_by_clones() {
        let (r, w) = pair();
        assert_eq!(r.status_flags(), linux_abi::O_RDONLY);
        assert_eq!(w.status_flags(), linux_abi::O_WRONLY);
        let dup = w.clone();
        dup.set_nonblocking(true);
        assert_eq!(w.status_flags(), linux_abi::O_WRONLY | linux_abi::O_NONBLOCK);
        assert_eq!(r.status_flags(), linux_abi::O_RDONLY, "the other end is a different description");
    }

    #[test]
    fn closed_ends_leave_the_registry_and_open_ones_stay() {
        let (r, w) = pair();
        assert!(local_wait::readiness_of(r.token_id()).is_some());
        assert!(local_wait::readiness_of(w.token_id()).is_some());
        assert!(r.release_descriptor_reference().last);
        assert!(local_wait::readiness_of(r.token_id()).is_none());
        assert!(local_wait::readiness_of(w.token_id()).is_some());
        assert!(w.release_descriptor_reference().last);
        assert!(local_wait::readiness_of(w.token_id()).is_none());
    }
}
