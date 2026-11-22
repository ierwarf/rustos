//! Bounded read / write / hash pipeline for one executable snapshot.
//!
//! - **Owner:** the snapshot worker that runs `open_executable_snapshot` owns the
//!   pipeline for the duration of one call. Nothing here outlives it: the ring
//!   buffer, every stage task and every helper thread are finished (joined)
//!   before [`run`] returns, on success, error and cancellation alike.
//! - **Shape:** a ring of `slots` fixed windows. The caller thread is the
//!   *reader*: it fills window `w` straight into slot `w % slots` (the storaged
//!   lanes read into that very buffer, no staging copy). Two more stages consume
//!   a filled window read-only and in order: the *writer* (sink / memfd write)
//!   and the optional *hasher* (content digest). They run on helper threads when
//!   the host has them, and otherwise the caller runs them between its own reads
//!   (still correct, only serial).
//! - **Ownership invariant (the ring is the only shared state):** three
//!   monotonic counters, all counting whole windows from 0:
//!   `read_done >= max(written, hashed)`, and the reader may start window `w`
//!   only while `w - min(written, hashed) < slots`. So slot `w % slots` is
//!   written by exactly one thread (the reader) only after both consumers
//!   released the window that previously lived there, and a consumer reads
//!   window `w` only after the reader's Release of `read_done > w`. Release /
//!   Acquire on those counters is the whole handoff. At most
//!   `slots * window` bytes are ever resident, whatever the file size.
//! - **Failure:** the first error (read, write, control, source change) is
//!   recorded and the ring is aborted. Every stage observes the abort at its
//!   next step and finishes; the caller joins all helpers before returning, then
//!   reports the bytes the sink confirmed. A pipeline that did not move every
//!   window through every stage returns an error, so a partial image can never
//!   be sealed from an `Ok`.
//! - **Waiting:** stages park on one event word (`Ring::events`) which every
//!   state change bumps and wakes; parks are timed by the host, so a lost wake
//!   costs latency, never progress.

use core::marker::PhantomData;
use core::slice;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use spin::Mutex;
use storage_core::file_io::{
    FileIdentity, ReadAt, TransferControl, TransferError, TransferFailure, WriteAt,
};

/// Window and slot arithmetic for one transfer of `len` bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingGeometry {
    len: u64,
    window: usize,
    slots: usize,
}

impl RingGeometry {
    /// `window` is the window size cap and `slots` the slot count cap; a small
    /// file shrinks both so the ring never exceeds the file. `None` for an empty
    /// transfer or a zero cap.
    pub fn new(len: u64, window: usize, slots: usize) -> Option<Self> {
        if len == 0 || window == 0 || slots == 0 {
            return None;
        }
        let window = usize::try_from(len).map_or(window, |len| len.min(window));
        let windows = len.div_ceil(window as u64);
        let slots = usize::try_from(windows).map_or(slots, |windows| windows.min(slots));
        Some(Self { len, window, slots })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn window_bytes(&self) -> usize {
        self.window
    }

    pub fn slots(&self) -> usize {
        self.slots
    }

    /// Number of windows that cover the transfer.
    pub fn windows(&self) -> u64 {
        self.len.div_ceil(self.window as u64)
    }

    /// File offset and byte length of window `index` (the last may be short).
    pub fn span(&self, index: u64) -> (u64, usize) {
        let offset = index * self.window as u64;
        let remaining = self.len - offset;
        (offset, usize::try_from(remaining).map_or(self.window, |r| r.min(self.window)))
    }

    /// Bytes of ring buffer this transfer needs.
    pub fn buffer_bytes(&self) -> Option<usize> {
        self.window.checked_mul(self.slots)
    }
}

/// Outcome of asking a ring handle for work.
pub enum Poll<T> {
    Ready(T),
    /// Nothing can be done until another stage moves.
    Pending,
    /// No further work will ever come (all windows done, or the ring aborted).
    Finished,
}

/// The ring: slot storage plus the three window counters (see module docs).
pub struct Ring<'b> {
    base: *mut u8,
    geometry: RingGeometry,
    hash_enabled: bool,
    read_done: AtomicU64,
    written: AtomicU64,
    hashed: AtomicU64,
    aborted: AtomicBool,
    events: AtomicU32,
    taken: [AtomicBool; 3],
    _buffer: PhantomData<&'b mut [u8]>,
}

// SAFETY: `base` points into a buffer exclusively borrowed for `'b`. Slot
// access is arbitrated by the counters (module docs): a slot has one mutable
// accessor (the unique `RingReader`) only while no consumer may touch it, and
// shared readers only after the reader's Release publication. The handles are
// handed out once each (`taken`), so no two threads share a role.
unsafe impl Sync for Ring<'_> {}

const READER: usize = 0;
const WRITER: usize = 1;
const HASHER: usize = 2;

impl<'b> Ring<'b> {
    /// `None` if `buffer` is smaller than the geometry needs.
    pub fn new(buffer: &'b mut [u8], geometry: RingGeometry, hash_enabled: bool) -> Option<Self> {
        if buffer.len() < geometry.buffer_bytes()? {
            return None;
        }
        Some(Self {
            base: buffer.as_mut_ptr(),
            geometry,
            hash_enabled,
            read_done: AtomicU64::new(0),
            written: AtomicU64::new(0),
            hashed: AtomicU64::new(0),
            aborted: AtomicBool::new(false),
            events: AtomicU32::new(0),
            taken: [const { AtomicBool::new(false) }; 3],
            _buffer: PhantomData,
        })
    }

