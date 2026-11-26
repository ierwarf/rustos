//! Windowed block-read engine: cache, in-flight tracking and stream read-ahead.
//!
//! - **Owner:** `storaged` owns read caching and read-ahead policy. The DVM
//!   transport (`block.rs`) only submits, collects and cancels tickets.
//! - **Boundary:** client ranges, provider generation and completions are
//!   untrusted; geometry is validated before any window is requested.
//! - **Lifecycle:** a window is requested, completed (or failed), consumed and
//!   evicted; a write or generation change drops it before reuse.
//! - **Unit:** the device is cached and fetched in aligned `WINDOW_BYTES`
//!   windows. A window is `Ready`, `InFlight` (one transport ticket) or
//!   `Failed`; a window is requested from the transport at most once, so
//!   concurrent readers of the same range share one fetch.
//! - **Streams:** a small table of sequential streams. A read that starts at or
//!   just past the previous read's last window continues that stream; from the
//!   second sequential read on the stream reads ahead, and its depth doubles per
//!   sequential read up to the current cap (Linux readahead ramp-up). A read
//!   that skips windows ahead of its stream halves the depth and, below
//!   `INITIAL_DEPTH`, stops read-ahead until sequential order is seen again
//!   (ramp-down): a stream that jumps around fetches nothing speculative.
//!   Interleaved streams, such as several parallel readers of one file, are
//!   tracked independently instead of resetting one shared frontier.
//! - **Feedback:** every speculative window is tagged. Consuming it counts as
//!   used; being evicted or dropped unread counts as wasted. Each epoch of
//!   resolved windows moves a global depth cap down when most were wasted and
//!   back up when nearly all were used, so a workload whose access pattern
//!   merely looks sequential (many processes faulting one big file) cannot keep
//!   the device busy with data nobody reads.
//! - **Completion:** exactly one pump thread collects completed tickets and
//!   wakes the readers waiting on them, so readers never contend for transport
//!   waits and a read-ahead window completes without any reader asking for it.
//! - **Concurrency:** the engine lock covers state and non-blocking transport
//!   submits only. It is never held across a completion wait or a blocking
//!   transport call; the pump collects outside the lock with the buffer moved out
//!   of the window.
//!   A `Ready` window's bytes are shared (`Arc`): a reader pins the windows it
//!   needs under the lock and copies them out after releasing it, so the large
//!   memcpy never serializes other readers. Eviction or invalidation only drops
//!   the cache's reference; a pinned buffer is recycled only after its last
//!   reader lets go, so it cannot be overwritten under a copy.
//! - **Invalidation:** a write marks overlapping `InFlight` windows stale. A
//!   stale window is collected and dropped, never cached, so bytes read before a
//!   write cannot repopulate the cache after it.
//! - **Failure:** a transport error fails the window, is returned to the waiting
//!   reader and is not cached. A ticket past `COMPLETION_TIMEOUT` is reported to
//!   the transport (quarantine) and its window dropped.
//! - **Forbidden:** no cache across generation, no window cached after a failed
//!   or stale completion, no unbounded read-ahead, no lock held across a wait.
//! - **Evidence:** `dvm-read-cache` and `dvm-volume-io`.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rustos_user_abi::syscall::{DvmBlockTicketWire, BLOCK_BROKER_MAX_IO_BYTES};

pub(crate) const WINDOW_BYTES: usize = BLOCK_BROKER_MAX_IO_BYTES;
pub(crate) const COMPLETION_TIMEOUT: Duration = Duration::from_secs(15);

/// Cached plus in-flight windows (12 MiB). Streamed data is marked evict-first
/// once consumed, so this bounds read-ahead, not the working set.
const WINDOW_LIMIT: usize = 192;
/// Read-ahead and demand fetches together stay below the transport's 64 data
/// slots so writes and flushes are never starved of a slot.
const IN_FLIGHT_LIMIT: usize = 48;
const _: () = assert!(IN_FLIGHT_LIMIT < WINDOW_LIMIT);
const STREAM_LIMIT: usize = 8;
const INITIAL_DEPTH: u64 = 4;
const MAX_DEPTH: u64 = 40;
/// A request continues a stream when it starts at most this many windows past
/// the stream's last window, or at most `STREAM_BACKWARD_WINDOWS` before it.
/// The backward slack absorbs several parallel readers whose adjacent chunks
/// arrive out of order.
const STREAM_SLACK_WINDOWS: u64 = 2;
const STREAM_BACKWARD_WINDOWS: u64 = 4;
const FREE_BUFFER_LIMIT: usize = 64;
const REPORT_INTERVAL_READS: u64 = 1024;
/// Resolved speculative windows per feedback epoch.
const FEEDBACK_EPOCH: u64 = 32;

/// Device geometry a read is validated against. A change of any field drops
/// every cached and in-flight window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Geometry {
    pub generation: u64,
    pub block_size: usize,
    pub capacity_blocks: u64,
    window_blocks: u64,
}

impl Geometry {
    pub(crate) fn new(generation: u64, block_size: usize, capacity_blocks: u64) -> Option<Self> {
        if generation == 0
            || block_size == 0
            || block_size > WINDOW_BYTES
            || WINDOW_BYTES % block_size != 0
            || capacity_blocks == 0
        {
            return None;
        }
        Some(Self {
            generation,
            block_size,
            capacity_blocks,
            window_blocks: (WINDOW_BYTES / block_size) as u64,
        })
    }

    fn last_window(&self) -> u64 {
        (self.capacity_blocks - 1) / self.window_blocks
    }

    /// First block and block count of window `index` (the last one may be short).
    fn window_extent(&self, index: u64) -> (u64, u64) {
        let start = index * self.window_blocks;
        (start, self.window_blocks.min(self.capacity_blocks - start))
    }
}

/// The transport the engine fetches through. `block.rs` implements it with the
/// DVM block broker; tests implement it in memory.
pub(crate) trait Transport {
    fn submit_read(
        &self,
        generation: u64,
        lba: u64,
        block_count: u64,
        byte_len: usize,
    ) -> Result<DvmBlockTicketWire, i32>;
    /// One non-blocking collect: `Ok(true)` only after this exact ticket has
    /// overwritten the entire supplied slice. Recycled buffers may retain
    /// prior contents; pending or failed collects must never publish them.
    fn try_collect(&self, ticket: DvmBlockTicketWire, buf: &mut [u8]) -> Result<bool, i32>;
    fn cancel(&self, ticket: DvmBlockTicketWire);
    /// A ticket outlived `COMPLETION_TIMEOUT`. Runs without the engine lock; may
    /// call back into `clear`.
    fn timed_out(&self, ticket: DvmBlockTicketWire);
}

enum State {
    InFlight {
        ticket: DvmBlockTicketWire,
        /// `None` while the pump is collecting into it.
        buf: Option<Vec<u8>>,
        stale: bool,
    },
    /// Shared so a reader can pin the bytes and copy them out without the
    /// engine lock; eviction or invalidation only drops the cache's reference.
    Ready(Arc<Vec<u8>>),
    Failed(i32),
}

