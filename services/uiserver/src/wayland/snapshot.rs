//! Off-thread, budgeted wl_shm snapshots.
//!
//! A wl_shm pool fd is chosen by the client. The compositor never maps it (a
//! truncate would SIGBUS the compositor); it `pread`s owned copies instead, and
//! on RustOS a vfsd-backed file makes that `pread` one service call that can
//! take a whole IPC deadline. Doing it on the UI thread let one slow or hostile
//! client stall input, presentation and every other client. This module moves
//! the read (and the BGRA->u32 conversion) to a small bounded worker pool:
//!
//! * the UI thread only *plans* a commit and later *applies* the finished pixels
//!   (a memcpy), so it never waits on a client fd;
//! * [`FairQueue`] enforces per-client and global in-flight byte budgets, at
//!   most one running job per client (a stuck client pins one worker, never
//!   the pool), FIFO fairness across clients, per-job age expiry and
//!   cancellation;
//! * a job holds an `Arc<OwnedFd>`, so closing the pool cannot recycle the fd
//!   number under a running `pread`.
//!
//! Prior art and invariants: `docs/ai/ui-startup-readiness.md`.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::*;

/// Largest in-flight snapshot bytes one client may hold. Two maximum-size
/// buffers (`MAX_WAYLAND_SHM_POOL_BYTES` each).
pub(super) const SNAPSHOT_CLIENT_BYTES: usize = 2 * MAX_WAYLAND_SHM_POOL_BYTES;
/// In-flight snapshot bytes across all clients.
pub(super) const SNAPSHOT_TOTAL_BYTES: usize = 4 * MAX_WAYLAND_SHM_POOL_BYTES;
/// Jobs one client may have queued or running. A client commits at most once
/// per surface per snapshot (later commits coalesce), so this bounds surfaces.
pub(super) const SNAPSHOT_CLIENT_JOBS: usize = 16;
/// Workers are spawned on demand up to this cap. Together with one running job
/// per client, a stalled client pins one worker and `cap` stalled clients are
/// needed to starve the pool.
pub(super) const SNAPSHOT_MAX_WORKERS: usize = 4;
/// Age after which a job is cancelled and its client told the buffer was
/// unreadable. Larger than two worst-case service calls
/// (`IPC_BOOT_CONTROL_HARD_LIMIT_MS`).
pub(super) const SNAPSHOT_JOB_BUDGET: Duration = Duration::from_secs(12);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refused {
    ClientJobs,
    ClientBytes,
    TotalBytes,
}

pub(super) type JobId = u64;

#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    pub(super) client_jobs: usize,
    pub(super) client_bytes: usize,
    pub(super) total_bytes: usize,
}

struct Entry<K, T> {
    id: JobId,
    key: K,
    payload: T,
}

struct Live<K> {
    key: K,
    bytes: usize,
    submitted_at: Duration,
    running: bool,
    cancelled: bool,
}

#[derive(Default)]
struct Load {
    jobs: usize,
    bytes: usize,
    running: usize,
}

/// The scheduling and admission core, independent of threads and clocks.
pub(super) struct FairQueue<K, T> {
    queue: VecDeque<Entry<K, T>>,
    live: HashMap<JobId, Live<K>>,
    clients: HashMap<K, Load>,
    total_bytes: usize,
    next_id: JobId,
    limits: Limits,
}

impl<K: Clone + Eq + Hash, T> FairQueue<K, T> {
    pub(super) fn new(limits: Limits) -> Self {
        Self {
            queue: VecDeque::new(),
            live: HashMap::new(),
            clients: HashMap::new(),
            total_bytes: 0,
            next_id: 1,
            limits,
        }
    }