    pub fn geometry(&self) -> RingGeometry {
        self.geometry
    }

    /// The event word every state change bumps.
    pub fn events(&self) -> &AtomicU32 {
        &self.events
    }

    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::Acquire)
    }

    /// Stops every stage at its next step.
    pub fn abort(&self) {
        self.aborted.store(true, Ordering::Release);
        self.bump();
    }

    fn bump(&self) {
        self.events.fetch_add(1, Ordering::Release);
    }

    fn take(&self, role: usize) -> bool {
        !self.taken[role].swap(true, Ordering::AcqRel)
    }

    /// The unique producer handle; `None` if already handed out.
    pub fn reader(&self) -> Option<RingReader<'_, 'b>> {
        self.take(READER)
            .then_some(RingReader { ring: self, next: 0, begun: false })
    }

    /// The unique writer-stage handle.
    pub fn writer(&self) -> Option<RingConsumer<'_, 'b>> {
        self.take(WRITER)
            .then_some(RingConsumer { ring: self, stage: WRITER, next: 0 })
    }

    /// The unique hasher-stage handle; `None` when hashing is disabled or the
    /// handle was already handed out.
    pub fn hasher(&self) -> Option<RingConsumer<'_, 'b>> {
        (self.hash_enabled && self.take(HASHER))
            .then_some(RingConsumer { ring: self, stage: HASHER, next: 0 })
    }

    /// Windows whose slot every consumer has released.
    fn released(&self) -> u64 {
        let written = self.written.load(Ordering::Acquire);
        if self.hash_enabled {
            written.min(self.hashed.load(Ordering::Acquire))
        } else {
            written
        }
    }

    fn slot_ptr(&self, window: u64) -> *mut u8 {
        let slot = (window % self.geometry.slots as u64) as usize;
        // SAFETY: `slot < slots` and the constructor checked that the buffer
        // holds `slots * window` bytes.
        unsafe { self.base.add(slot * self.geometry.window) }
    }
}

/// The producer end of the ring.
pub struct RingReader<'r, 'b> {
    ring: &'r Ring<'b>,
    next: u64,
    begun: bool,
}

impl RingReader<'_, '_> {
    /// The next window to fill, if its slot has been released by both consumers.
    /// Repeats the same window until [`Self::commit`].
    pub fn begin(&mut self) -> Poll<(u64, &mut [u8])> {
        let ring = self.ring;
        if ring.is_aborted() || self.next >= ring.geometry.windows() {
            return Poll::Finished;
        }
        if self.next - ring.released() >= ring.geometry.slots as u64 {
            return Poll::Pending;
        }
        let (_, len) = ring.geometry.span(self.next);
        // SAFETY: `next - released < slots`, so the window that last used this
        // slot (`next - slots`) was fully consumed and released with Release
        // stores that `released()` Acquired. `read_done == next`, so no
        // consumer may touch this slot, and this unique handle is the only
        // writer. The borrow of `self` keeps it from outliving `commit`.
        let buffer = unsafe { slice::from_raw_parts_mut(ring.slot_ptr(self.next), len) };
        self.begun = true;
        Poll::Ready((self.next, buffer))
    }

    /// Publishes the window returned by `begin` as fully read.
    pub fn commit(&mut self) {
        assert!(self.begun, "commit without begin");
        self.begun = false;
        self.next += 1;
        // ORDERING: Release publishes every byte written into the slot.
        self.ring.read_done.store(self.next, Ordering::Release);
        self.ring.bump();
    }
}

/// A consuming stage (writer or hasher): sees every filled window once, in order.
pub struct RingConsumer<'r, 'b> {
    ring: &'r Ring<'b>,
    stage: usize,
    next: u64,
}

impl RingConsumer<'_, '_> {
    /// The next filled window, borrowed read-only until [`Self::finish`].
    pub fn begin(&mut self) -> Poll<(u64, &[u8])> {
        let ring = self.ring;
        if ring.is_aborted() || self.next >= ring.geometry.windows() {
            return Poll::Finished;
        }
        // ORDERING: pairs with the reader's Release in `commit`.
        if self.next >= ring.read_done.load(Ordering::Acquire) {
            return Poll::Pending;
        }
        let (_, len) = ring.geometry.span(self.next);
        // SAFETY: `next < read_done`, so the reader finished writing this slot
        // and will not touch it again until this stage's counter passes `next`
        // (see `Ring::released`); consumers only read.
        let data = unsafe { slice::from_raw_parts(ring.slot_ptr(self.next), len) };
        Poll::Ready((self.next, data))
    }

    /// Releases the window returned by `begin` to the reader.
    pub fn finish(&mut self) {
        self.next += 1;
        let counter = if self.stage == WRITER {
            &self.ring.written
        } else {
            &self.ring.hashed
        };
        // ORDERING: Release orders this stage's reads of the slot before the
        // reader's Acquire in `released`.
        counter.store(self.next, Ordering::Release);
        self.ring.bump();
    }
}

/// Cancellation and progress hooks for one pipeline run.
pub trait PipelineControl: Sync {
    /// Polled before every provider operation.
    fn check(&self) -> Result<(), TransferControl>;
    /// Bytes the sink has confirmed so far.
    fn progress(&self, bytes: u64);
}

/// Incremental content digest fed every window once, in order.
pub trait WindowDigest: Send {
    fn update(&mut self, bytes: &[u8]);
}

/// Placeholder digest type for a run that hashes nothing.
pub struct NoDigest;
impl WindowDigest for NoDigest {
    fn update(&mut self, _: &[u8]) {}
}