/// One run of a read served from one pinned window. `bytes` keeps the window's
/// buffer alive (and un-recycled) while the reader copies after dropping the
/// engine lock.
struct Segment {
    bytes: Arc<Vec<u8>>,
    src: usize,
    dst: usize,
    len: usize,
}

/// A broker read is at most one window in bytes and therefore intersects at
/// most two cache windows. Keep those pins on the reader's stack; retain a
/// general overflow path for larger internal reads and the engine tests.
struct PinnedSegments {
    inline: [Option<Segment>; 2],
    overflow: Vec<Segment>,
    len: usize,
}

impl PinnedSegments {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            inline: [None, None],
            overflow: Vec::with_capacity(capacity.saturating_sub(2)),
            len: 0,
        }
    }

    fn push(&mut self, segment: Segment) {
        if self.len < self.inline.len() {
            self.inline[self.len] = Some(segment);
        } else {
            self.overflow.push(segment);
        }
        self.len += 1;
    }

    fn len(&self) -> usize {
        self.len
    }

    fn iter(&self) -> impl Iterator<Item = &Segment> {
        self.inline.iter().filter_map(Option::as_ref).chain(&self.overflow)
    }
}

struct Collection {
    index: u64,
    ticket: DvmBlockTicketWire,
    buf: Vec<u8>,
    result: Result<bool, i32>,
}

/// Owned by the single completion pump, outside the engine mutex. Reusing the
/// lists also avoids a second allocation to turn submissions into results.
#[derive(Default)]
pub(crate) struct PumpScratch {
    batch: Vec<Collection>,
    expired: Vec<(u64, DvmBlockTicketWire)>,
}

struct Window {
    state: State,
    /// Fetched speculatively and not read yet.
    ahead: bool,
    /// Larger is more recent; zero marks consumed streaming data, evicted first.
    stamp: u64,
    submitted: Instant,
}

struct Stream {
    id: u64,
    last_window: u64,
    ahead_end: u64,
    depth: u64,
    run: u32,
    stamp: u64,
}

struct Inner {
    /// Current ceiling of any stream's depth, moved by used/wasted feedback.
    depth_cap: u64,
    epoch_used: u64,
    epoch_wasted: u64,
    ahead_used_total: u64,
    ahead_wasted_total: u64,
    geometry: Option<Geometry>,
    windows: BTreeMap<u64, Window>,
    streams: Vec<Stream>,
    free_buffers: Vec<Vec<u8>>,
    in_flight: usize,
    stamp: u64,
    next_stream_id: u64,
}

#[derive(Default)]
struct Stats {
    reads: AtomicU64,
    waited: AtomicU64,
    stall_ns: AtomicU64,
    demand_windows: AtomicU64,
    ahead_windows: AtomicU64,
    in_flight_peak: AtomicU64,
}

pub(crate) struct PumpStatus {
    pub in_flight: usize,
    pub progressed: usize,
}

pub(crate) struct Engine {
    inner: Mutex<Inner>,
    /// A window changed state: waiting readers re-check.
    ready: Condvar,
    /// A window went in flight: the parked pump has work.
    work: Condvar,
    /// A stream's read-ahead lead may have run low, or the pump freed
    /// in-flight capacity, since the last `top_up`. Lets `top_up` skip the
    /// engine lock when nothing changed.
    topup_hint: AtomicBool,
    stats: Stats,
}

impl Inner {
    const fn new() -> Self {
        Self {
            depth_cap: MAX_DEPTH,
            epoch_used: 0,
            epoch_wasted: 0,
            ahead_used_total: 0,
            ahead_wasted_total: 0,
            geometry: None,
            windows: BTreeMap::new(),
            streams: Vec::new(),
            free_buffers: Vec::new(),
            in_flight: 0,
            stamp: 0,
            next_stream_id: 1,
        }
    }

    fn next_stamp(&mut self) -> u64 {
        self.stamp += 1;
        self.stamp
    }

    fn take_buffer(&mut self, len: usize) -> Vec<u8> {
        match self.free_buffers.iter().position(|b| b.capacity() >= len) {
            Some(index) => {
                let mut buf = self.free_buffers.swap_remove(index);
                buf.resize(len, 0);
                buf
            }
            None => vec![0_u8; len],
        }
    }

    /// Returns a cache-owned window buffer to the free list when no reader still
    /// pins it. A pinned buffer is simply released: its last reader frees it,
    /// so it can never be reused (and overwritten) under a reader's copy.
    fn release(&mut self, bytes: Arc<Vec<u8>>) {
        if let Ok(buf) = Arc::try_unwrap(bytes) {
            self.recycle(buf);
        }
    }

    fn recycle(&mut self, buf: Vec<u8>) {
        if self.free_buffers.len() < FREE_BUFFER_LIMIT && buf.capacity() > 0 {
            // The whole slice is overwritten before a successful collect can
            // publish it. Retain initialized bytes so take_buffer's resize is
            // a no-op for the usual full window, instead of zeroing 64 KiB.
            self.free_buffers.push(buf);
        }
    }

    /// Drops everything cached and flags everything in flight stale.
    fn reset(&mut self) {
        let windows = core::mem::take(&mut self.windows);
        for (index, mut window) in windows {
            match window.state {
                State::InFlight { ticket, buf, .. } => {
                    window.state = State::InFlight {
                        ticket,
                        buf,
                        stale: true,
                    };
                    self.windows.insert(index, window);
                }
                State::Ready(bytes) => {
                    if window.ahead {
                        self.resolve_ahead(0, 1);
                    }
                    self.release(bytes);
                }
                State::Failed(_) => {}
            }
        }
        self.streams.clear();
    }

    /// Feeds speculative-window outcomes back into the depth cap.
    fn resolve_ahead(&mut self, used: u64, wasted: u64) {
        self.epoch_used += used;
        self.epoch_wasted += wasted;
        self.ahead_used_total += used;
        self.ahead_wasted_total += wasted;
        let total = self.epoch_used + self.epoch_wasted;
        if total < FEEDBACK_EPOCH {
            return;
        }
        if self.epoch_wasted * 2 > total {
            self.depth_cap = (self.depth_cap / 2).max(INITIAL_DEPTH);
        } else if self.epoch_wasted * 4 <= total {
            self.depth_cap = (self.depth_cap * 2).min(MAX_DEPTH);
        }
        self.epoch_used = 0;
        self.epoch_wasted = 0;
    }

    /// Rebinds to `geometry`, dropping state of any other generation or shape.
    fn bind(&mut self, geometry: Geometry) {
        if self.geometry != Some(geometry) {
            self.reset();
            self.geometry = Some(geometry);
        }
    }