    pub(super) fn submit(
        &mut self,
        key: K,
        payload: T,
        bytes: usize,
        now: Duration,
    ) -> Result<JobId, Refused> {
        let load = self.clients.get(&key);
        let (jobs, held) = load.map_or((0, 0), |load| (load.jobs, load.bytes));
        if jobs >= self.limits.client_jobs {
            return Err(Refused::ClientJobs);
        }
        if held.saturating_add(bytes) > self.limits.client_bytes {
            return Err(Refused::ClientBytes);
        }
        if self.total_bytes.saturating_add(bytes) > self.limits.total_bytes {
            return Err(Refused::TotalBytes);
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let load = self.clients.entry(key.clone()).or_default();
        load.jobs += 1;
        load.bytes += bytes;
        self.total_bytes += bytes;
        self.live.insert(
            id,
            Live {
                key: key.clone(),
                bytes,
                submitted_at: now,
                running: false,
                cancelled: false,
            },
        );
        self.queue.push_back(Entry { id, key, payload });
        Ok(id)
    }

    /// The oldest queued job whose client has nothing running.
    pub(super) fn next(&mut self) -> Option<(JobId, T)> {
        let position = self.queue.iter().position(|entry| {
            self.clients
                .get(&entry.key)
                .is_none_or(|load| load.running == 0)
        })?;
        let entry = self.queue.remove(position)?;
        if let Some(load) = self.clients.get_mut(&entry.key) {
            load.running += 1;
        }
        if let Some(live) = self.live.get_mut(&entry.id) {
            live.running = true;
        }
        Some((entry.id, entry.payload))
    }

    fn release(&mut self, id: JobId) -> Option<Live<K>> {
        let live = self.live.remove(&id)?;
        self.total_bytes = self.total_bytes.saturating_sub(live.bytes);
        if let Some(load) = self.clients.get_mut(&live.key) {
            load.jobs = load.jobs.saturating_sub(1);
            load.bytes = load.bytes.saturating_sub(live.bytes);
            if live.running {
                load.running = load.running.saturating_sub(1);
            }
            if load.jobs == 0 && load.running == 0 {
                self.clients.remove(&live.key);
            }
        }
        Some(live)
    }

    /// A running job returned. True when its result is still wanted.
    pub(super) fn finish(&mut self, id: JobId) -> bool {
        self.release(id).is_some_and(|live| !live.cancelled)
    }

    /// Cancel a job. A queued job vanishes at once; a running one cannot be
    /// interrupted (the read is a blocking call), so it keeps its budget and its
    /// client's running slot until it returns and its result is then dropped.
    pub(super) fn cancel(&mut self, id: JobId) {
        let running = match self.live.get_mut(&id) {
            Some(live) if live.running => {
                live.cancelled = true;
                return;
            }
            Some(_) => false,
            None => return,
        };
        debug_assert!(!running);
        self.queue.retain(|entry| entry.id != id);
        self.release(id);
    }

    /// Jobs older than `budget` that nobody has cancelled yet.
    pub(super) fn expired(&self, now: Duration, budget: Duration) -> Vec<JobId> {
        self.live
            .iter()
            .filter(|(_, live)| !live.cancelled && now.saturating_sub(live.submitted_at) >= budget)
            .map(|(id, _)| *id)
            .collect()
    }

    pub(super) fn has_queued(&self) -> bool {
        !self.queue.is_empty()
    }

    #[cfg(test)]
    fn held_bytes(&self) -> usize {
        self.total_bytes
    }
}

/// What to read, fully planned on the UI thread and executable anywhere.
pub(super) struct SnapshotSpec {
    pub(super) fd: Arc<OwnedFd>,
    pub(super) read_start: usize,
    pub(super) read_len: usize,
    pub(super) layout: SnapshotLayout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SnapshotLayout {
    Full {
        width: usize,
        height: usize,
        stride: usize,
        has_alpha: bool,
    },
    Damage {
        damage: Rect,
        stride: usize,
        has_alpha: bool,
    },
}

#[derive(Debug)]
pub(super) enum SnapshotPixels {
    Full {
        pixels: Vec<u32>,
        width: usize,
        height: usize,
    },
    /// `rows` is `damage.width * damage.height` pixels, row-major.
    Damage { damage: Rect, rows: Vec<u32> },
}

impl SnapshotSpec {
    /// Read and convert. Pure with respect to compositor state: nothing here
    /// touches a surface, so a failure or a cancellation leaves no trace.
    pub(super) fn run(self) -> io::Result<SnapshotPixels> {
        let bytes = super::shm::read_shm_snapshot(self.fd.as_raw_fd(), self.read_start, self.read_len)?;
        let bad = || io::Error::from(io::ErrorKind::InvalidData);
        match self.layout {
            SnapshotLayout::Full {
                width,
                height,
                stride,
                has_alpha,
            } => {
                let width_bytes = width.checked_mul(4).ok_or_else(bad)?;
                let count = checked_wayland_pixel_count(width, height).ok_or_else(bad)?;
                let mut pixels = Vec::new();
                pixels
                    .try_reserve_exact(count)
                    .map_err(|_| io::Error::from_raw_os_error(libc::ENOMEM))?;
                pixels.resize(count, 0);
                for row in 0..height {
                    let src_start = row.checked_mul(stride).ok_or_else(bad)?;
                    let src_end = src_start.checked_add(width_bytes).ok_or_else(bad)?;
                    let dst_start = row.checked_mul(width).ok_or_else(bad)?;
                    let src = bytes.get(src_start..src_end).ok_or_else(bad)?;
                    let dst = pixels.get_mut(dst_start..dst_start + width).ok_or_else(bad)?;
                    if !copy_wayland_bgra_row(src, dst, has_alpha) {
                        return Err(bad());
                    }
                }
                Ok(SnapshotPixels::Full {
                    pixels,
                    width,
                    height,
                })
            }
            SnapshotLayout::Damage {
                damage,
                stride,
                has_alpha,
            } => {
                let width_bytes = damage.width.checked_mul(4).ok_or_else(bad)?;
                let count = damage.width.checked_mul(damage.height).ok_or_else(bad)?;
                let mut rows = Vec::new();
                rows.try_reserve_exact(count)
                    .map_err(|_| io::Error::from_raw_os_error(libc::ENOMEM))?;
                rows.resize(count, 0);
                for row in 0..damage.height {
                    let src_start = row.checked_mul(stride).ok_or_else(bad)?;
                    let src_end = src_start.checked_add(width_bytes).ok_or_else(bad)?;
                    let dst_start = row * damage.width;
                    let src = bytes.get(src_start..src_end).ok_or_else(bad)?;
                    let dst = rows
                        .get_mut(dst_start..dst_start + damage.width)
                        .ok_or_else(bad)?;
                    if !copy_wayland_bgra_row(src, dst, has_alpha) {
                        return Err(bad());
                    }
                }
                Ok(SnapshotPixels::Damage { damage, rows })
            }
        }
    }
}

/// Copy a finished damage patch into the retained surface pixels. Returns
/// false, leaving `dst` untouched, when the patch does not fit.
pub(super) fn blit_damage(
    dst: &mut Arc<Vec<u32>>,
    dst_width: usize,
    dst_height: usize,
    damage: Rect,
    rows: &[u32],
) -> bool {
    if dst_width.checked_mul(dst_height) != Some(dst.len())
        || damage.x.checked_add(damage.width).is_none_or(|end| end > dst_width)
        || damage.y.checked_add(damage.height).is_none_or(|end| end > dst_height)
        || damage.width.checked_mul(damage.height) != Some(rows.len())
    {
        return false;
    }
    let pixels = Arc::make_mut(dst);
    for row in 0..damage.height {
        let dst_start = (damage.y + row) * dst_width + damage.x;
        pixels[dst_start..dst_start + damage.width]
            .copy_from_slice(&rows[row * damage.width..(row + 1) * damage.width]);
    }
    true
}

pub(super) struct Completion {
    pub(super) id: JobId,
    pub(super) result: io::Result<SnapshotPixels>,
}

struct Shared {
    state: Mutex<ServiceState>,
    work: Condvar,
    epoch: Instant,
}

struct ServiceState {
    queue: FairQueue<ClientId, SnapshotSpec>,
    done: Vec<Completion>,
    workers: usize,
    idle: usize,
    waker: Option<UiWakeSender>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, ServiceState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }
}

pub(super) struct SnapshotService {
    shared: Arc<Shared>,
}

impl SnapshotService {
    pub(super) fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(ServiceState {
                    queue: FairQueue::new(Limits {
                        client_jobs: SNAPSHOT_CLIENT_JOBS,
                        client_bytes: SNAPSHOT_CLIENT_BYTES,
                        total_bytes: SNAPSHOT_TOTAL_BYTES,
                    }),
                    done: Vec::new(),
                    workers: 0,
                    idle: 0,
                    waker: None,
                }),
                work: Condvar::new(),
                epoch: Instant::now(),
            }),
        }
    }

    pub(super) fn set_waker(&self, waker: UiWakeSender) {
        self.shared.lock().waker = Some(waker);
    }

    pub(super) fn submit(&self, client: ClientId, spec: SnapshotSpec) -> Result<JobId, Refused> {
        let bytes = spec.read_len;
        let mut state = self.shared.lock();
        let now = self.shared.now();
        let id = state.queue.submit(client, spec, bytes, now)?;
        let spawn = state.idle == 0 && state.workers < SNAPSHOT_MAX_WORKERS;
        if spawn {
            // Counted before the spawn so concurrent submits cannot overshoot.
            state.workers += 1;
        }
        drop(state);
        if spawn {
            let shared = Arc::clone(&self.shared);
            let spawned = crate::sys::spawn_ui_thread(
                crate::sys::UiThreadRole::Background,
                "uiserver-shm-snapshot",
                move || worker(shared, SnapshotSpec::run),
            );
            if spawned.is_err() {
                // The job stays queued for an existing worker; with none at all
                // it expires and the client is told, rather than hanging.
                self.shared.lock().workers -= 1;
                diag_line("uiserver: shm snapshot worker failed to start");
            }
        }
        self.shared.work.notify_one();
        Ok(id)
    }

    pub(super) fn cancel(&self, id: JobId) {
        self.shared.lock().queue.cancel(id);
    }

    pub(super) fn take_completions(&self) -> Vec<Completion> {
        std::mem::take(&mut self.shared.lock().done)
    }

    pub(super) fn has_completions(&self) -> bool {
        !self.shared.lock().done.is_empty()
    }

    /// Cancel and return every job past its age budget.
    pub(super) fn reap_expired(&self) -> Vec<JobId> {
        let mut state = self.shared.lock();
        let expired = state.queue.expired(self.shared.now(), SNAPSHOT_JOB_BUDGET);
        for id in &expired {
            state.queue.cancel(*id);
        }
        expired
    }

    /// Whether some job may already be past its budget, so the UI loop knows a
    /// protocol turn is owed even with no socket edge.
    pub(super) fn expiry_due(&self) -> bool {
        let state = self.shared.lock();
        !state.queue.expired(self.shared.now(), SNAPSHOT_JOB_BUDGET).is_empty()
    }
}