/// Handle to a started helper task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StageToken(pub usize);

/// Threads and parking for the pipeline's helper stages.
///
/// # Safety
/// `start` may run `task` on another thread with its lifetime erased. The
/// implementation must run it at most once, only between `start` and the
/// matching `join` returning, and `join` must not return before the task has
/// returned and will never touch it again. [`run`] joins every token it
/// started before the borrowed task is dropped.
pub unsafe trait StageHost: Sync {
    /// Runs `task` on a free helper thread, or `None` when none is free.
    unsafe fn start(&self, task: &(dyn Fn() + Sync)) -> Option<StageToken>;
    fn join(&self, token: StageToken);
    /// Parks while `word == expected`; may return early or on a timeout.
    fn wait(&self, word: &AtomicU32, expected: u32);
    /// Wakes every thread parked on `word`.
    fn wake(&self, word: &AtomicU32);
    fn now_ns(&self) -> u64;
}

/// Stage-busy and stall accounting for diagnostics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PipelineStats {
    pub windows: u64,
    /// Stages that ran on their own thread (0..=2); the rest ran on the caller.
    pub helper_stages: u32,
    /// Time the reader had no free slot or other work.
    pub reader_stall_ns: u64,
    pub writer_stall_ns: u64,
    pub hasher_stall_ns: u64,
}

enum Step {
    Progress,
    Idle,
    Done,
}

struct WriterRole<'a, 'b, W> {
    consumer: RingConsumer<'a, 'b>,
    sink: &'a mut W,
}

struct HasherRole<'a, 'b, D> {
    consumer: RingConsumer<'a, 'b>,
    digest: &'a mut D,
}

/// State shared between the caller and the helper stages.
struct Shared<'a, 'b, RE, W: WriteAt, D, C, H> {
    ring: &'a Ring<'b>,
    control: &'a C,
    host: &'a H,
    first_error: Mutex<Option<TransferError<RE, W::Error>>>,
    sink_bytes: AtomicU64,
    writer: Mutex<WriterRole<'a, 'b, W>>,
    hasher: Option<Mutex<HasherRole<'a, 'b, D>>>,
    writer_done: AtomicBool,
    hasher_done: AtomicBool,
    writer_stall_ns: AtomicU64,
    hasher_stall_ns: AtomicU64,
}

impl<RE, W, D, C, H> Shared<'_, '_, RE, W, D, C, H>
where
    W: WriteAt,
    D: WindowDigest,
    C: PipelineControl,
    H: StageHost,
{
    fn signal(&self) {
        self.host.wake(self.ring.events());
    }

    /// Records the first error and stops every stage.
    fn fail(&self, error: TransferError<RE, W::Error>) {
        {
            let mut first = self.first_error.lock();
            if first.is_none() {
                *first = Some(error);
            }
        }
        self.ring.abort();
        self.signal();
    }

    fn writer_step(&self) -> Step {
        if self.writer_done.load(Ordering::Acquire) {
            return Step::Done;
        }
        // Held by whichever thread drives this stage; never contended once a
        // stage has a helper, because the caller then does not drive it.
        let Some(mut guard) = self.writer.try_lock() else {
            return Step::Idle;
        };
        let WriterRole { consumer, sink } = &mut *guard;
        let (index, data) = match consumer.begin() {
            Poll::Finished => {
                self.writer_done.store(true, Ordering::Release);
                self.signal();
                return Step::Done;
            }
            Poll::Pending => return Step::Idle,
            Poll::Ready(window) => window,
        };
        let (offset, _) = self.ring.geometry().span(index);
        let mut sent = 0usize;
        while sent < data.len() {
            if self.ring.is_aborted() {
                self.writer_done.store(true, Ordering::Release);
                self.signal();
                return Step::Done;
            }
            if let Err(error) = self.control.check() {
                self.fail(TransferError::Control(error));
                continue;
            }
            match sink.write_at(offset + sent as u64, &data[sent..]) {
                Err(error) => self.fail(TransferError::Write(error)),
                Ok(0) => self.fail(TransferError::WriteZero),
                Ok(n) if n > data.len() - sent => self.fail(TransferError::InvalidCount),
                Ok(n) => {
                    sent += n;
                    let total = self.sink_bytes.fetch_add(n as u64, Ordering::AcqRel) + n as u64;
                    self.control.progress(total);
                }
            }
        }
        consumer.finish();
        self.signal();
        Step::Progress
    }

    fn hasher_step(&self) -> Step {
        let Some(role) = self.hasher.as_ref() else {
            return Step::Done;
        };
        if self.hasher_done.load(Ordering::Acquire) {
            return Step::Done;
        }
        let Some(mut guard) = role.try_lock() else {
            return Step::Idle;
        };
        let HasherRole { consumer, digest } = &mut *guard;
        match consumer.begin() {
            Poll::Finished => {
                self.hasher_done.store(true, Ordering::Release);
                self.signal();
                Step::Done
            }
            Poll::Pending => Step::Idle,
            Poll::Ready((_, data)) => {
                digest.update(data);
                consumer.finish();
                self.signal();
                Step::Progress
            }
        }
    }

    /// Drives one helper stage until it is done, parking while idle.
    fn drive(&self, step: impl Fn() -> Step, stall_ns: &AtomicU64) {
        loop {
            let seen = self.ring.events().load(Ordering::Acquire);
            match step() {
                Step::Progress => {}
                Step::Done => return,
                Step::Idle => {
                    let parked = self.host.now_ns();
                    self.host.wait(self.ring.events(), seen);
                    stall_ns.fetch_add(
                        self.host.now_ns().saturating_sub(parked),
                        Ordering::Relaxed,
                    );
                }
            }
        }
    }
}