    /// Evicts the least recently used `Ready` window when the cache is full.
    /// Returns whether a new window may be admitted; demand fetches are admitted
    /// regardless because a reader is blocked on them.
    fn make_room(&mut self, demand: bool) -> bool {
        if self.windows.len() < WINDOW_LIMIT {
            return true;
        }
        let victim = self
            .windows
            .iter()
            .filter(|(_, w)| matches!(w.state, State::Ready(_) | State::Failed(_)))
            .min_by_key(|(_, w)| w.stamp)
            .map(|(index, _)| *index);
        match victim {
            Some(index) => {
                if let Some(Window {
                    state: State::Ready(bytes),
                    ahead,
                    ..
                }) = self.windows.remove(&index)
                {
                    if ahead {
                        self.resolve_ahead(0, 1);
                    }
                    self.release(bytes);
                }
                true
            }
            None => demand,
        }
    }

    /// Records a read of windows `first..=last`; returns its stream id.
    fn note_access(&mut self, first: u64, last: u64) -> u64 {
        let stamp = self.next_stamp();
        let cap = self.depth_cap;
        if let Some(stream) = self.streams.iter_mut().find(|s| {
            first + STREAM_BACKWARD_WINDOWS >= s.last_window
                && first <= s.last_window + STREAM_SLACK_WINDOWS
        }) {
            let skipped = first > stream.last_window + 1;
            stream.last_window = last.max(stream.last_window);
            stream.stamp = stamp;
            if skipped {
                // Ramp-down: windows were jumped over, so the stream is not
                // reading what was fetched for it. Halve the depth; below the
                // initial depth stop reading ahead until order returns.
                stream.depth /= 2;
                if stream.depth < INITIAL_DEPTH {
                    stream.depth = 0;
                    stream.run = 1;
                }
                stream.ahead_end = stream.ahead_end.min(stream.last_window);
            } else {
                stream.run = stream.run.saturating_add(1);
                if stream.run >= 2 {
                    stream.depth = if stream.depth == 0 {
                        INITIAL_DEPTH
                    } else {
                        stream.depth * 2
                    }
                    .min(cap);
                }
            }
            return stream.id;
        }
        if self.streams.len() == STREAM_LIMIT {
            if let Some(oldest) = self
                .streams
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| s.stamp)
                .map(|(index, _)| index)
            {
                self.streams.swap_remove(oldest);
            }
        }
        let id = self.next_stream_id;
        self.next_stream_id += 1;
        self.streams.push(Stream {
            id,
            last_window: last,
            ahead_end: last,
            depth: 0,
            run: 1,
            stamp,
        });
        id
    }
}