fn worker(shared: Arc<Shared>, run: impl Fn(SnapshotSpec) -> io::Result<SnapshotPixels>) {
    loop {
        let (id, spec) = {
            let mut state = shared.lock();
            loop {
                if let Some(job) = state.queue.next() {
                    break job;
                }
                state.idle += 1;
                state = shared
                    .work
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
                state.idle -= 1;
            }
        };
        let result = run(spec);
        let mut state = shared.lock();
        let wanted = state.queue.finish(id);
        let waker = if wanted {
            state.done.push(Completion { id, result });
            state.waker.clone()
        } else {
            None
        };
        let more = state.queue.has_queued();
        drop(state);
        if more {
            // Finishing frees this client's running slot; its next job (or
            // another client's) may now be runnable.
            shared.work.notify_all();
        }
        if let Some(waker) = waker {
            waker.signal();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            client_jobs: 3,
            client_bytes: 100,
            total_bytes: 150,
        }
    }

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    #[test]
    fn a_client_runs_one_job_at_a_time_and_others_are_not_blocked() {
        let mut queue = FairQueue::<u32, &str>::new(limits());
        queue.submit(1, "a1", 10, secs(0)).unwrap();
        queue.submit(1, "a2", 10, secs(0)).unwrap();
        queue.submit(2, "b1", 10, secs(0)).unwrap();
        let (first, payload) = queue.next().unwrap();
        assert_eq!(payload, "a1");
        // Client 1 is running, so its second job is skipped and client 2 runs.
        assert_eq!(queue.next().map(|(_, p)| p), Some("b1"));
        assert!(queue.next().is_none(), "a2 waits for a1");
        assert!(queue.finish(first));
        assert_eq!(queue.next().map(|(_, p)| p), Some("a2"));
    }

    #[test]
    fn budgets_refuse_the_job_that_would_exceed_them() {
        let mut queue = FairQueue::<u32, ()>::new(limits());
        queue.submit(1, (), 60, secs(0)).unwrap();
        assert_eq!(queue.submit(1, (), 41, secs(0)), Err(Refused::ClientBytes));
        queue.submit(2, (), 60, secs(0)).unwrap();
        assert_eq!(queue.submit(3, (), 40, secs(0)), Err(Refused::TotalBytes));
        let mut jobs = FairQueue::<u32, ()>::new(limits());
        for _ in 0..3 {
            jobs.submit(1, (), 1, secs(0)).unwrap();
        }
        assert_eq!(jobs.submit(1, (), 1, secs(0)), Err(Refused::ClientJobs));
        assert!(jobs.submit(2, (), 1, secs(0)).is_ok(), "other clients are unaffected");
    }

    #[test]
    fn cancelling_a_queued_job_releases_its_budget_immediately() {
        let mut queue = FairQueue::<u32, ()>::new(limits());
        let id = queue.submit(1, (), 80, secs(0)).unwrap();
        queue.cancel(id);
        assert_eq!(queue.held_bytes(), 0);
        assert!(!queue.has_queued());
        queue.submit(1, (), 80, secs(0)).unwrap();
    }

    #[test]
    fn cancelling_a_running_job_keeps_its_budget_until_the_read_returns() {
        let mut queue = FairQueue::<u32, ()>::new(limits());
        let id = queue.submit(1, (), 80, secs(0)).unwrap();
        let (running, ()) = queue.next().unwrap();
        assert_eq!(running, id);
        queue.cancel(id);
        // The blocked read still owns its buffer and its client's worker slot.
        assert_eq!(queue.held_bytes(), 80);
        assert_eq!(queue.submit(1, (), 30, secs(0)), Err(Refused::ClientBytes));
        queue.submit(1, (), 10, secs(0)).unwrap();
        assert!(queue.next().is_none(), "client 1 still has a running job");
        assert!(!queue.finish(id), "the cancelled result is dropped");
        assert_eq!(queue.held_bytes(), 10);
        assert!(queue.next().is_some());
    }

    #[test]
    fn jobs_expire_by_age_whether_queued_or_running_and_only_once() {
        let mut queue = FairQueue::<u32, ()>::new(limits());
        let running = queue.submit(1, (), 1, secs(0)).unwrap();
        let queued = queue.submit(2, (), 1, secs(1)).unwrap();
        assert_eq!(queue.next().map(|(id, ())| id), Some(running));
        assert!(queue.expired(secs(5), secs(10)).is_empty());
        let mut expired = queue.expired(secs(11), secs(10));
        expired.sort_unstable();
        assert_eq!(expired, vec![running, queued]);
        for id in &expired {
            queue.cancel(*id);
        }
        assert!(
            queue.expired(secs(100), secs(10)).is_empty(),
            "cancelled jobs are not reported again"
        );
    }

    #[test]
    fn blit_damage_is_transactional() {
        let mut dst = Arc::new(vec![0_u32; 4 * 4]);
        let damage = Rect { x: 1, y: 1, width: 2, height: 2 };
        assert!(blit_damage(&mut dst, 4, 4, damage, &[1, 2, 3, 4]));
        assert_eq!(&dst[5..7], &[1, 2]);
        assert_eq!(&dst[9..11], &[3, 4]);
        let before = dst.as_ref().clone();
        let outside = Rect { x: 3, y: 3, width: 2, height: 2 };
        assert!(!blit_damage(&mut dst, 4, 4, outside, &[9, 9, 9, 9]));
        assert!(!blit_damage(&mut dst, 4, 4, damage, &[9, 9, 9]));
        assert_eq!(*dst, before);
    }

    #[test]
    fn a_parked_read_does_not_hold_up_the_pool_or_other_clients() {
        // A read that never returns (a stalled vfsd call) is modelled by a
        // runner that parks on a channel for one particular offset. The "UI
        // thread" (this test) keeps making progress, a second client's snapshot
        // completes meanwhile, and cancelling the parked job drops its result
        // and returns its budget once the read finally returns.
        let display = super::super::tests::test_display();
        let stuck_client = super::super::tests::test_client(&display);
        let good_client = super::super::tests::test_client(&display);
        let (release, parked) = std::sync::mpsc::channel::<()>();
        let parked = Arc::new(Mutex::new(parked));
        let shared = Arc::new(Shared {
            state: Mutex::new(ServiceState {
                queue: FairQueue::new(limits_big()),
                done: Vec::new(),
                workers: 0,
                idle: 0,
                waker: None,
            }),
            work: Condvar::new(),
            epoch: Instant::now(),
        });
        const PARKED_OFFSET: usize = 999;
        for _ in 0..2 {
            let shared = Arc::clone(&shared);
            let parked = Arc::clone(&parked);
            let _ = crate::sys::spawn_ui_thread(crate::sys::UiThreadRole::Background, "test-snapshot-worker", move || {
                worker(shared, move |spec| {
                    if spec.read_start == PARKED_OFFSET {
                        let _ = parked.lock().unwrap().recv();
                    }
                    Ok(SnapshotPixels::Full {
                        pixels: vec![spec.read_start as u32],
                        width: 1,
                        height: 1,
                    })
                })
            });
        }
        let fd = Arc::new(tempfile_with(&[0; 4]));
        let spec = |read_start| SnapshotSpec {
            fd: Arc::clone(&fd),
            read_start,
            read_len: 4,
            layout: SnapshotLayout::Full {
                width: 1,
                height: 1,
                stride: 4,
                has_alpha: true,
            },
        };
        let (stuck, good) = {
            let mut state = shared.lock();
            let stuck = state
                .queue
                .submit(stuck_client, spec(PARKED_OFFSET), 4, Duration::ZERO)
                .unwrap();
            let good = state
                .queue
                .submit(good_client, spec(7), 4, Duration::ZERO)
                .unwrap();
            (stuck, good)
        };
        shared.work.notify_all();
        let started = Instant::now();
        let position = loop {
            if let Some(position) = shared.lock().done.iter().position(|c| c.id == good) {
                break position;
            }
            assert!(
                started.elapsed() < secs(5),
                "healthy client starved by a parked read"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(shared.lock().done.remove(position).result.is_ok());
        assert!(!shared.lock().done.iter().any(|c| c.id == stuck));
        shared.lock().queue.cancel(stuck);
        assert_eq!(shared.lock().queue.held_bytes(), 4, "the parked read still owns its buffer");
        release.send(()).unwrap();
        let released = Instant::now();
        while shared.lock().queue.held_bytes() != 0 {
            assert!(
                released.elapsed() < secs(5),
                "budget not released after the read returned"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            shared.lock().done.iter().all(|c| c.id != stuck),
            "a cancelled job's result is never delivered"
        );
    }

    fn limits_big() -> Limits {
        Limits { client_jobs: 4, client_bytes: 1 << 20, total_bytes: 1 << 20 }
    }

    fn tempfile_with(contents: &[u8]) -> OwnedFd {
        let name = std::ffi::CString::new("snapshot-test").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        let written = unsafe { libc::write(fd, contents.as_ptr().cast(), contents.len()) };
        assert_eq!(written as usize, contents.len());
        unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) }
    }
}