/// Joins a started helper when dropped, so a helper never outlives the data it
/// borrows from the caller's frame.
struct StartedStage<'h, H: StageHost> {
    host: &'h H,
    token: Option<StageToken>,
}

impl<H: StageHost> Drop for StartedStage<'_, H> {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.host.join(token);
        }
    }
}

fn failure<R, W>(completed_bytes: u64, error: TransferError<R, W>) -> TransferFailure<R, W> {
    TransferFailure { completed_bytes, error }
}

/// Moves `source` (exactly `geometry.len()` bytes) to `sink` through the ring in
/// `buffer`, hashing into `digest` when given.
///
/// On `Ok` every byte was read, written and (if requested) hashed, and the
/// sink confirmed exactly `geometry.len()` bytes. On `Err`,
/// `completed_bytes` is the prefix the sink confirmed; no stage is running any
/// more and the ring is no longer referenced.
pub fn run<R, W, D, C, H>(
    source: &mut R,
    sink: &mut W,
    digest: Option<&mut D>,
    buffer: &mut [u8],
    geometry: RingGeometry,
    control: &C,
    host: &H,
) -> Result<PipelineStats, TransferFailure<R::Error, W::Error>>
where
    R: ReadAt,
    R::Error: Send,
    W: WriteAt + Send,
    W::Error: Send,
    D: WindowDigest,
    C: PipelineControl,
    H: StageHost,
{
    let identity = source.identity();
    let len = source.len();
    if len != geometry.len() {
        return Err(failure(0, TransferError::SourceChanged));
    }
    let Some(ring) = Ring::new(buffer, geometry, digest.is_some()) else {
        return Err(failure(0, TransferError::InvalidWindow));
    };
    let (Some(mut reader), Some(writer_handle)) = (ring.reader(), ring.writer()) else {
        return Err(failure(0, TransferError::InvalidWindow));
    };
    let hasher_handle = ring.hasher();
    let shared = Shared::<R::Error, W, D, C, H> {
        ring: &ring,
        control,
        host,
        first_error: Mutex::new(None),
        sink_bytes: AtomicU64::new(0),
        writer: Mutex::new(WriterRole { consumer: writer_handle, sink }),
        hasher: match (hasher_handle, digest) {
            (Some(consumer), Some(digest)) => Some(Mutex::new(HasherRole { consumer, digest })),
            _ => None,
        },
        writer_done: AtomicBool::new(false),
        hasher_done: AtomicBool::new(false),
        writer_stall_ns: AtomicU64::new(0),
        hasher_stall_ns: AtomicU64::new(0),
    };
    let has_hasher = shared.hasher.is_some();
    if !has_hasher {
        shared.hasher_done.store(true, Ordering::Release);
    }

    // Declared before the guards, so they are dropped (joined) first.
    let writer_task = || shared.drive(|| shared.writer_step(), &shared.writer_stall_ns);
    let hasher_task = || shared.drive(|| shared.hasher_step(), &shared.hasher_stall_ns);
    let (mut writer_helper, mut hasher_helper) = (
        StartedStage { host, token: None },
        StartedStage { host, token: None },
    );
    if geometry.windows() > 1 {
        // SAFETY: both guards join before `writer_task` / `hasher_task` (and
        // everything they borrow) are dropped, on every path out of this frame.
        writer_helper.token = unsafe { host.start(&writer_task) };
        if has_hasher {
            hasher_helper.token = unsafe { host.start(&hasher_task) };
        }
    }
    let writer_inline = writer_helper.token.is_none();
    let hasher_inline = has_hasher && hasher_helper.token.is_none();
    let helper_stages =
        u32::from(!writer_inline) + u32::from(has_hasher && !hasher_inline);

    let mut reader_done = false;
    let mut reader_stall_ns = 0u64;
    loop {
        let seen = ring.events().load(Ordering::Acquire);
        let mut progressed = false;
        if !reader_done {
            match read_step(&shared, &mut reader, source, identity, len) {
                Step::Progress => progressed = true,
                Step::Done => {
                    reader_done = true;
                    progressed = true;
                }
                Step::Idle => {}
            }
        }
        if writer_inline && !shared.writer_done.load(Ordering::Acquire) {
            progressed |= !matches!(shared.writer_step(), Step::Idle);
        }
        if hasher_inline && !shared.hasher_done.load(Ordering::Acquire) {
            progressed |= !matches!(shared.hasher_step(), Step::Idle);
        }
        if reader_done
            && shared.writer_done.load(Ordering::Acquire)
            && shared.hasher_done.load(Ordering::Acquire)
        {
            break;
        }
        if !progressed {
            let parked = host.now_ns();
            host.wait(ring.events(), seen);
            reader_stall_ns += host.now_ns().saturating_sub(parked);
        }
    }
    drop(writer_helper);
    drop(hasher_helper);

    let completed = shared.sink_bytes.load(Ordering::Acquire);
    if let Some(error) = shared.first_error.lock().take() {
        return Err(failure(completed, error));
    }
    if completed != len || ring.is_aborted() {
        // Unreachable without a recorded error; never report a partial image
        // as complete.
        return Err(failure(completed, TransferError::InvalidCount));
    }
    if source.identity() != identity || source.len() != len {
        return Err(failure(completed, TransferError::SourceChanged));
    }
    control
        .check()
        .map_err(|error| failure(completed, TransferError::Control(error)))?;
    Ok(PipelineStats {
        windows: geometry.windows(),
        helper_stages,
        reader_stall_ns,
        writer_stall_ns: shared.writer_stall_ns.load(Ordering::Relaxed),
        hasher_stall_ns: shared.hasher_stall_ns.load(Ordering::Relaxed),
    })
}