impl Engine {
    pub(crate) const fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::new()),
            ready: Condvar::new(),
            work: Condvar::new(),
            topup_hint: AtomicBool::new(false),
            stats: Stats {
                reads: AtomicU64::new(0),
                waited: AtomicU64::new(0),
                stall_ns: AtomicU64::new(0),
                demand_windows: AtomicU64::new(0),
                ahead_windows: AtomicU64::new(0),
                in_flight_peak: AtomicU64::new(0),
            },
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Submits window `index`. The caller holds the engine lock.
    fn submit<T: Transport + ?Sized>(
        &self,
        inner: &mut Inner,
        transport: &T,
        index: u64,
        demand: bool,
    ) -> Result<(), i32> {
        let geometry = inner.geometry.ok_or(libc::ESTALE)?;
        let (start, blocks) = geometry.window_extent(index);
        let byte_len = blocks as usize * geometry.block_size;
        inner.make_room(demand);
        let ticket = transport.submit_read(geometry.generation, start, blocks, byte_len)?;
        let buf = inner.take_buffer(byte_len);
        let stamp = inner.next_stamp();
        inner.windows.insert(
            index,
            Window {
                state: State::InFlight {
                    ticket,
                    buf: Some(buf),
                    stale: false,
                },
                ahead: !demand,
                stamp,
                submitted: Instant::now(),
            },
        );
        inner.in_flight += 1;
        self.stats
            .in_flight_peak
            .fetch_max(inner.in_flight as u64, Ordering::Relaxed);
        let counter = if demand {
            &self.stats.demand_windows
        } else {
            &self.stats.ahead_windows
        };
        counter.fetch_add(1, Ordering::Relaxed);
        self.work.notify_one();
        Ok(())
    }

    /// Submits the windows `stream_id` should have in flight ahead of its read
    /// position. Best effort: any refusal just ends the read-ahead.
    fn read_ahead<T: Transport + ?Sized>(
        &self,
        inner: &mut Inner,
        transport: &T,
        stream_id: u64,
        only_when_low: bool,
    ) {
        let Some(geometry) = inner.geometry else {
            return;
        };
        let Some(stream) = inner.streams.iter().find(|s| s.id == stream_id) else {
            return;
        };
        let depth = stream.depth.min(inner.depth_cap);
        if depth == 0 {
            return;
        }
        let lead = stream.ahead_end.saturating_sub(stream.last_window);
        if only_when_low && lead >= depth / 2 {
            return;
        }
        let target = (stream.last_window + depth).min(geometry.last_window());
        let mut next = stream.ahead_end.max(stream.last_window) + 1;
        while next <= target && inner.in_flight < IN_FLIGHT_LIMIT {
            if !inner.windows.contains_key(&next) {
                if !inner.make_room(false) || self.submit(inner, transport, next, false).is_err() {
                    break;
                }
            }
            next += 1;
        }
        if let Some(stream) = inner.streams.iter_mut().find(|s| s.id == stream_id) {
            stream.ahead_end = stream.ahead_end.max(next - 1);
        }
    }

    /// Reads `block_count` blocks at `lba` into `out`, returning the byte count.
    /// Missing windows are fetched; a window already in flight is waited for, not
    /// fetched again.
    pub(crate) fn read<T: Transport + ?Sized>(
        &self,
        transport: &T,
        geometry: Geometry,
        lba: u64,
        block_count: u64,
        out: &mut [u8],
    ) -> Result<usize, i32> {
        let end = lba
            .checked_add(block_count)
            .filter(|end| *end <= geometry.capacity_blocks && block_count != 0)
            .ok_or(libc::EINVAL)?;
        let total = usize::try_from(block_count)
            .ok()
            .and_then(|blocks| blocks.checked_mul(geometry.block_size))
            .ok_or(libc::EINVAL)?;
        if total > out.len() {
            return Err(libc::EOVERFLOW);
        }
        let first = lba / geometry.window_blocks;
        let last = (end - 1) / geometry.window_blocks;
        let deadline = Instant::now() + COMPLETION_TIMEOUT;
        let mut waited_since = None;
        let segments;
        let mut inner = self.lock();
        inner.bind(geometry);
        let stream_id = inner.note_access(first, last);
        loop {
            let mut ready = true;
            let mut fetched = false;
            for index in first..=last {
                match inner.windows.get(&index).map(|w| &w.state) {
                    Some(State::Ready(_)) => {}
                    Some(State::InFlight { .. }) => ready = false,
                    Some(State::Failed(errno)) => {
                        let errno = *errno;
                        inner.windows.remove(&index);
                        return Err(errno);
                    }
                    None => {
                        self.submit(&mut inner, transport, index, true)?;
                        ready = false;
                        fetched = true;
                    }
                }
            }
            if fetched {
                self.read_ahead(&mut inner, transport, stream_id, false);
            }
            if ready {
                segments = self
                    .pin_segments(&mut inner, geometry, stream_id, lba, end, first, last)
                    .ok_or(libc::EIO)?;
                break;
            }
            let now = Instant::now();
            let Some(remaining) = deadline
                .checked_duration_since(now)
                .filter(|d| !d.is_zero())
            else {
                return Err(libc::ETIMEDOUT);
            };
            waited_since.get_or_insert(now);
            inner = match self.ready.wait_timeout(inner, remaining) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
            if inner.geometry != Some(geometry) {
                return Err(libc::ESTALE);
            }
        }
        drop(inner);
        // The copy runs without the engine lock: each segment pins its window's
        // buffer, so eviction, invalidation or a write racing this read only
        // drops the cache's reference. The read observes either side of such a
        // write, as a read concurrent with it always could.
        for segment in segments.iter() {
            out[segment.dst..segment.dst + segment.len]
                .copy_from_slice(&segment.bytes[segment.src..segment.src + segment.len]);
        }
        drop(segments);
        self.stats.reads.fetch_add(1, Ordering::Relaxed);
        if let Some(since) = waited_since {
            self.stats.waited.fetch_add(1, Ordering::Relaxed);
            self.stats.stall_ns.fetch_add(
                u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
        self.report_if_due();
        Ok(total)
    }

    /// Pins the `Ready` windows `first..=last` for the copy of `lba..end` and
    /// updates their recency. The caller holds the engine lock and has just
    /// seen every window `Ready`.
    #[allow(clippy::too_many_arguments)]
    fn pin_segments(
        &self,
        inner: &mut Inner,
        geometry: Geometry,
        stream_id: u64,
        lba: u64,
        end: u64,
        first: u64,
        last: u64,
    ) -> Option<PinnedSegments> {
        let streaming = inner
            .streams
            .iter()
            .any(|s| s.id == stream_id && s.depth > 0);
        let cap = inner.depth_cap;
        let low = inner.streams.iter().any(|s| {
            s.id == stream_id
                && s.depth > 0
                && s.ahead_end.saturating_sub(s.last_window) < s.depth.min(cap) / 2
        });
        if low {
            // ORDERING: Relaxed; `top_up` re-reads the state under the lock.
            self.topup_hint.store(true, Ordering::Relaxed);
        }
        let expected = (last - first + 1) as usize;
        let mut segments = PinnedSegments::with_capacity(expected);
        let mut used = 0;
        for index in first..=last {
            let (start, blocks) = geometry.window_extent(index);
            let from = lba.max(start);
            let to = end.min(start + blocks);
            let consumed = to == start + blocks;
            let stamp = if streaming && consumed {
                0
            } else {
                inner.next_stamp()
            };
            if let Some(window) = inner.windows.get_mut(&index) {
                if let State::Ready(bytes) = &window.state {
                    segments.push(Segment {
                        bytes: Arc::clone(bytes),
                        src: (from - start) as usize * geometry.block_size,
                        dst: (from - lba) as usize * geometry.block_size,
                        len: (to - from) as usize * geometry.block_size,
                    });
                }
                window.stamp = stamp;
                if window.ahead {
                    window.ahead = false;
                    used += 1;
                }
            }
        }
        if used != 0 {
            inner.resolve_ahead(used, 0);
        }
        // Every window was `Ready` under this same lock hold, so a gap is an
        // engine bug; never report success with bytes missing from `out`.
        (segments.len() == expected).then_some(segments)
    }

    /// Runs after a reply has been sent: refills the read-ahead of streams whose
    /// lead has run low, so the submit syscalls overlap the requester's own work.
    pub(crate) fn top_up<T: Transport + ?Sized>(&self, transport: &T) {
        // ORDERING: Relaxed; the stream state itself is read under the lock.
        if !self.topup_hint.swap(false, Ordering::Relaxed) {
            return;
        }
        let mut inner = self.lock();
        let mut ids = [0_u64; STREAM_LIMIT];
        let count = inner.streams.len();
        for (slot, stream) in ids.iter_mut().zip(&inner.streams) {
            *slot = stream.id;
        }
        for &id in &ids[..count] {
            self.read_ahead(&mut inner, transport, id, true);
        }
    }

    /// Drops cached bytes overlapping a write and flags overlapping fetches stale.
    pub(crate) fn invalidate_range(&self, generation: u64, lba: u64, block_count: u64) {
        let mut inner = self.lock();
        let Some(geometry) = inner.geometry.filter(|g| g.generation == generation) else {
            return;
        };
        if block_count == 0 {
            return;
        }
        let first = lba / geometry.window_blocks;
        let last = lba.saturating_add(block_count - 1) / geometry.window_blocks;
        let last = last.min(geometry.last_window());
        let indices: Vec<u64> = inner.windows.range(first..=last).map(|(i, _)| *i).collect();
        for index in indices {
            let Some(window) = inner.windows.get_mut(&index) else {
                continue;
            };
            match &mut window.state {
                State::InFlight { stale, .. } => *stale = true,
                State::Ready(_) | State::Failed(_) => {
                    if let Some(Window {
                        state: State::Ready(bytes),
                        ..
                    }) = inner.windows.remove(&index)
                    {
                        inner.release(bytes);
                    }
                }
            }
        }
        drop(inner);
        self.ready.notify_all();
    }

    #[cfg(test)]
    pub(crate) fn seed_ready(&self, geometry: Geometry, index: u64, bytes: Vec<u8>) {
        let mut inner = self.lock();
        inner.bind(geometry);
        let stamp = inner.next_stamp();
        inner.windows.insert(
            index,
            Window {
                state: State::Ready(Arc::new(bytes)),
                ahead: false,
                stamp,
                submitted: Instant::now(),
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn has_ready(&self, index: u64) -> bool {
        matches!(
            self.lock().windows.get(&index).map(|w| &w.state),
            Some(State::Ready(_))
        )
    }

    /// Drops every cached window and forgets every stream.
    pub(crate) fn clear(&self) {
        self.lock().reset();
        self.ready.notify_all();
    }

    /// Blocks the pump while nothing is in flight.
    pub(crate) fn park_until_work(&self, limit: Duration) {
        let inner = self.lock();
        if inner.in_flight == 0 {
            let _ = self.work.wait_timeout(inner, limit);
        }
    }

    /// Collects every completed in-flight window and wakes its readers.
    pub(crate) fn pump_once<T: Transport + ?Sized>(
        &self,
        transport: &T,
        scratch: &mut PumpScratch,
    ) -> PumpStatus {
        debug_assert!(scratch.batch.is_empty() && scratch.expired.is_empty());
        {
            let mut inner = self.lock();
            if inner.in_flight == 0 {
                return PumpStatus { in_flight: 0, progressed: 0 };
            }
            let now = Instant::now();
            for (index, window) in inner.windows.iter_mut() {
                let State::InFlight { ticket, buf, .. } = &mut window.state else {
                    continue;
                };
                if now.saturating_duration_since(window.submitted) > COMPLETION_TIMEOUT {
                    scratch.expired.push((*index, *ticket));
                } else if let Some(buf) = buf.take() {
                    scratch.batch.push(Collection {
                        index: *index,
                        ticket: *ticket,
                        buf,
                        result: Ok(false),
                    });
                }
            }
        }
        for (_, ticket) in &scratch.expired {
            transport.timed_out(*ticket);
        }
        for collection in &mut scratch.batch {
            collection.result = transport.try_collect(collection.ticket, &mut collection.buf);
        }

        let mut progressed = 0;
        let mut inner = self.lock();
        for (index, _) in scratch.expired.drain(..) {
            if let Some(Window {
                state: State::InFlight { buf, .. },
                ..
            }) = inner.windows.remove(&index)
            {
                inner.in_flight = inner.in_flight.saturating_sub(1);
                if let Some(buf) = buf {
                    inner.recycle(buf);
                }
                progressed += 1;
            }
        }
        for Collection { index, ticket, buf, result } in scratch.batch.drain(..) {
            let stamp = inner.next_stamp();
            let Some(window) = inner.windows.get_mut(&index) else {
                // Removed while the pump held the buffer: no reader owns it.
                if !matches!(result, Ok(true) | Err(_)) {
                    transport.cancel(ticket);
                }
                inner.recycle(buf);
                continue;
            };
            let State::InFlight {
                stale, buf: slot, ..
            } = &mut window.state
            else {
                inner.recycle(buf);
                continue;
            };
            let stale = *stale;
            match result {
                Ok(false) => {
                    if stale {
                        // Nobody can use this fetch once it lands; free the slot.
                        transport.cancel(ticket);
                        inner.windows.remove(&index);
                        inner.in_flight = inner.in_flight.saturating_sub(1);
                        inner.recycle(buf);
                        progressed += 1;
                    } else {
                        *slot = Some(buf);
                    }
                }
                Ok(true) => {
                    inner.in_flight = inner.in_flight.saturating_sub(1);
                    progressed += 1;
                    if stale {
                        inner.windows.remove(&index);
                        inner.recycle(buf);
                    } else if let Some(window) = inner.windows.get_mut(&index) {
                        window.state = State::Ready(Arc::new(buf));
                        window.stamp = stamp;
                    }
                }
                Err(errno) => {
                    inner.in_flight = inner.in_flight.saturating_sub(1);
                    progressed += 1;
                    inner.recycle(buf);
                    if stale {
                        inner.windows.remove(&index);
                    } else if let Some(window) = inner.windows.get_mut(&index) {
                        window.state = State::Failed(errno);
                    }
                }
            }
        }
        let status = PumpStatus {
            in_flight: inner.in_flight,
            progressed,
        };
        drop(inner);
        if progressed != 0 {
            // Freed in-flight capacity may let a starved stream read ahead.
            self.topup_hint.store(true, Ordering::Relaxed);
            self.ready.notify_all();
        }
        status
    }

    fn report_if_due(&self) {
        let reads = self.stats.reads.load(Ordering::Relaxed);
        if reads == 0 || reads % REPORT_INTERVAL_READS != 0 {
            return;
        }
        let (streams, used, wasted, cap) = {
            let inner = self.lock();
            (
                inner.streams.len(),
                inner.ahead_used_total,
                inner.ahead_wasted_total,
                inner.depth_cap,
            )
        };
        eprintln!(
            "storaged: read engine reads={reads} waited={} stall_us={} demand_windows={} ahead_windows={} in_flight_peak={} streams={streams} ahead_used={used} ahead_wasted={wasted} depth_cap={cap}",
            self.stats.waited.load(Ordering::Relaxed),
            self.stats.stall_ns.load(Ordering::Relaxed) / 1_000,
            self.stats.demand_windows.load(Ordering::Relaxed),
            self.stats.ahead_windows.load(Ordering::Relaxed),
            self.stats.in_flight_peak.load(Ordering::Relaxed),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const BLOCK: usize = 4096;
    const WINDOW_BLOCKS: u64 = (WINDOW_BYTES / BLOCK) as u64;

    /// In-memory transport. Every submit completes on the next `try_collect`
    /// unless `hold` is set, and the data of block `b` is `b as u8` repeated.
    #[derive(Default)]
    struct Fake {
        state: Mutex<FakeState>,
        hold: AtomicBool,
    }

    #[derive(Default)]
    struct FakeState {
        next: u64,
        submitted: Vec<(u64, u64)>,
        tickets: BTreeMap<u64, (u64, u64)>,
        cancelled: Vec<u64>,
        fail_collect: Option<i32>,
        timed_out: Vec<u64>,
    }

    impl Fake {
        fn submitted(&self) -> Vec<(u64, u64)> {
            self.state.lock().unwrap().submitted.clone()
        }

        /// How many times the window starting at block `lba` was fetched.
        fn fetches_of(&self, lba: u64) -> usize {
            self.submitted()
                .iter()
                .filter(|(start, _)| *start == lba)
                .count()
        }
    }

    impl Transport for Fake {
        fn submit_read(
            &self,
            generation: u64,
            lba: u64,
            block_count: u64,
            byte_len: usize,
        ) -> Result<DvmBlockTicketWire, i32> {
            assert_eq!(byte_len, block_count as usize * BLOCK);
            let mut state = self.state.lock().unwrap();
            state.next += 1;
            let id = state.next;
            state.submitted.push((lba, block_count));
            state.tickets.insert(id, (lba, block_count));
            Ok(DvmBlockTicketWire {
                generation,
                request_id: id,
                data_slot: (id % 64) as u32,
                ..DvmBlockTicketWire::default()
            })
        }

        fn try_collect(&self, ticket: DvmBlockTicketWire, buf: &mut [u8]) -> Result<bool, i32> {
            if self.hold.load(Ordering::Relaxed) {
                return Ok(false);
            }
            let mut state = self.state.lock().unwrap();
            if let Some(errno) = state.fail_collect.take() {
                state.tickets.remove(&ticket.request_id);
                return Err(errno);
            }
            let (lba, count) = state.tickets.remove(&ticket.request_id).ok_or(libc::EIO)?;
            for block in 0..count as usize {
                buf[block * BLOCK..(block + 1) * BLOCK].fill((lba as usize + block) as u8);
            }
            Ok(true)
        }

        fn cancel(&self, ticket: DvmBlockTicketWire) {
            let mut state = self.state.lock().unwrap();
            state.tickets.remove(&ticket.request_id);
            state.cancelled.push(ticket.request_id);
        }

        fn timed_out(&self, ticket: DvmBlockTicketWire) {
            self.state.lock().unwrap().timed_out.push(ticket.request_id);
        }
    }

    fn geometry(capacity_blocks: u64) -> Geometry {
        Geometry::new(7, BLOCK, capacity_blocks).unwrap()
    }

    /// Runs `body` while a pump thread collects completions.
    fn with_pump<R>(engine: &Engine, fake: &Fake, body: impl FnOnce() -> R) -> R {
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut scratch = PumpScratch::default();
                while !stop.load(Ordering::Relaxed) {
                    let status = engine.pump_once(fake, &mut scratch);
                    if status.in_flight == 0 {
                        engine.park_until_work(Duration::from_millis(2));
                    } else if status.progressed == 0 {
                        std::thread::sleep(Duration::from_micros(200));
                    }
                }
            });
            let result = body();
            stop.store(true, Ordering::Relaxed);
            result
        })
    }

    fn expect_data(out: &[u8], lba: u64, blocks: u64) {
        for block in 0..blocks as usize {
            assert!(
                out[block * BLOCK..(block + 1) * BLOCK]
                    .iter()
                    .all(|b| *b == (lba as usize + block) as u8),
                "block {} content",
                lba as usize + block
            );
        }
    }

    #[test]
    fn cold_read_fetches_one_window_and_the_next_read_hits() {
        let engine = Engine::new();
        let fake = Fake::default();
        let geo = geometry(1024);
        with_pump(&engine, &fake, || {
            let mut out = vec![0_u8; 3 * BLOCK];
            assert_eq!(engine.read(&fake, geo, 5, 3, &mut out), Ok(3 * BLOCK));
            expect_data(&out, 5, 3);
            assert_eq!(engine.read(&fake, geo, 6, 1, &mut out[..BLOCK]), Ok(BLOCK));
            expect_data(&out[..BLOCK], 6, 1);
        });
        assert_eq!(fake.submitted(), vec![(0, WINDOW_BLOCKS)]);
    }

    #[test]
    fn recycled_buffer_is_not_published_until_its_complete_successful_collect() {
        let engine = Engine::new();
        let fake = Fake::default();
        let geo = geometry(4 * WINDOW_BLOCKS);
        {
            let mut inner = engine.lock();
            inner.bind(geo);
            inner.recycle(vec![0xcc; WINDOW_BYTES]);
            engine.submit(&mut inner, &fake, 0, true).unwrap();
        }
        fake.hold.store(true, Ordering::Relaxed);
        let mut scratch = PumpScratch::default();
        assert_eq!(engine.pump_once(&fake, &mut scratch).progressed, 0);
        assert!(!engine.has_ready(0));
        assert!(scratch.batch.is_empty() && scratch.expired.is_empty());
        let capacity = scratch.batch.capacity();
        fake.hold.store(false, Ordering::Relaxed);
        assert_eq!(engine.pump_once(&fake, &mut scratch).progressed, 1);
        let mut out = vec![0xff; BLOCK];
        assert_eq!(engine.read(&fake, geo, 3, 1, &mut out), Ok(BLOCK));
        expect_data(&out, 3, 1);

        engine.invalidate_range(geo.generation, 0, 1);
        {
            let mut inner = engine.lock();
            engine.submit(&mut inner, &fake, 1, true).unwrap();
        }
        fake.state.lock().unwrap().fail_collect = Some(libc::EIO);
        assert_eq!(engine.pump_once(&fake, &mut scratch).progressed, 1);
        out.fill(0xa5);
        assert_eq!(engine.read(&fake, geo, WINDOW_BLOCKS, 1, &mut out), Err(libc::EIO));
        assert!(out.iter().all(|&byte| byte == 0xa5));
        assert!(scratch.batch.is_empty() && scratch.expired.is_empty());
        assert_eq!(scratch.batch.capacity(), capacity);
    }

    #[test]
    fn read_pins_two_windows_inline_and_preserves_larger_read_order() {
        let engine = Engine::new();
        let geo = geometry(4 * WINDOW_BLOCKS);
        for index in 0..3 {
            engine.seed_ready(geo, index, vec![index as u8; WINDOW_BYTES]);
        }
        let mut inner = engine.lock();
        let two = engine.pin_segments(
            &mut inner, geo, 0, WINDOW_BLOCKS - 1, WINDOW_BLOCKS + 1, 0, 1,
        ).unwrap();
        assert_eq!(two.len(), 2);
        assert_eq!(two.overflow.capacity(), 0);
        let parts: Vec<_> = two.iter().map(|segment| (segment.src, segment.dst, segment.len)).collect();
        assert_eq!(parts, [(WINDOW_BYTES - BLOCK, 0, BLOCK), (0, BLOCK, BLOCK)]);
        let three = engine.pin_segments(
            &mut inner, geo, 0, 0, 3 * WINDOW_BLOCKS, 0, 2,
        ).unwrap();
        assert_eq!(three.len(), 3);
        assert_eq!(three.overflow.len(), 1);
        for (index, segment) in three.iter().enumerate() {
            assert_eq!(segment.dst, index * WINDOW_BYTES);
            assert_eq!(segment.len, WINDOW_BYTES);
            assert!(segment.bytes.iter().all(|&byte| byte == index as u8));
        }
    }

    #[test]
    fn read_spanning_windows_assembles_bytes_at_the_right_offsets() {
        let engine = Engine::new();
        let fake = Fake::default();
        let geo = geometry(1024);
        with_pump(&engine, &fake, || {
            let start = WINDOW_BLOCKS - 3;
            let mut out = vec![0_u8; 8 * BLOCK];
            assert_eq!(engine.read(&fake, geo, start, 8, &mut out), Ok(8 * BLOCK));
            expect_data(&out, start, 8);
        });
        let windows: BTreeSet<u64> = fake.submitted().iter().map(|(lba, _)| *lba).collect();
        assert!(windows.contains(&0) && windows.contains(&WINDOW_BLOCKS));
    }

    #[test]
    fn last_window_is_short_at_the_device_end() {
        let engine = Engine::new();
        let fake = Fake::default();
        let capacity = WINDOW_BLOCKS + 5;
        let geo = geometry(capacity);
        with_pump(&engine, &fake, || {
            let mut out = vec![0_u8; 5 * BLOCK];
            assert_eq!(
                engine.read(&fake, geo, WINDOW_BLOCKS, 5, &mut out),
                Ok(5 * BLOCK)
            );
            expect_data(&out, WINDOW_BLOCKS, 5);
            assert_eq!(
                engine.read(&fake, geo, capacity - 1, 2, &mut out),
                Err(libc::EINVAL),
                "read past the device end is rejected"
            );
        });
        assert!(fake.submitted().contains(&(WINDOW_BLOCKS, 5)));
    }

    #[test]
    fn sequential_stream_reads_ahead_and_never_fetches_a_window_twice() {
        let engine = Engine::new();
        let fake = Fake::default();
        let geo = geometry(64 * WINDOW_BLOCKS);
        with_pump(&engine, &fake, || {
            let mut out = vec![0_u8; 13 * BLOCK];
            let mut lba = 0;
            for _ in 0..40 {
                assert_eq!(engine.read(&fake, geo, lba, 13, &mut out), Ok(13 * BLOCK));
                expect_data(&out, lba, 13);
                engine.top_up(&fake);
                lba += 13;
            }
        });
        let submitted = fake.submitted();
        let unique: BTreeSet<_> = submitted.iter().collect();
        assert_eq!(unique.len(), submitted.len(), "each window fetched once");
        let read_windows = (40 * 13) / WINDOW_BLOCKS + 1;
        assert!(
            submitted.len() as u64 > read_windows,
            "the stream must have read ahead of what it consumed"
        );
        assert!(submitted.len() as u64 <= read_windows + MAX_DEPTH + 1);
    }

    #[test]
    fn reads_that_skip_windows_ramp_the_stream_down_to_no_read_ahead() {
        let engine = Engine::new();
        let fake = Fake::default();
        let geo = geometry(512 * WINDOW_BLOCKS);
        let mut last_read_window = 0;
        with_pump(&engine, &fake, || {
            let mut out = vec![0_u8; BLOCK];
            // Sequential phase: the stream ramps up to its full depth.
            for window in 0..8_u64 {
                engine
                    .read(&fake, geo, window * WINDOW_BLOCKS, 1, &mut out)
                    .expect("sequential read");
                engine.top_up(&fake);
            }
            // Strided phase: every read skips one window, as a process that
            // faults every other page range of a mapped file does.
            for step in 0..40_u64 {
                let window = 8 + step * 2 + 1;
                engine
                    .read(&fake, geo, window * WINDOW_BLOCKS, 1, &mut out)
                    .expect("strided read");
                engine.top_up(&fake);
                last_read_window = window;
            }
        });
        let farthest = fake
            .submitted()
            .iter()
            .map(|(lba, _)| lba / WINDOW_BLOCKS)
            .max()
            .unwrap();
        assert!(
            farthest <= last_read_window + 8,
            "a skipping stream kept reading {} windows ahead",
            farthest - last_read_window
        );
    }

    #[test]
    fn wasted_read_ahead_lowers_the_depth_cap_and_used_read_ahead_restores_it() {
        let mut inner = Inner::new();
        assert_eq!(inner.depth_cap, MAX_DEPTH);
        for _ in 0..FEEDBACK_EPOCH {
            inner.resolve_ahead(0, 1);
        }
        assert_eq!(inner.depth_cap, MAX_DEPTH / 2);
        for _ in 0..8 {
            for _ in 0..FEEDBACK_EPOCH {
                inner.resolve_ahead(0, 1);
            }
        }
        assert_eq!(inner.depth_cap, INITIAL_DEPTH, "never below the initial depth");
        for _ in 0..8 {
            for _ in 0..FEEDBACK_EPOCH {
                inner.resolve_ahead(1, 0);
            }
        }
        assert_eq!(inner.depth_cap, MAX_DEPTH);
        // A mixed epoch (a third wasted) holds the cap where it is.
        inner.depth_cap = 10;
        for index in 0..FEEDBACK_EPOCH {
            if index % 3 == 0 {
                inner.resolve_ahead(0, 1);
            } else {
                inner.resolve_ahead(1, 0);
            }
        }
        assert_eq!(inner.depth_cap, 10);
        assert_eq!(
            inner.ahead_used_total + inner.ahead_wasted_total,
            18 * FEEDBACK_EPOCH
        );
    }

    #[test]
    fn random_reads_do_not_read_ahead() {
        let engine = Engine::new();
        let fake = Fake::default();
        let geo = geometry(64 * WINDOW_BLOCKS);
        with_pump(&engine, &fake, || {
            let mut out = vec![0_u8; BLOCK];
            for window in [30_u64, 3, 51, 17, 40] {
                let lba = window * WINDOW_BLOCKS + 1;
                assert_eq!(engine.read(&fake, geo, lba, 1, &mut out), Ok(BLOCK));
                engine.top_up(&fake);
            }
        });
        assert_eq!(fake.submitted().len(), 5);
    }

    #[test]
    fn interleaved_sequential_streams_are_tracked_separately() {
        let engine = Engine::new();
        let fake = Fake::default();
        let geo = geometry(256 * WINDOW_BLOCKS);
        let bases = [0_u64, 64 * WINDOW_BLOCKS, 128 * WINDOW_BLOCKS];
        with_pump(&engine, &fake, || {
            let mut out = vec![0_u8; 13 * BLOCK];
            for step in 0..30 {
                for base in bases {
                    let lba = base + step * 13;
                    assert_eq!(engine.read(&fake, geo, lba, 13, &mut out), Ok(13 * BLOCK));
                    expect_data(&out, lba, 13);
                    engine.top_up(&fake);
                }
            }
        });
        let submitted = fake.submitted();
        for base in bases {
            let ahead = submitted
                .iter()
                .filter(|(lba, _)| *lba >= base && *lba < base + 64 * WINDOW_BLOCKS)
                .count() as u64;
            assert!(
                ahead > (30 * 13) / WINDOW_BLOCKS + 1,
                "stream at {base} must read ahead independently"
            );
        }
    }

    #[test]
    fn concurrent_readers_of_one_window_share_one_fetch() {
        let engine = Engine::new();
        let fake = Fake::default();
        let geo = geometry(1024);
        fake.hold.store(true, Ordering::Relaxed);
        with_pump(&engine, &fake, || {
            std::thread::scope(|scope| {
                let readers: Vec<_> = (0..4)
                    .map(|i| {
                        let (engine, fake) = (&engine, &fake);
                        scope.spawn(move || {
                            let mut out = vec![0_u8; BLOCK];
                            engine.read(fake, geo, i, 1, &mut out).map(|_| out)
                        })
                    })
                    .collect();
                std::thread::sleep(Duration::from_millis(30));
                fake.hold.store(false, Ordering::Relaxed);
                for (i, reader) in readers.into_iter().enumerate() {
                    let out = reader.join().unwrap().expect("read");
                    expect_data(&out, i as u64, 1);
                }
            });
        });
        assert_eq!(fake.submitted(), vec![(0, WINDOW_BLOCKS)]);
    }

    #[test]
    fn write_invalidation_drops_cached_and_in_flight_windows() {
        let engine = Engine::new();
        let fake = Fake::default();
        let geo = geometry(1024);
        with_pump(&engine, &fake, || {
            let mut out = vec![0_u8; BLOCK];
            engine.read(&fake, geo, 2, 1, &mut out).unwrap();
            engine.invalidate_range(7, 2, 1);
            engine.read(&fake, geo, 2, 1, &mut out).unwrap();
        });
        assert_eq!(fake.fetches_of(0), 2, "cached window refetched");

        let engine = Engine::new();
        let fake = Fake::default();
        fake.hold.store(true, Ordering::Relaxed);
        with_pump(&engine, &fake, || {
            std::thread::scope(|scope| {
                let reader = scope.spawn(|| {
                    let mut out = vec![0_u8; BLOCK];
                    engine.read(&fake, geo, 2, 1, &mut out).map(|_| out)
                });
                std::thread::sleep(Duration::from_millis(20));
                // The write lands while the read is in flight: its bytes are old.
                engine.invalidate_range(7, 0, WINDOW_BLOCKS);
                fake.hold.store(false, Ordering::Relaxed);
                let out = reader.join().unwrap().expect("read after stale refetch");
                expect_data(&out, 2, 1);
            });
        });
        assert_eq!(fake.fetches_of(0), 2, "stale fetch dropped, then refetched");
    }

    #[test]
    fn pinned_window_survives_invalidation_without_being_recycled() {
        let engine = Engine::new();
        let geo = geometry(4 * WINDOW_BLOCKS);
        engine.seed_ready(geo, 0, vec![0x5a; WINDOW_BYTES]);
        let segments = {
            let mut inner = engine.lock();
            engine
                .pin_segments(&mut inner, geo, 0, 0, 2, 0, 0)
                .expect("ready window pins")
        };
        engine.invalidate_range(geo.generation, 0, 1);
        assert!(!engine.has_ready(0));
        {
            let mut inner = engine.lock();
            assert!(
                inner.free_buffers.is_empty(),
                "a pinned buffer must not reach the free list"
            );
            // A new fetch taking a buffer must not be handed the pinned one.
            let mut fresh = inner.take_buffer(WINDOW_BYTES);
            fresh.fill(0xff);
        }
        assert_eq!(segments.len(), 1);
        assert!(segments.iter().next().unwrap().bytes[..2 * BLOCK].iter().all(|b| *b == 0x5a));
        // Once the last reader lets go the buffer is recyclable again.
        drop(segments);
        engine.seed_ready(geo, 1, vec![1; WINDOW_BYTES]);
        engine.invalidate_range(geo.generation, WINDOW_BLOCKS, 1);
        assert_eq!(engine.lock().free_buffers.len(), 1);
    }

    #[test]
    fn pinned_window_survives_lru_eviction() {
        let engine = Engine::new();
        let geo = geometry((WINDOW_LIMIT as u64 + 2) * WINDOW_BLOCKS);
        engine.seed_ready(geo, 0, vec![0x77; WINDOW_BYTES]);
        let segments = {
            let mut inner = engine.lock();
            engine
                .pin_segments(&mut inner, geo, 0, 0, 1, 0, 0)
                .expect("ready window pins")
        };
        for index in 1..WINDOW_LIMIT as u64 {
            engine.seed_ready(geo, index, vec![0; 1]);
        }
        {
            let mut inner = engine.lock();
            // The pinned window is the least recently used: it is the victim.
            assert!(inner.make_room(true));
            assert!(!inner.windows.contains_key(&0));
            assert!(inner.free_buffers.is_empty());
        }
        assert!(segments.iter().next().unwrap().bytes.iter().all(|b| *b == 0x77));
    }

    #[test]
    fn concurrent_readers_copy_while_windows_are_invalidated() {
        let engine = Engine::new();
        let fake = Fake::default();
        let geo = geometry(64 * WINDOW_BLOCKS);
        let stop = AtomicBool::new(false);
        with_pump(&engine, &fake, || {
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        engine.invalidate_range(geo.generation, 0, 8 * WINDOW_BLOCKS);
                        std::thread::sleep(Duration::from_micros(100));
                    }
                });
                let mut handles = Vec::new();
                for _ in 0..3 {
                    handles.push(scope.spawn(|| {
                        let mut out = vec![0_u8; 16 * BLOCK];
                        for round in 0..64_u64 {
                            let lba = (round % 8) * WINDOW_BLOCKS + 3;
                            assert_eq!(engine.read(&fake, geo, lba, 16, &mut out), Ok(16 * BLOCK));
                            expect_data(&out, lba, 16);
                        }
                    }));
                }
                for handle in handles {
                    handle.join().unwrap();
                }
                stop.store(true, Ordering::Relaxed);
            });
        });
    }

    #[test]
    fn transport_error_fails_the_reader_and_is_not_cached() {
        let engine = Engine::new();
        let fake = Fake::default();
        let geo = geometry(1024);
        fake.state.lock().unwrap().fail_collect = Some(libc::EIO);
        with_pump(&engine, &fake, || {
            let mut out = vec![0_u8; BLOCK];
            assert_eq!(engine.read(&fake, geo, 1, 1, &mut out), Err(libc::EIO));
            assert_eq!(engine.read(&fake, geo, 1, 1, &mut out), Ok(BLOCK));
            expect_data(&out, 1, 1);
        });
        assert_eq!(fake.fetches_of(0), 2);
    }

    #[test]
    fn generation_change_drops_the_cache() {
        let engine = Engine::new();
        let fake = Fake::default();
        with_pump(&engine, &fake, || {
            let mut out = vec![0_u8; BLOCK];
            engine.read(&fake, geometry(1024), 0, 1, &mut out).unwrap();
            let next = Geometry::new(8, BLOCK, 1024).unwrap();
            engine.read(&fake, next, 0, 1, &mut out).unwrap();
        });
        assert_eq!(fake.submitted().len(), 2);
    }

    #[test]
    fn streamed_windows_are_evicted_before_the_working_set() {
        let engine = Engine::new();
        let fake = Fake::default();
        let total = (WINDOW_LIMIT as u64 + 40) * WINDOW_BLOCKS;
        let geo = geometry(total);
        with_pump(&engine, &fake, || {
            let mut out = vec![0_u8; BLOCK];
            // A metadata block read once, far from the stream.
            let meta = total - 1;
            engine.read(&fake, geo, meta, 1, &mut out).unwrap();
            let mut chunk = vec![0_u8; 16 * BLOCK];
            let mut lba = 0;
            while lba + 16 <= (WINDOW_LIMIT as u64 + 20) * WINDOW_BLOCKS {
                engine.read(&fake, geo, lba, 16, &mut chunk).unwrap();
                engine.top_up(&fake);
                lba += 16;
            }
            let before = fake.submitted().len();
            engine.read(&fake, geo, meta, 1, &mut out).unwrap();
            assert_eq!(
                fake.submitted().len(),
                before,
                "working set survived the stream"
            );
        });
        let inner = engine.lock();
        assert!(inner.windows.len() <= WINDOW_LIMIT + IN_FLIGHT_LIMIT);
    }

    #[test]
    fn expired_ticket_is_reported_and_its_window_dropped() {
        let engine = Engine::new();
        let fake = Fake::default();
        let geo = geometry(1024);
        fake.hold.store(true, Ordering::Relaxed);
        {
            let mut inner = engine.lock();
            inner.bind(geo);
            engine.submit(&mut inner, &fake, 0, true).unwrap();
            let window = inner.windows.get_mut(&0).unwrap();
            window.submitted = Instant::now() - COMPLETION_TIMEOUT - Duration::from_secs(1);
        }
        let status = engine.pump_once(&fake, &mut PumpScratch::default());
        assert_eq!(status.in_flight, 0);
        assert_eq!(fake.state.lock().unwrap().timed_out.len(), 1);
        assert!(engine.lock().windows.is_empty());
    }
}