/// Fills one window from `source`, looping over short reads.
fn read_step<R, W, D, C, H>(
    shared: &Shared<'_, '_, R::Error, W, D, C, H>,
    reader: &mut RingReader<'_, '_>,
    source: &mut R,
    identity: FileIdentity,
    len: u64,
) -> Step
where
    R: ReadAt,
    W: WriteAt,
    D: WindowDigest,
    C: PipelineControl,
    H: StageHost,
{
    let (index, window) = match reader.begin() {
        Poll::Finished => return Step::Done,
        Poll::Pending => return Step::Idle,
        Poll::Ready(window) => window,
    };
    let (offset, _) = shared.ring.geometry().span(index);
    let mut filled = 0usize;
    while filled < window.len() {
        if shared.ring.is_aborted() {
            return Step::Done;
        }
        if let Err(error) = shared.control.check() {
            shared.fail(TransferError::Control(error));
            return Step::Done;
        }
        if source.identity() != identity || source.len() != len {
            shared.fail(TransferError::SourceChanged);
            return Step::Done;
        }
        let want = window.len() - filled;
        match source.read_at(offset + filled as u64, &mut window[filled..]) {
            Err(error) => shared.fail(TransferError::Read(error)),
            Ok(0) => shared.fail(TransferError::UnexpectedEof),
            Ok(n) if n > want => shared.fail(TransferError::InvalidCount),
            Ok(n) => {
                filled += n;
                continue;
            }
        }
        return Step::Done;
    }
    if source.identity() != identity || source.len() != len {
        shared.fail(TransferError::SourceChanged);
        return Step::Done;
    }
    reader.commit();
    shared.signal();
    Step::Progress
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::sync::atomic::AtomicUsize;
    use std::sync::atomic::AtomicI64;
    use std::thread::JoinHandle;

    const ID: FileIdentity = FileIdentity { volume: 1, object: 2, generation: 3 };

    /// Threaded host: up to `helpers` concurrently started tasks.
    struct TestHost {
        helpers: usize,
        started: AtomicUsize,
        joined: AtomicUsize,
        threads: Mutex<Vec<Option<JoinHandle<()>>>>,
        clock: AtomicU64,
    }
    impl TestHost {
        fn new(helpers: usize) -> Self {
            Self {
                helpers,
                started: AtomicUsize::new(0),
                joined: AtomicUsize::new(0),
                threads: Mutex::new(Vec::new()),
                clock: AtomicU64::new(0),
            }
        }
    }
    // SAFETY: the task runs on a std thread that `join` waits for before
    // returning; `run` joins before the borrowed task is dropped.
    unsafe impl StageHost for TestHost {
        unsafe fn start(&self, task: &(dyn Fn() + Sync)) -> Option<StageToken> {
            if self.started.load(Ordering::Acquire) >= self.helpers {
                return None;
            }
            self.started.fetch_add(1, Ordering::AcqRel);
            struct Erased(*const (dyn Fn() + Sync));
            // SAFETY: only used by the spawned thread under the host contract.
            unsafe impl Send for Erased {}
            let erased = Erased(unsafe {
                core::mem::transmute::<&(dyn Fn() + Sync), *const (dyn Fn() + Sync + 'static)>(
                    task,
                )
            });
            let handle = std::thread::spawn(move || {
                let erased = erased;
                // SAFETY: valid until `join` returns.
                unsafe { (*erased.0)() };
            });
            let mut threads = self.threads.lock();
            threads.push(Some(handle));
            Some(StageToken(threads.len() - 1))
        }
        fn join(&self, token: StageToken) {
            let handle = self.threads.lock()[token.0].take().unwrap();
            handle.join().unwrap();
            self.joined.fetch_add(1, Ordering::AcqRel);
        }
        fn wait(&self, _: &AtomicU32, _: u32) {
            std::thread::sleep(std::time::Duration::from_micros(20));
        }
        fn wake(&self, _: &AtomicU32) {}
        fn now_ns(&self) -> u64 {
            self.clock.fetch_add(1, Ordering::Relaxed)
        }
    }

    fn pattern(offset: u64) -> u8 {
        (offset.wrapping_mul(0x9e37_79b9) >> 7) as u8 ^ (offset >> 13) as u8
    }

    struct Source {
        len: u64,
        max_read: usize,
        fail_at: Option<u64>,
        eof_at: Option<u64>,
        reads: AtomicUsize,
        lead: Option<(&'static AtomicU64, u64, usize)>,
        generation: u64,
        change_after_reads: Option<usize>,
    }
    impl Source {
        fn new(len: u64) -> Self {
            Self {
                len,
                max_read: usize::MAX,
                fail_at: None,
                eof_at: None,
                reads: AtomicUsize::new(0),
                lead: None,
                generation: ID.generation,
                change_after_reads: None,
            }
        }
    }
    impl ReadAt for Source {
        type Error = i32;
        fn identity(&self) -> FileIdentity {
            FileIdentity { generation: self.generation, ..ID }
        }
        fn len(&self) -> u64 {
            self.len
        }
        fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, i32> {
            let n = self.reads.fetch_add(1, Ordering::AcqRel);
            if self.change_after_reads.is_some_and(|after| n >= after) {
                self.generation += 1;
            }
            if let Some((sink_bytes, window, slots)) = self.lead {
                // Bounded ring: the window being filled is fewer than `slots`
                // ahead of what the sink has fully confirmed.
                let index = offset / window;
                let confirmed = sink_bytes.load(Ordering::Acquire) / window;
                assert!(index - confirmed < slots as u64, "ring overran its slots");
            }
            if self.fail_at.is_some_and(|at| offset >= at) {
                return Err(5);
            }
            if self.eof_at.is_some_and(|at| offset >= at) {
                return Ok(0);
            }
            let n = output.len().min(self.max_read);
            for (i, byte) in output[..n].iter_mut().enumerate() {
                *byte = pattern(offset + i as u64);
            }
            Ok(n)
        }
    }

    struct Sink {
        data: Vec<u8>,
        max_write: usize,
        fail_at: Option<u64>,
        zero_at: Option<u64>,
        confirmed: &'static AtomicU64,
        delay: bool,
    }
    impl WriteAt for Sink {
        type Error = i32;
        fn write_at(&mut self, offset: u64, input: &[u8]) -> Result<usize, i32> {
            assert_eq!(offset, self.data.len() as u64, "writes must be sequential");
            if self.fail_at.is_some_and(|at| offset >= at) {
                return Err(28);
            }
            if self.zero_at.is_some_and(|at| offset >= at) {
                return Ok(0);
            }
            if self.delay {
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
            let n = input.len().min(self.max_write);
            self.data.extend_from_slice(&input[..n]);
            self.confirmed.fetch_add(n as u64, Ordering::AcqRel);
            Ok(n)
        }
    }

    #[derive(Default)]
    struct Sum {
        bytes: u64,
        sum: u64,
    }
    impl WindowDigest for Sum {
        fn update(&mut self, bytes: &[u8]) {
            for byte in bytes {
                self.sum = self.sum.wrapping_mul(31).wrapping_add(u64::from(*byte));
            }
            self.bytes += bytes.len() as u64;
        }
    }
    fn expected_sum(len: u64) -> u64 {
        let mut sum = Sum::default();
        let all: Vec<u8> = (0..len).map(pattern).collect();
        sum.update(&all);
        sum.sum
    }

    struct Control {
        cancel_after_checks: AtomicI64,
        progress: AtomicU64,
    }
    impl Control {
        fn never() -> Self {
            Self { cancel_after_checks: AtomicI64::new(i64::MAX), progress: AtomicU64::new(0) }
        }
    }
    impl PipelineControl for Control {
        fn check(&self) -> Result<(), TransferControl> {
            if self.cancel_after_checks.fetch_sub(1, Ordering::AcqRel) <= 0 {
                Err(TransferControl::Cancelled)
            } else {
                Ok(())
            }
        }
        fn progress(&self, bytes: u64) {
            let previous = self.progress.swap(bytes, Ordering::AcqRel);
            assert!(bytes >= previous, "progress must be monotonic");
        }
    }

    fn leak_counter() -> &'static AtomicU64 {
        std::boxed::Box::leak(std::boxed::Box::new(AtomicU64::new(0)))
    }

    // ---- ring state machine -------------------------------------------------

    #[test]
    fn geometry_shrinks_to_small_files_and_covers_large_ones() {
        let g = RingGeometry::new(10, 4096, 3).unwrap();
        assert_eq!((g.window_bytes(), g.slots(), g.windows()), (10, 1, 1));
        assert_eq!(g.buffer_bytes(), Some(10));
        let g = RingGeometry::new(8192 + 5, 4096, 3).unwrap();
        assert_eq!((g.window_bytes(), g.slots(), g.windows()), (4096, 3, 3));
        assert_eq!(g.span(2), (8192, 5));
        assert_eq!(g.span(1), (4096, 4096));
        let g = RingGeometry::new(306_789_704, 4 << 20, 3).unwrap();
        assert_eq!(g.windows(), 74);
        assert_eq!(g.buffer_bytes(), Some(12 << 20));
        assert!(RingGeometry::new(0, 4096, 3).is_none());
        assert!(RingGeometry::new(1, 0, 3).is_none());
        assert!(RingGeometry::new(1, 1, 0).is_none());
    }

    #[test]
    fn window_offsets_stay_exact_beyond_four_gib() {
        let len = (1u64 << 32) + 71;
        let g = RingGeometry::new(len, 1 << 20, 3).unwrap();
        assert_eq!(g.windows(), 4097);
        assert_eq!(g.span(4096), (4096 << 20, 71));
        assert_eq!(g.span(4095), (4095 << 20, 1 << 20));
        assert_eq!(g.buffer_bytes(), Some(3 << 20));
    }

    #[test]
    fn ring_bounds_the_reader_to_released_slots() {
        let g = RingGeometry::new(5 * 16, 16, 2).unwrap();
        let mut buffer = alloc::vec![0u8; g.buffer_bytes().unwrap()];
        let ring = Ring::new(&mut buffer, g, true).unwrap();
        let mut reader = ring.reader().unwrap();
        let mut writer = ring.writer().unwrap();
        let mut hasher = ring.hasher().unwrap();
        assert!(ring.reader().is_none() && ring.writer().is_none() && ring.hasher().is_none());
        for expected in 0..2u64 {
            let Poll::Ready((index, window)) = reader.begin() else { panic!() };
            assert_eq!((index, window.len()), (expected, 16));
            window.fill(expected as u8 + 1);
            reader.commit();
        }
        // Both slots are occupied: window 2 must wait.
        assert!(matches!(reader.begin(), Poll::Pending));
        assert!(matches!(writer.begin(), Poll::Ready((0, _))));
        writer.finish();
        // The hasher has not released slot 0 yet.
        assert!(matches!(reader.begin(), Poll::Pending));
        let Poll::Ready((0, data)) = hasher.begin() else { panic!() };
        assert!(data.iter().all(|b| *b == 1));
        hasher.finish();
        let Poll::Ready((2, window)) = reader.begin() else { panic!() };
        // Window 2 reuses the slot of window 0, now released.
        window.fill(3);
        // Not committed yet: consumers must not see window 2.
        assert!(matches!(hasher.begin(), Poll::Ready((1, _))));
        hasher.finish();
        assert!(matches!(hasher.begin(), Poll::Pending));
        reader.commit();
        assert!(matches!(hasher.begin(), Poll::Ready((2, _))));
        // Window 1 is still intact in its own slot for the writer.
        let Poll::Ready((1, data)) = writer.begin() else { panic!() };
        assert!(data.iter().all(|b| *b == 2));
    }

    #[test]
    fn ring_without_hasher_releases_on_write_alone_and_abort_finishes_all() {
        let g = RingGeometry::new(3 * 8, 8, 1).unwrap();
        assert_eq!(g.slots(), 1);
        let mut buffer = alloc::vec![0u8; 8];
        let ring = Ring::new(&mut buffer, g, false).unwrap();
        assert!(ring.hasher().is_none());
        let mut reader = ring.reader().unwrap();
        let mut writer = ring.writer().unwrap();
        assert!(matches!(reader.begin(), Poll::Ready((0, _))));
        reader.commit();
        assert!(matches!(reader.begin(), Poll::Pending));
        assert!(matches!(writer.begin(), Poll::Ready((0, _))));
        writer.finish();
        assert!(matches!(reader.begin(), Poll::Ready((1, _))));
        ring.abort();
        assert!(matches!(reader.begin(), Poll::Finished));
        assert!(matches!(writer.begin(), Poll::Finished));
    }

    #[test]
    fn ring_rejects_a_short_buffer() {
        let g = RingGeometry::new(64, 16, 2).unwrap();
        let mut buffer = alloc::vec![0u8; 31];
        assert!(Ring::new(&mut buffer, g, false).is_none());
    }

    // ---- pipeline -----------------------------------------------------------

    fn run_case(
        len: u64,
        window: usize,
        slots: usize,
        helpers: usize,
        hash: bool,
        max_read: usize,
        max_write: usize,
    ) {
        let confirmed = leak_counter();
        let g = RingGeometry::new(len, window, slots).unwrap();
        let mut source = Source::new(len);
        source.max_read = max_read;
        source.lead = Some((confirmed, g.window_bytes() as u64, g.slots()));
        let mut sink = Sink {
            data: Vec::new(),
            max_write,
            fail_at: None,
            zero_at: None,
            confirmed,
            delay: helpers > 0,
        };
        let mut digest = Sum::default();
        let mut buffer = alloc::vec![0u8; g.buffer_bytes().unwrap()];
        let control = Control::never();
        let host = TestHost::new(helpers);
        let stats = run(
            &mut source,
            &mut sink,
            hash.then_some(&mut digest),
            &mut buffer,
            g,
            &control,
            &host,
        )
        .unwrap_or_else(|e| panic!("len={len} helpers={helpers}: {e:?}"));
        assert_eq!(sink.data.len() as u64, len);
        assert!(sink.data.iter().enumerate().all(|(i, b)| *b == pattern(i as u64)));
        assert_eq!(stats.windows, g.windows());
        assert_eq!(control.progress.load(Ordering::Acquire), len);
        if hash {
            assert_eq!(digest.bytes, len);
            assert_eq!(digest.sum, expected_sum(len));
        } else {
            assert_eq!(digest.bytes, 0);
        }
        assert_eq!(host.started.load(Ordering::Acquire), host.joined.load(Ordering::Acquire));
        let expected_helpers = if g.windows() > 1 { helpers.min(1 + usize::from(hash)) } else { 0 };
        assert_eq!(stats.helper_stages as usize, expected_helpers);
    }

    #[test]
    fn data_is_exact_with_every_stage_placement_and_odd_sizes() {
        for helpers in 0..=2 {
            for hash in [false, true] {
                run_case(1, 64, 3, helpers, hash, usize::MAX, usize::MAX);
                run_case(64, 64, 3, helpers, hash, usize::MAX, usize::MAX);
                run_case(64 * 5 + 17, 64, 3, helpers, hash, usize::MAX, usize::MAX);
                run_case(64 * 7, 64, 2, helpers, hash, usize::MAX, usize::MAX);
                run_case(64 * 9 + 1, 64, 3, helpers, hash, 13, 7);
                run_case(100_003, 4096, 3, helpers, hash, 1000, 1500);
            }
        }
    }

    #[test]
    fn read_error_aborts_cleanly_and_reports_the_confirmed_prefix() {
        for helpers in 0..=2 {
            let confirmed = leak_counter();
            let g = RingGeometry::new(64 * 12, 64, 3).unwrap();
            let mut source = Source::new(64 * 12);
            source.fail_at = Some(64 * 6);
            let mut sink = Sink {
                data: Vec::new(), max_write: usize::MAX, fail_at: None, zero_at: None,
                confirmed, delay: false,
            };
            let mut digest = Sum::default();
            let mut buffer = alloc::vec![0u8; g.buffer_bytes().unwrap()];
            let host = TestHost::new(helpers);
            let error = run(&mut source, &mut sink, Some(&mut digest), &mut buffer, g,
                &Control::never(), &host).unwrap_err();
            assert_eq!(error.error, TransferError::Read(5));
            // Only whole, previously read windows can have been written.
            assert_eq!(error.completed_bytes, sink.data.len() as u64);
            assert!(error.completed_bytes <= 64 * 6);
            assert!(sink.data.iter().enumerate().all(|(i, b)| *b == pattern(i as u64)));
            assert_eq!(host.started.load(Ordering::Acquire), host.joined.load(Ordering::Acquire));
        }
    }

    #[test]
    fn write_error_unblocks_a_reader_waiting_on_a_full_ring() {
        for helpers in 0..=2 {
            let confirmed = leak_counter();
            let g = RingGeometry::new(64 * 40, 64, 2).unwrap();
            let mut source = Source::new(64 * 40);
            let mut sink = Sink {
                data: Vec::new(), max_write: 10, fail_at: Some(64 + 20), zero_at: None,
                confirmed, delay: true,
            };
            let mut buffer = alloc::vec![0u8; g.buffer_bytes().unwrap()];
            let host = TestHost::new(helpers);
            let error = run(&mut source, &mut sink, None::<&mut NoDigest>, &mut buffer, g,
                &Control::never(), &host).unwrap_err();
            assert_eq!(error.error, TransferError::Write(28));
            // Partial writes before the failing one still count as confirmed.
            assert_eq!(error.completed_bytes, 64 + 20);
            assert_eq!(sink.data.len() as u64, 64 + 20);
            assert_eq!(host.started.load(Ordering::Acquire), host.joined.load(Ordering::Acquire));
        }
    }

    #[test]
    fn eof_zero_write_and_source_change_are_errors_not_partial_images() {
        let g = RingGeometry::new(64 * 6, 64, 3).unwrap();
        let mut buffer = alloc::vec![0u8; g.buffer_bytes().unwrap()];
        for helpers in 0..=2 {
            let host = TestHost::new(helpers);
            let confirmed = leak_counter();
            let sink = || Sink {
                data: Vec::new(), max_write: usize::MAX, fail_at: None, zero_at: None,
                confirmed, delay: false,
            };
            let mut source = Source::new(64 * 6);
            source.eof_at = Some(64 * 2);
            let error = run(&mut source, &mut sink(), None::<&mut NoDigest>, &mut buffer, g,
                &Control::never(), &host).unwrap_err();
            assert_eq!(error.error, TransferError::UnexpectedEof);

            let mut zero = sink();
            zero.zero_at = Some(64);
            let error = run(&mut Source::new(64 * 6), &mut zero, None::<&mut NoDigest>, &mut buffer,
                g, &Control::never(), &host).unwrap_err();
            assert_eq!(error.error, TransferError::WriteZero);
            assert_eq!(error.completed_bytes, 64);

            let mut changing = Source::new(64 * 6);
            changing.change_after_reads = Some(1);
            let error = run(&mut changing, &mut sink(), None::<&mut NoDigest>, &mut buffer, g,
                &Control::never(), &host).unwrap_err();
            assert_eq!(error.error, TransferError::SourceChanged);

            let mut short = Source::new(64 * 6 + 1);
            let error = run(&mut short, &mut sink(), None::<&mut NoDigest>, &mut buffer, g,
                &Control::never(), &host).unwrap_err();
            assert_eq!(error.error, TransferError::SourceChanged);
            assert_eq!(error.completed_bytes, 0);
            assert_eq!(host.started.load(Ordering::Acquire), host.joined.load(Ordering::Acquire));
        }
    }

    #[test]
    fn cancellation_stops_every_stage_and_joins() {
        for helpers in 0..=2 {
            for after in [0i64, 3, 11, 40] {
                let confirmed = leak_counter();
                let g = RingGeometry::new(64 * 30, 64, 3).unwrap();
                let mut source = Source::new(64 * 30);
                let mut sink = Sink {
                    data: Vec::new(), max_write: usize::MAX, fail_at: None, zero_at: None,
                    confirmed, delay: helpers > 0,
                };
                let mut digest = Sum::default();
                let mut buffer = alloc::vec![0u8; g.buffer_bytes().unwrap()];
                let control = Control {
                    cancel_after_checks: AtomicI64::new(after),
                    progress: AtomicU64::new(0),
                };
                let host = TestHost::new(helpers);
                let result = run(&mut source, &mut sink, Some(&mut digest), &mut buffer, g,
                    &control, &host);
                let error = result.unwrap_err();
                assert_eq!(error.error, TransferError::Control(TransferControl::Cancelled));
                assert_eq!(error.completed_bytes, sink.data.len() as u64);
                assert_eq!(host.started.load(Ordering::Acquire), host.joined.load(Ordering::Acquire));
            }
        }
    }

    #[test]
    fn a_ring_buffer_smaller_than_the_geometry_is_rejected_up_front() {
        let g = RingGeometry::new(100, 16, 2).unwrap();
        let mut buffer = alloc::vec![0u8; 8];
        let confirmed = leak_counter();
        let mut sink = Sink {
            data: Vec::new(), max_write: usize::MAX, fail_at: None, zero_at: None,
            confirmed, delay: false,
        };
        let error = run(&mut Source::new(100), &mut sink, None::<&mut NoDigest>, &mut buffer, g,
            &Control::never(), &TestHost::new(0)).unwrap_err();
        assert_eq!(error.error, TransferError::InvalidWindow);
        assert!(sink.data.is_empty());
    }
}
