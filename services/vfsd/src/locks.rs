//! Advisory file-lock table (policy owner: vfsd, Ring3).
//!
//! Pure data structure, no IPC and no clock of its own (`now` is a parameter),
//! so every rule is unit tested. Ring0 never sees a lock; compat only marshals
//! `VFS_IPC_OP_FILE_LOCK`.
//!
//! Two independent families share one table, as on Linux:
//!
//! * record locks: POSIX (`Owner::Process`, owned by the pid) and OFD
//!   (`Owner::Description`, owned by the open description). These conflict with
//!   each other even inside one process, because their owners differ;
//! * `flock(2)` (`Owner::Flock`, owned by the open description), which never
//!   interacts with record locks.
//!
//! Locks are keyed by the file identity `(mount_id, node_id)`. A lock is an
//! inclusive byte range `[start, end]`; `end == u64::MAX` means "to EOF and
//! beyond". Within a family the same owner never conflicts with itself, a
//! new request replaces what the owner had in the range (split/merge), and
//! adjacent or overlapping same-kind ranges of one owner are coalesced.
//!
//! Blocking is cooperative. vfsd never waits: [`LockTable::set_wait`] either
//! grants, reports `EAGAIN` (and remembers the waiter so POSIX deadlock cycles
//! can be seen) or reports `EDEADLK`. A remembered waiter is only evidence for
//! [`WAIT_TTL_NS`]: the compat layer re-attempts every few milliseconds, which
//! refreshes it, and a waiter that stopped (signal, exit) simply ages out, so a
//! stale entry can delay a deadlock verdict but cannot fabricate one for long.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use rustos_user_abi::syscall::{
    LifecycleEventWire, LIFECYCLE_EVENT_EXIT, LIFECYCLE_EVENT_FORK, VfsFileLockWire, VFS_FILE_LOCK_CMD_CANCEL_WAIT, VFS_FILE_LOCK_CMD_GETLK,
    VFS_FILE_LOCK_CMD_RELEASE_FILE, VFS_FILE_LOCK_CMD_SETLKW, VFS_FILE_LOCK_OWNER_FLOCK,
    VFS_FILE_LOCK_OWNER_OFD, VFS_FILE_LOCK_OWNER_POSIX, VFS_FILE_LOCK_TYPE_RDLCK,
    VFS_FILE_LOCK_TYPE_UNLCK, VFS_FILE_LOCK_TYPE_WRLCK, VFS_FILE_LOCK_WHENCE_CUR,
    VFS_FILE_LOCK_WHENCE_END, VFS_FILE_LOCK_WHENCE_SET,
};

pub const EAGAIN: i32 = 11;
pub const EINVAL: i32 = 22;
pub const EDEADLK: i32 = 35;
pub const ENOLCK: i32 = 37;
pub const EOVERFLOW: i32 = 75;

/// Per-file and per-vfsd bounds; exceeding either is `ENOLCK`.
pub const MAX_LOCKS_PER_FILE: usize = 512;
pub const MAX_LOCKS_TOTAL: usize = 8192;
/// Remembered blocked `SETLKW` attempts (older ones are evicted).
pub const MAX_WAITS: usize = 256;
/// How long a blocked attempt counts as "still waiting" without a retry.
pub const WAIT_TTL_NS: u64 = 250_000_000;

pub type FileKey = (u64, u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Owner {
    /// POSIX record lock, owned by the process (pid).
    Process(u64),
    /// OFD record lock, owned by the open description (`remote_id`).
    Description(u64),
    /// `flock(2)`, owned by the open description (`remote_id`).
    Flock(u64),
}

impl Owner {
    fn is_flock(self) -> bool {
        matches!(self, Owner::Flock(_))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum LockKind {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockOp {
    Set(LockKind),
    Unlock,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Lock {
    pub owner: Owner,
    pub kind: LockKind,
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Copy, Debug)]
struct Wait {
    tid: u64,
    pid: u64,
    key: FileKey,
    kind: LockKind,
    start: u64,
    end: u64,
    at_ns: u64,
}

/// Exact identity of a kernel lock mutation. IDs are never reused for a poll.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct LockTransactionId {
    pub pid: u64,
    pub tid: u64,
    pub operation_hi: u64,
    pub operation_lo: u64,
    /// Monotonic kernel issuance sequence, ordered with process close events.
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LockCandidate {
    pub key: FileKey,
    pub owner: Owner,
    pub op: LockOp,
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Debug)]
enum LockTransaction {
    Prepared { candidate: LockCandidate, updated: Vec<Lock>, growth: usize },
    Finished { candidate: Option<LockCandidate>, result: Result<(), i32> },
}

/// Hard bound: admission fails rather than forgetting an unresolved result.
pub const MAX_LOCK_TRANSACTIONS: usize = 8192;

#[derive(Default)]
pub struct LockTable {
    files: BTreeMap<FileKey, Vec<Lock>>,
    total: usize,
    waits: Vec<Wait>,
    transactions: BTreeMap<LockTransactionId, LockTransaction>,
    close_watermarks: BTreeMap<(u64, FileKey), u64>,
}

/// Resolves a `struct flock` range to an inclusive `[start, end]`.
///
/// `base` is the `whence` origin (`0`, the description offset or the file
/// size). `len == 0` extends to EOF, a negative `len` covers
/// `[start+len, start-1]`. A range that begins before byte 0 is `EINVAL`; one
/// that does not fit in `i64` is `EOVERFLOW`.
pub fn resolve_range(base: u64, start: i64, len: i64) -> Result<(u64, u64), i32> {
    let base = i64::try_from(base).map_err(|_| EOVERFLOW)?;
    let abs = base.checked_add(start).ok_or(EOVERFLOW)?;
    if abs < 0 {
        return Err(EINVAL);
    }
    match len {
        0 => Ok((abs as u64, u64::MAX)),
        1.. => {
            let end = abs.checked_add(len - 1).ok_or(EOVERFLOW)?;
            Ok((abs as u64, end as u64))
        }
        _ => {
            let first = abs + len; // abs >= 0 and len < 0: cannot overflow
            if first < 0 {
                return Err(EINVAL);
            }
            // first >= 0 and len < 0 imply abs >= 1.
            Ok((first as u64, (abs - 1) as u64))
        }
    }
}

/// `l_len` for a reported range (`0` = to EOF).
pub fn report_len(start: u64, end: u64) -> i64 {
    if end == u64::MAX {
        0
    } else {
        (end - start + 1).min(i64::MAX as u64) as i64
    }
}

fn overlaps(l: &Lock, start: u64, end: u64) -> bool {
    l.start <= end && start <= l.end
}

fn conflicts(l: &Lock, owner: Owner, kind: LockKind, start: u64, end: u64) -> bool {
    l.owner != owner
        && l.owner.is_flock() == owner.is_flock()
        && overlaps(l, start, end)
        && (l.kind == LockKind::Write || kind == LockKind::Write)
}

/// Replaces `owner`'s coverage of `[start, end]` in `locks` and coalesces.
fn apply(locks: &[Lock], owner: Owner, op: LockOp, start: u64, end: u64) -> Vec<Lock> {
    let mut out: Vec<Lock> = Vec::with_capacity(locks.len() + 2);
    for l in locks {
        if l.owner != owner || !overlaps(l, start, end) {
            out.push(*l);
            continue;
        }
        if l.start < start {
            out.push(Lock { end: start - 1, ..*l });
        }
        if l.end > end {
            out.push(Lock { start: end + 1, ..*l });
        }
    }
    if let LockOp::Set(kind) = op {
        out.push(Lock { owner, kind, start, end });
    }
    coalesce(&mut out);
    out
}

fn coalesce(locks: &mut Vec<Lock>) {
    locks.sort_by_key(|l| (l.owner, l.kind, l.start, l.end));
    let mut merged: Vec<Lock> = Vec::with_capacity(locks.len());
    for l in locks.drain(..) {
        match merged.last_mut() {
            Some(last)
                if last.owner == l.owner
                    && last.kind == l.kind
                    && l.start <= last.end.saturating_add(1) =>
            {
                last.end = last.end.max(l.end);
            }
            _ => merged.push(l),
        }
    }
    merged.sort_by_key(|l| (l.start, l.end, l.owner, l.kind));
    *locks = merged;
}

impl LockTable {
    pub const fn new() -> Self {
        Self { files: BTreeMap::new(), total: 0, waits: Vec::new(), transactions: BTreeMap::new(), close_watermarks: BTreeMap::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    pub fn total_locks(&self) -> usize {
        self.total
    }

    pub fn locks_for(&self, key: FileKey) -> &[Lock] {
        self.files.get(&key).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn wait_count(&self) -> usize {
        self.waits.len()
    }

    /// Reserve a file snapshot without publishing the requested conversion.
    /// Replayed operations return their original outcome, including EAGAIN.
    pub fn prepare_transaction(&mut self, id: LockTransactionId, candidate: LockCandidate) -> Result<(), i32> {
        if let Some(entry) = self.transactions.get(&id) {
            return match entry {
                LockTransaction::Prepared { candidate: old, .. } if *old == candidate => Ok(()),
                LockTransaction::Finished { candidate: Some(old), result } if *old == candidate => *result,
                LockTransaction::Finished { candidate: None, result } => *result,
                _ => Err(EINVAL),
            };
        }
        if id.pid == 0 || id.tid == 0 || id.sequence == 0 || (id.operation_hi == 0 && id.operation_lo == 0) {
            return Err(EINVAL);
        }
        if self.transactions.len() >= MAX_LOCK_TRANSACTIONS {
            return Err(ENOLCK);
        }
        let result = if candidate.owner == Owner::Process(id.pid)
            && self.close_watermarks.get(&(id.pid, candidate.key)).is_some_and(|&close| id.sequence <= close) {
            Err(125)
        } else if candidate.start > candidate.end {
            Err(EINVAL)
        } else if self.transactions.values().any(|entry| matches!(entry,
            LockTransaction::Prepared { candidate: other, .. } if other.key == candidate.key)) {
            Err(EAGAIN)
        } else if matches!(candidate.op, LockOp::Set(kind) if self.test(candidate.key, candidate.owner, kind, candidate.start, candidate.end).is_some()) {
            Err(EAGAIN)
        } else {
            Ok(())
        };
        if let Err(errno) = result {
            self.transactions.insert(id, LockTransaction::Finished { candidate: Some(candidate), result });
            return Err(errno);
        }
        let current = self.files.get(&candidate.key).map(Vec::as_slice).unwrap_or(&[]);
        let updated = apply(current, candidate.owner, candidate.op, candidate.start, candidate.end);
        let growth = updated.len().saturating_sub(current.len());
        let reserved: usize = self.transactions.values().map(|entry| match entry {
            LockTransaction::Prepared { growth, .. } => *growth,
            _ => 0,
        }).sum();
        if updated.len() > MAX_LOCKS_PER_FILE || self.total.saturating_add(reserved).saturating_add(growth) > MAX_LOCKS_TOTAL {
            self.transactions.insert(id, LockTransaction::Finished { candidate: Some(candidate), result: Err(ENOLCK) });
            return Err(ENOLCK);
        }
        self.transactions.insert(id, LockTransaction::Prepared { candidate, updated, growth });
        Ok(())
    }

    /// Commit once; repeated commit returns the exact recorded terminal result.
    pub fn commit_transaction(&mut self, id: LockTransactionId) -> Result<(), i32> {
        let entry = self.transactions.remove(&id).ok_or(EINVAL)?;
        match entry {
            LockTransaction::Prepared { candidate, updated, .. } => {
                let before = self.files.get(&candidate.key).map_or(0, Vec::len);
                self.total = self.total + updated.len() - before;
                if updated.is_empty() { self.files.remove(&candidate.key); }
                else { self.files.insert(candidate.key, updated); }
                self.transactions.insert(id, LockTransaction::Finished { candidate: Some(candidate), result: Ok(()) });
                Ok(())
            }
            terminal @ LockTransaction::Finished { .. } => {
                let LockTransaction::Finished { result, .. } = terminal else { unreachable!() };
                self.transactions.insert(id, terminal);
                result
            }
        }
    }

    /// Fence a late prepare, or cancel a prepared candidate without undoing
    /// existing locks. A committed transaction remains committed.
    pub fn cancel_transaction(&mut self, id: LockTransactionId) -> Result<(), i32> {
        if id.pid == 0 || id.tid == 0 || id.sequence == 0 || (id.operation_hi == 0 && id.operation_lo == 0) { return Err(EINVAL); }
        if let Some(entry) = self.transactions.get_mut(&id) {
            return match entry {
                LockTransaction::Prepared { candidate, .. } => {
                    let candidate = *candidate;
                    *entry = LockTransaction::Finished { candidate: Some(candidate), result: Err(125) };
                    Ok(())
                }
                LockTransaction::Finished { result: Ok(()), .. } => Err(EINVAL),
                LockTransaction::Finished { .. } => Ok(()),
            };
        }
        if self.transactions.len() >= MAX_LOCK_TRANSACTIONS { return Err(ENOLCK); }
        self.transactions.insert(id, LockTransaction::Finished { candidate: None, result: Err(125) });
        Ok(())
    }

    fn cancel_candidates(&mut self, mut affected: impl FnMut(&LockCandidate) -> bool) {
        for entry in self.transactions.values_mut() {
            if let LockTransaction::Prepared { candidate, .. } = entry {
                if affected(candidate) {
                    *entry = LockTransaction::Finished { candidate: Some(*candidate), result: Err(125) };
                }
            }
        }
    }

    /// First lock (lowest start) that would block the described request.
    pub fn test(&self, key: FileKey, owner: Owner, kind: LockKind, start: u64, end: u64) -> Option<Lock> {
        self.files
            .get(&key)?
            .iter()
            .find(|l| conflicts(l, owner, kind, start, end))
            .copied()
    }

    /// Non-blocking take/convert/release. A conflict is `EAGAIN`; growth past
    /// the bounds is `ENOLCK` (an unlock that splits a lock can grow too).
    pub fn set(
        &mut self,
        key: FileKey,
        owner: Owner,
        op: LockOp,
        start: u64,
        end: u64,
    ) -> Result<(), i32> {
        if self.transactions.values().any(|entry| matches!(entry,
            LockTransaction::Prepared { candidate, .. } if candidate.key == key)) {
            return Err(EAGAIN);
        }
        if start > end {
            return Err(EINVAL);
        }
        if let LockOp::Set(kind) = op {
            if self.test(key, owner, kind, start, end).is_some() {
                return Err(EAGAIN);
            }
        }
        let current = self.files.get(&key).map(Vec::as_slice).unwrap_or(&[]);
        let updated = apply(current, owner, op, start, end);
        let before = current.len();
        let reserved: usize = self.transactions.values().map(|entry| match entry {
            LockTransaction::Prepared { growth, .. } => *growth,
            _ => 0,
        }).sum();
        if updated.len() > MAX_LOCKS_PER_FILE
            || self.total.saturating_add(reserved).saturating_add(updated.len().saturating_sub(before)) > MAX_LOCKS_TOTAL
        {
            return Err(ENOLCK);
        }
        self.total = self.total + updated.len() - before;
        if updated.is_empty() {
            self.files.remove(&key);
        } else {
            self.files.insert(key, updated);
        }
        Ok(())
    }

    /// `SETLKW` attempt for thread `tid` of process `pid`. Grants like
    /// [`Self::set`]; on conflict remembers the waiter and returns `EAGAIN`,
    /// or `EDEADLK` when a POSIX owner would wait on a cycle. Only POSIX
    /// (`Owner::Process`) waiters take part in deadlock detection, as on
    /// Linux (OFD and `flock` waits are never reported as deadlocks).
    #[allow(clippy::too_many_arguments)]
    pub fn set_wait(
        &mut self,
        key: FileKey,
        owner: Owner,
        tid: u64,
        op: LockOp,
        start: u64,
        end: u64,
        now_ns: u64,
    ) -> Result<(), i32> {
        match self.set(key, owner, op, start, end) {
            Ok(()) => {
                self.cancel_wait(tid);
                Ok(())
            }
            Err(EAGAIN) => {
                let LockOp::Set(kind) = op else { return Err(EAGAIN) };
                if let Owner::Process(pid) = owner {
                    if self.would_deadlock(pid, key, kind, start, end, now_ns) {
                        self.cancel_wait(tid);
                        return Err(EDEADLK);
                    }
                    self.remember_wait(Wait { tid, pid, key, kind, start, end, at_ns: now_ns }, now_ns);
                }
                Err(EAGAIN)
            }
            Err(other) => Err(other),
        }
    }

    fn remember_wait(&mut self, wait: Wait, now_ns: u64) {
        self.waits.retain(|w| w.tid != wait.tid && Self::fresh(w, now_ns));
        if self.waits.len() >= MAX_WAITS {
            // Oldest first: the table is bounded, detection degrades, safety
            // does not (a missing waiter can only hide a cycle).
            self.waits.remove(0);
        }
        self.waits.push(wait);
    }

    fn fresh(wait: &Wait, now_ns: u64) -> bool {
        now_ns.saturating_sub(wait.at_ns) <= WAIT_TTL_NS
    }

    pub fn cancel_wait(&mut self, tid: u64) {
        self.waits.retain(|w| w.tid != tid);
    }

    /// Would `pid` waiting for `[start, end]` of `key` close a wait-for cycle?
    fn would_deadlock(&self, pid: u64, key: FileKey, kind: LockKind, start: u64, end: u64, now_ns: u64) -> bool {
        let mut seen: Vec<u64> = Vec::new();
        let mut stack = self.posix_blockers(key, pid, kind, start, end);
        while let Some(holder) = stack.pop() {
            if holder == pid {
                return true;
            }
            if seen.contains(&holder) {
                continue;
            }
            seen.push(holder);
            for w in self.waits.iter().filter(|w| w.pid == holder && Self::fresh(w, now_ns)) {
                stack.extend(self.posix_blockers(w.key, holder, w.kind, w.start, w.end));
            }
        }
        false
    }

    /// Pids of POSIX locks that block `pid` on the range (OFD and `flock`
    /// holders end the chain: nothing can be said about what they wait for).
    fn posix_blockers(&self, key: FileKey, pid: u64, kind: LockKind, start: u64, end: u64) -> Vec<u64> {
        let owner = Owner::Process(pid);
        self.files
            .get(&key)
            .into_iter()
            .flatten()
            .filter(|l| conflicts(l, owner, kind, start, end))
            .filter_map(|l| match l.owner {
                Owner::Process(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    fn retain_locks(&mut self, mut keep: impl FnMut(&FileKey, &Lock) -> bool) {
        let mut emptied = Vec::new();
        for (key, locks) in self.files.iter_mut() {
            let before = locks.len();
            locks.retain(|l| keep(key, l));
            self.total -= before - locks.len();
            if locks.is_empty() {
                emptied.push(*key);
            }
        }
        for key in emptied {
            self.files.remove(&key);
        }
    }

    /// Record the close boundary before releasing; an older prepare arriving
    /// after the close cannot recreate the process's locks on this inode.
    pub fn release_process_file_through(&mut self, pid: u64, key: FileKey, sequence: u64) -> Result<(), i32> {
        if pid == 0 || sequence == 0 { return Err(EINVAL); }
        if !self.close_watermarks.contains_key(&(pid, key)) && self.close_watermarks.len() >= MAX_LOCK_TRANSACTIONS {
            return Err(ENOLCK);
        }
        let watermark = self.close_watermarks.entry((pid, key)).or_insert(0);
        if sequence <= *watermark { return Ok(()); }
        *watermark = sequence;
        self.release_process_file(pid, key);
        Ok(())
    }

    /// POSIX: the process closed some fd of `key`, so all of its record locks
    /// on that file go, whichever fd took them.
    pub fn release_process_file(&mut self, pid: u64, key: FileKey) {
        self.cancel_candidates(|candidate| candidate.key == key);
        if let Some(locks) = self.files.get_mut(&key) {
            let before = locks.len();
            locks.retain(|l| l.owner != Owner::Process(pid));
            self.total -= before - locks.len();
            if locks.is_empty() {
                self.files.remove(&key);
            }
        }
    }

    /// Process exit: every POSIX lock and every blocked attempt of `pid`.
    pub fn release_process(&mut self, pid: u64) {
        self.cancel_candidates(|_| true);
        self.waits.retain(|w| w.pid != pid);
        if self.total != 0 {
            self.retain_locks(|_, l| l.owner != Owner::Process(pid));
        }
    }

    /// Applies one lifecycle event. An exit drops the pid's POSIX locks. A
    /// fork names a *new* pid, so any POSIX lock still filed under it belongs
    /// to a dead predecessor whose exit event was lost (queue overflow): drop
    /// it, or the new process would silently inherit the lock as its own and
    /// bypass every conflict. A child never inherits record locks anyway.
    pub fn apply_lifecycle_event(&mut self, event: &LifecycleEventWire) {
        if event.pid == 0 {
            return;
        }
        if event.event == LIFECYCLE_EVENT_EXIT || event.event == LIFECYCLE_EVENT_FORK {
            self.release_process(event.pid);
        }
    }

    /// Final close of an open description: its OFD and `flock` locks.
    pub fn release_description(&mut self, remote_id: u64) {
        self.cancel_candidates(|_| true);
        if self.total != 0 {
            self.retain_locks(|_, l| {
                l.owner != Owner::Description(remote_id) && l.owner != Owner::Flock(remote_id)
            });
        }
    }
}

/// Kernel-stamped facts a `FILE_LOCK` request is executed against.
#[derive(Clone, Copy, Debug)]
pub struct LockContext {
    /// Sender process/thread (never taken from the wire).
    pub pid: u64,
    pub tid: u64,
    /// Open description the request names.
    pub remote_id: u64,
    pub key: FileKey,
    /// Description offset (`SEEK_CUR` base) and live file size (`SEEK_END`).
    pub cursor: u64,
    pub file_len: u64,
    pub now_ns: u64,
}

/// Executes one decoded `VFS_IPC_OP_FILE_LOCK` wire. `Ok(Some(reply))` is a
/// `GETLK` answer to send back; `Err(errno)` is the reply status (`EAGAIN`
/// means "conflict, retry" for the blocking command).
pub fn execute(
    table: &mut LockTable,
    ctx: &LockContext,
    wire: &VfsFileLockWire,
) -> Result<Option<VfsFileLockWire>, i32> {
    if !wire.is_well_formed() || wire.pid != 0 || ctx.pid == 0 {
        return Err(EINVAL);
    }
    match wire.cmd {
        VFS_FILE_LOCK_CMD_CANCEL_WAIT => {
            table.cancel_wait(ctx.tid);
            return Ok(None);
        }
        VFS_FILE_LOCK_CMD_RELEASE_FILE => {
            table.release_process_file(ctx.pid, ctx.key);
            return Ok(None);
        }
        _ => {}
    }
    let (owner, whole_file) = match wire.owner {
        VFS_FILE_LOCK_OWNER_POSIX => (Owner::Process(ctx.pid), false),
        VFS_FILE_LOCK_OWNER_OFD => (Owner::Description(ctx.remote_id), false),
        VFS_FILE_LOCK_OWNER_FLOCK => (Owner::Flock(ctx.remote_id), true),
        _ => return Err(EINVAL),
    };
    let (start, end) = if whole_file {
        (0, u64::MAX)
    } else {
        let base = match wire.whence {
            VFS_FILE_LOCK_WHENCE_SET => 0,
            VFS_FILE_LOCK_WHENCE_CUR => ctx.cursor,
            VFS_FILE_LOCK_WHENCE_END => ctx.file_len,
            _ => return Err(EINVAL),
        };
        resolve_range(base, wire.start, wire.len)?
    };
    let kind = match wire.lock_type {
        VFS_FILE_LOCK_TYPE_RDLCK => Some(LockKind::Read),
        VFS_FILE_LOCK_TYPE_WRLCK => Some(LockKind::Write),
        _ => None,
    };
    if wire.cmd == VFS_FILE_LOCK_CMD_GETLK {
        let kind = kind.ok_or(EINVAL)?;
        let mut reply = *wire;
        match table.test(ctx.key, owner, kind, start, end) {
            Some(found) => {
                reply.lock_type = match found.kind {
                    LockKind::Read => VFS_FILE_LOCK_TYPE_RDLCK,
                    LockKind::Write => VFS_FILE_LOCK_TYPE_WRLCK,
                };
                reply.whence = VFS_FILE_LOCK_WHENCE_SET;
                reply.start = i64::try_from(found.start).unwrap_or(i64::MAX);
                reply.len = report_len(found.start, found.end);
                // An OFD/flock owner has no pid to report (Linux: -1).
                reply.pid = match found.owner {
                    Owner::Process(pid) => i64::try_from(pid).unwrap_or(-1),
                    _ => -1,
                };
            }
            None => reply.lock_type = VFS_FILE_LOCK_TYPE_UNLCK,
        }
        return Ok(Some(reply));
    }
    let op = kind.map_or(LockOp::Unlock, LockOp::Set);
    if wire.cmd == VFS_FILE_LOCK_CMD_SETLKW {
        table.set_wait(ctx.key, owner, ctx.tid, op, start, end, ctx.now_ns)
    } else {
        table.set(ctx.key, owner, op, start, end)
    }
    .map(|()| None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const F: FileKey = (1, 10);
    const G: FileKey = (1, 11);
    const A: Owner = Owner::Process(100);
    const B: Owner = Owner::Process(200);
    const C: Owner = Owner::Process(300);
    const R: LockOp = LockOp::Set(LockKind::Read);
    const W: LockOp = LockOp::Set(LockKind::Write);

    fn ranges(t: &LockTable, key: FileKey, owner: Owner) -> Vec<(LockKind, u64, u64)> {
        t.locks_for(key)
            .iter()
            .filter(|l| l.owner == owner)
            .map(|l| (l.kind, l.start, l.end))
            .collect()
    }

    fn transaction_id(n: u64) -> LockTransactionId {
        LockTransactionId { pid: 100, tid: 101, operation_hi: 1, operation_lo: n, sequence: n }
    }

    fn candidate(owner: Owner, op: LockOp) -> LockCandidate {
        LockCandidate { key: F, owner, op, start: 0, end: 9 }
    }

    #[test]
    fn transaction_replay_and_conversion_preserve_old_ranges_until_commit() {
        let mut table = LockTable::new();
        table.set(F, A, R, 0, 9).unwrap();
        let id = transaction_id(1);
        let requested = candidate(A, W);
        assert_eq!(table.prepare_transaction(id, requested), Ok(()));
        assert_eq!(table.prepare_transaction(id, requested), Ok(()));
        assert_eq!(ranges(&table, F, A), vec![(LockKind::Read, 0, 9)]);
        assert_eq!(table.set(F, B, R, 0, 9), Err(EAGAIN));
        assert_eq!(table.commit_transaction(id), Ok(()));
        assert_eq!(table.commit_transaction(id), Ok(()));
        assert_eq!(ranges(&table, F, A), vec![(LockKind::Write, 0, 9)]);
    }

    #[test]
    fn cancellation_fences_late_prepare_and_any_close_fences_commit() {
        let mut table = LockTable::new();
        let early = transaction_id(1);
        assert_eq!(table.cancel_transaction(early), Ok(()));
        assert_eq!(table.prepare_transaction(early, candidate(A, W)), Err(125));
        table.set(F, A, R, 0, 9).unwrap();
        let id = transaction_id(2);
        assert_eq!(table.prepare_transaction(id, candidate(A, W)), Ok(()));
        table.release_process_file(100, F);
        assert_eq!(table.commit_transaction(id), Err(125));
        assert!(table.locks_for(F).is_empty());
    }

    #[test]
    fn close_watermark_rejects_an_unseen_older_prepare() {
        let mut table = LockTable::new();
        assert_eq!(table.release_process_file_through(100, F, 10), Ok(()));
        assert_eq!(table.prepare_transaction(transaction_id(9), candidate(A, W)), Err(125));
        assert_eq!(table.prepare_transaction(transaction_id(11), candidate(A, W)), Ok(()));
        assert_eq!(table.commit_transaction(transaction_id(11)), Ok(()));
        // Replaying an old close must not release a later lock.
        assert_eq!(table.release_process_file_through(100, F, 10), Ok(()));
        assert_eq!(ranges(&table, F, A), vec![(LockKind::Write, 0, 9)]);
    }

    #[test]
    fn transaction_conflict_result_is_replayed_after_blocker_changes() {
        let mut table = LockTable::new();
        table.set(F, B, W, 0, 9).unwrap();
        let id = transaction_id(1);
        let requested = candidate(A, W);
        assert_eq!(table.prepare_transaction(id, requested), Err(EAGAIN));
        table.release_process_file(200, F);
        assert_eq!(table.prepare_transaction(id, requested), Err(EAGAIN));
        assert_eq!(table.prepare_transaction(transaction_id(2), requested), Ok(()));
    }

    #[test]
    fn range_resolution_follows_flock_rules() {
        assert_eq!(resolve_range(0, 5, 10), Ok((5, 14)));
        assert_eq!(resolve_range(0, 5, 0), Ok((5, u64::MAX)));
        assert_eq!(resolve_range(100, -10, 5), Ok((90, 94)));
        // Negative length covers [start+len, start-1].
        assert_eq!(resolve_range(0, 10, -4), Ok((6, 9)));
        assert_eq!(resolve_range(50, 0, -50), Ok((0, 49)));
        assert_eq!(resolve_range(0, 3, -4), Err(EINVAL));
        assert_eq!(resolve_range(0, -1, 1), Err(EINVAL));
        assert_eq!(resolve_range(0, i64::MAX, 2), Err(EOVERFLOW));
        assert_eq!(resolve_range(u64::MAX, 0, 1), Err(EOVERFLOW));
        assert_eq!(resolve_range(i64::MAX as u64, 1, 1), Err(EOVERFLOW));
        assert_eq!(resolve_range(0, 0, i64::MIN), Err(EINVAL));
        assert_eq!(report_len(5, 14), 10);
        assert_eq!(report_len(5, u64::MAX), 0);
    }

    #[test]
    fn read_locks_share_and_write_locks_exclude() {
        let mut t = LockTable::new();
        t.set(F, A, R, 0, 9).unwrap();
        t.set(F, B, R, 5, 20).unwrap();
        assert_eq!(t.set(F, C, W, 8, 8), Err(EAGAIN));
        assert_eq!(t.set(F, C, W, 21, 30), Ok(()));
        assert_eq!(t.set(F, A, W, 0, 9), Err(EAGAIN), "B still reads 5..=9");
        t.set(F, B, LockOp::Unlock, 0, u64::MAX).unwrap();
        t.set(F, A, W, 0, 9).unwrap();
        assert_eq!(t.set(F, B, R, 9, 9), Err(EAGAIN));
        assert_eq!(t.set(G, B, W, 0, u64::MAX), Ok(()), "other file is independent");
    }

    #[test]
    fn same_owner_never_conflicts_and_converts_in_place() {
        let mut t = LockTable::new();
        t.set(F, A, R, 0, 99).unwrap();
        t.set(F, A, W, 10, 19).unwrap();
        assert_eq!(
            ranges(&t, F, A),
            [(LockKind::Read, 0, 9), (LockKind::Write, 10, 19), (LockKind::Read, 20, 99)]
        );
        t.set(F, A, R, 10, 19).unwrap();
        assert_eq!(ranges(&t, F, A), [(LockKind::Read, 0, 99)], "re-merged");
    }

    #[test]
    fn adjacent_and_overlapping_same_kind_ranges_merge() {
        let mut t = LockTable::new();
        t.set(F, A, W, 0, 9).unwrap();
        t.set(F, A, W, 10, 19).unwrap();
        t.set(F, A, W, 30, 39).unwrap();
        assert_eq!(ranges(&t, F, A), [(LockKind::Write, 0, 19), (LockKind::Write, 30, 39)]);
        t.set(F, A, W, 15, 35).unwrap();
        assert_eq!(ranges(&t, F, A), [(LockKind::Write, 0, 39)]);
        t.set(F, A, W, 40, u64::MAX).unwrap();
        assert_eq!(ranges(&t, F, A), [(LockKind::Write, 0, u64::MAX)]);
        assert_eq!(t.total_locks(), 1);
    }

    #[test]
    fn unlock_splits_trims_and_removes() {
        let mut t = LockTable::new();
        t.set(F, A, W, 0, 99).unwrap();
        t.set(F, A, LockOp::Unlock, 40, 59).unwrap();
        assert_eq!(ranges(&t, F, A), [(LockKind::Write, 0, 39), (LockKind::Write, 60, 99)]);
        t.set(F, A, LockOp::Unlock, 0, 49).unwrap();
        assert_eq!(ranges(&t, F, A), [(LockKind::Write, 60, 99)]);
        t.set(F, A, LockOp::Unlock, 90, u64::MAX).unwrap();
        assert_eq!(ranges(&t, F, A), [(LockKind::Write, 60, 89)]);
        t.set(F, A, LockOp::Unlock, 0, u64::MAX).unwrap();
        assert!(t.is_empty());
        assert!(t.locks_for(F).is_empty());
        // Unlocking what is not held is a no-op, never a conflict.
        t.set(F, A, W, 0, 9).unwrap();
        t.set(F, B, LockOp::Unlock, 0, 9).unwrap();
        assert_eq!(ranges(&t, F, A), [(LockKind::Write, 0, 9)]);
    }

    #[test]
    fn eof_range_conflicts_with_everything_past_start() {
        let mut t = LockTable::new();
        t.set(F, A, W, 100, u64::MAX).unwrap();
        assert_eq!(t.set(F, B, R, 1 << 40, (1 << 40) + 1), Err(EAGAIN));
        assert_eq!(t.set(F, B, R, 0, 99), Ok(()));
        assert_eq!(t.test(F, B, LockKind::Read, 100, 100).unwrap().start, 100);
    }

    #[test]
    fn getlk_reports_the_first_conflict_by_start() {
        let mut t = LockTable::new();
        t.set(F, B, W, 50, 59).unwrap();
        t.set(F, C, W, 10, 19).unwrap();
        let first = t.test(F, A, LockKind::Read, 0, 100).unwrap();
        assert_eq!((first.owner, first.start, first.end), (C, 10, 19));
        assert!(t.test(F, A, LockKind::Read, 20, 49).is_none());
        // Own locks never show up.
        assert!(t.test(F, B, LockKind::Write, 50, 59).is_none());
        // Read vs read is compatible.
        t.set(F, A, R, 70, 79).unwrap();
        assert!(t.test(F, C, LockKind::Read, 70, 79).is_none());
        assert!(t.test(F, C, LockKind::Write, 70, 79).is_some());
    }

    #[test]
    fn ofd_and_posix_conflict_even_in_one_process_but_not_flock() {
        let mut t = LockTable::new();
        let ofd1 = Owner::Description(7);
        let ofd2 = Owner::Description(8);
        t.set(F, A, W, 0, 9).unwrap();
        assert_eq!(t.set(F, ofd1, R, 5, 5), Err(EAGAIN), "same pid, different owner kind");
        t.set(F, ofd1, W, 10, 19).unwrap();
        assert_eq!(t.set(F, ofd2, R, 15, 15), Err(EAGAIN), "two descriptions of one file");
        assert_eq!(t.set(F, A, R, 12, 12), Err(EAGAIN), "posix vs ofd");
        // flock is a different namespace.
        let fl = Owner::Flock(9);
        t.set(F, fl, W, 0, u64::MAX).unwrap();
        assert_eq!(t.set(F, Owner::Flock(10), R, 0, u64::MAX), Err(EAGAIN));
        assert_eq!(t.set(F, Owner::Flock(9), R, 0, u64::MAX), Ok(()), "convert");
        assert_eq!(t.set(F, Owner::Flock(10), R, 0, u64::MAX), Ok(()), "now shared");
    }

    #[test]
    fn flock_and_record_locks_do_not_interact() {
        let mut t = LockTable::new();
        t.set(F, Owner::Flock(1), W, 0, u64::MAX).unwrap();
        assert_eq!(t.set(F, B, W, 0, 9), Ok(()));
        assert!(t.test(F, C, LockKind::Write, 0, 9).is_some());
        t.release_description(1);
        assert_eq!(ranges(&t, F, B), [(LockKind::Write, 0, 9)]);
    }

    #[test]
    fn close_of_any_fd_releases_only_that_process_on_that_file() {
        let mut t = LockTable::new();
        t.set(F, A, W, 0, 9).unwrap();
        t.set(G, A, W, 0, 9).unwrap();
        t.set(F, B, R, 100, 109).unwrap();
        t.set(F, Owner::Description(5), R, 200, 209).unwrap();
        t.release_process_file(100, F);
        assert!(ranges(&t, F, A).is_empty());
        assert_eq!(ranges(&t, G, A).len(), 1, "other file untouched");
        assert_eq!(ranges(&t, F, B).len(), 1, "other process untouched");
        assert_eq!(t.locks_for(F).len(), 2, "OFD lock untouched by a posix close");
        assert_eq!(t.total_locks(), 3);
    }

    fn lifecycle(kind: u16, pid: u64) -> LifecycleEventWire {
        LifecycleEventWire { event: kind, pid, ..LifecycleEventWire::default() }
    }

    #[test]
    fn exit_event_releases_the_holder_so_a_conflicting_request_then_succeeds() {
        let mut t = LockTable::new();
        t.set(F, A, W, 0, 9).unwrap();
        assert_eq!(t.set(F, B, W, 0, 9), Err(EAGAIN));
        t.apply_lifecycle_event(&lifecycle(LIFECYCLE_EVENT_EXIT, 100));
        assert_eq!(t.set(F, B, W, 0, 9), Ok(()));
        assert!(t.locks_for(F).iter().all(|l| l.owner == B));
    }

    #[test]
    fn fork_of_a_reused_pid_retires_a_lost_exit_predecessors_locks() {
        let mut t = LockTable::new();
        t.set(F, A, W, 0, 9).unwrap();
        t.set(G, A, R, 0, 9).unwrap();
        t.set(F, Owner::Description(5), W, 100, 109).unwrap();
        // The exit of pid 100 was lost to overflow; pid 100 is forked anew.
        t.apply_lifecycle_event(&lifecycle(LIFECYCLE_EVENT_FORK, 100));
        assert!(ranges(&t, F, A).is_empty() && ranges(&t, G, A).is_empty());
        assert_eq!(t.total_locks(), 1, "OFD lock is not owned by the pid");
        // The new process starts with no locks and conflicts like anyone else.
        assert_eq!(t.set(F, A, W, 0, 9), Ok(()));
    }

    #[test]
    fn lifecycle_events_for_pid_zero_or_other_kinds_change_nothing() {
        let mut t = LockTable::new();
        t.set(F, A, W, 0, 9).unwrap();
        t.apply_lifecycle_event(&lifecycle(LIFECYCLE_EVENT_EXIT, 0));
        t.apply_lifecycle_event(&lifecycle(LIFECYCLE_EVENT_FORK, 0));
        t.apply_lifecycle_event(&lifecycle(LIFECYCLE_EVENT_FORK + 100, 100));
        t.apply_lifecycle_event(&lifecycle(LIFECYCLE_EVENT_EXIT, 200));
        assert_eq!(t.total_locks(), 1);
    }

    #[test]
    fn exit_releases_posix_locks_everywhere_and_forgets_waits() {
        let mut t = LockTable::new();
        t.set(F, A, W, 0, 9).unwrap();
        t.set(G, A, R, 0, 9).unwrap();
        t.set(F, B, W, 100, 109).unwrap();
        t.set(F, Owner::Description(5), W, 200, 209).unwrap();
        assert_eq!(t.set_wait(F, A, 11, W, 100, 109, 0), Err(EAGAIN));
        assert_eq!(t.wait_count(), 1);
        t.release_process(100);
        assert_eq!(t.wait_count(), 0);
        assert!(ranges(&t, F, A).is_empty() && ranges(&t, G, A).is_empty());
        assert_eq!(t.total_locks(), 2);
        t.release_description(5);
        assert_eq!(t.total_locks(), 1);
        t.release_process(200);
        assert!(t.is_empty());
    }

    #[test]
    fn final_close_releases_ofd_and_flock_of_that_description_only() {
        let mut t = LockTable::new();
        t.set(F, Owner::Description(5), W, 0, 9).unwrap();
        t.set(G, Owner::Flock(5), W, 0, u64::MAX).unwrap();
        t.set(F, Owner::Description(6), W, 20, 29).unwrap();
        t.release_description(5);
        assert_eq!(t.total_locks(), 1);
        assert_eq!(t.set(F, A, W, 0, 9), Ok(()));
    }

    #[test]
    fn blocked_attempt_is_retried_until_the_holder_lets_go() {
        let mut t = LockTable::new();
        t.set(F, A, W, 0, 9).unwrap();
        assert_eq!(t.set_wait(F, B, 21, W, 0, 9, 1), Err(EAGAIN));
        assert_eq!(t.set_wait(F, B, 21, W, 0, 9, 2), Err(EAGAIN));
        assert_eq!(t.wait_count(), 1, "one entry per waiting thread");
        t.release_process_file(100, F);
        assert_eq!(t.set_wait(F, B, 21, W, 0, 9, 3), Ok(()));
        assert_eq!(t.wait_count(), 0, "grant clears the waiter");
    }

    #[test]
    fn two_process_deadlock_is_reported_to_the_closing_waiter() {
        let mut t = LockTable::new();
        t.set(F, A, W, 0, 9).unwrap(); // A holds F[0..9]
        t.set(G, B, W, 0, 9).unwrap(); // B holds G[0..9]
        assert_eq!(t.set_wait(G, A, 1, W, 0, 9, 10), Err(EAGAIN), "A waits for B");
        assert_eq!(t.set_wait(F, B, 2, W, 0, 9, 11), Err(EDEADLK), "B waiting for A closes the cycle");
        // The refused waiter is not remembered as a waiter.
        assert_eq!(t.wait_count(), 1);
    }

    #[test]
    fn three_process_cycle_is_found_and_a_chain_is_not() {
        let mut t = LockTable::new();
        let h = |n: u64| (9, n);
        t.set(h(1), A, W, 0, 0).unwrap();
        t.set(h(2), B, W, 0, 0).unwrap();
        t.set(h(3), C, W, 0, 0).unwrap();
        assert_eq!(t.set_wait(h(2), A, 1, W, 0, 0, 0), Err(EAGAIN)); // A -> B
        assert_eq!(t.set_wait(h(3), B, 2, W, 0, 0, 0), Err(EAGAIN)); // B -> C
        // A chain A -> B -> C is not a deadlock for a fourth process.
        let d = Owner::Process(400);
        assert_eq!(t.set_wait(h(1), d, 4, W, 0, 0, 0), Err(EAGAIN));
        assert_eq!(t.set_wait(h(1), C, 3, W, 0, 0, 0), Err(EDEADLK), "C -> A closes A->B->C->A");
    }

    #[test]
    fn deadlock_needs_overlap_and_a_conflict() {
        let mut t = LockTable::new();
        t.set(F, A, W, 0, 9).unwrap();
        t.set(G, B, W, 0, 9).unwrap();
        assert_eq!(t.set_wait(G, A, 1, W, 0, 9, 0), Err(EAGAIN));
        // B wants a different range of F: no conflict, so no wait and no cycle.
        assert_eq!(t.set_wait(F, B, 2, W, 20, 29, 0), Ok(()));
        // Read/read waits cannot form a cycle either.
        let mut t = LockTable::new();
        t.set(F, A, R, 0, 9).unwrap();
        t.set(G, B, R, 0, 9).unwrap();
        assert_eq!(t.set_wait(G, A, 1, R, 0, 9, 0), Ok(()));
        assert_eq!(t.set_wait(F, B, 2, R, 0, 9, 0), Ok(()));
    }

    #[test]
    fn stale_waiters_age_out_and_cancel_forgets_them() {
        let mut t = LockTable::new();
        t.set(F, A, W, 0, 9).unwrap();
        t.set(G, B, W, 0, 9).unwrap();
        assert_eq!(t.set_wait(G, A, 1, W, 0, 9, 0), Err(EAGAIN));
        // A was interrupted long ago and never retried: B may wait for A.
        assert_eq!(t.set_wait(F, B, 2, W, 0, 9, WAIT_TTL_NS + 1), Err(EAGAIN));
        // An explicit cancel does the same without waiting for the TTL.
        let mut t = LockTable::new();
        t.set(F, A, W, 0, 9).unwrap();
        t.set(G, B, W, 0, 9).unwrap();
        assert_eq!(t.set_wait(G, A, 1, W, 0, 9, 0), Err(EAGAIN));
        t.cancel_wait(1);
        assert_eq!(t.set_wait(F, B, 2, W, 0, 9, 1), Err(EAGAIN));
    }

    #[test]
    fn ofd_and_flock_waits_are_never_deadlocks() {
        let mut t = LockTable::new();
        let o1 = Owner::Description(1);
        let o2 = Owner::Description(2);
        t.set(F, o1, W, 0, 9).unwrap();
        t.set(G, o2, W, 0, 9).unwrap();
        assert_eq!(t.set_wait(G, o1, 1, W, 0, 9, 0), Err(EAGAIN));
        assert_eq!(t.set_wait(F, o2, 2, W, 0, 9, 0), Err(EAGAIN));
        assert_eq!(t.wait_count(), 0);
    }

    #[test]
    fn waiter_table_is_bounded() {
        let mut t = LockTable::new();
        t.set(F, A, W, 0, 0).unwrap();
        for tid in 0..(MAX_WAITS as u64 + 40) {
            assert_eq!(t.set_wait(F, Owner::Process(1000 + tid), tid, W, 0, 0, 0), Err(EAGAIN));
        }
        assert_eq!(t.wait_count(), MAX_WAITS);
    }

    #[test]
    fn per_file_and_total_bounds_are_enolck_and_leave_state_intact() {
        let mut t = LockTable::new();
        // Disjoint read ranges of one owner never merge when separated by gaps.
        for i in 0..MAX_LOCKS_PER_FILE as u64 {
            t.set(F, A, R, i * 8, i * 8 + 2).unwrap();
        }
        assert_eq!(t.set(F, A, R, 1_000_000, 1_000_001), Err(ENOLCK));
        assert_eq!(t.locks_for(F).len(), MAX_LOCKS_PER_FILE);
        // Another owner on the same file counts too.
        assert_eq!(t.set(F, B, R, 1_000_000, 1_000_001), Err(ENOLCK));
        // A split by unlock that needs one more slot is also refused...
        assert_eq!(t.set(F, A, LockOp::Unlock, 1, 1), Err(ENOLCK));
        // ...while a shrinking change still works.
        t.set(F, A, LockOp::Unlock, 0, 2).unwrap();
        assert_eq!(t.locks_for(F).len(), MAX_LOCKS_PER_FILE - 1);

        let mut t = LockTable::new();
        let files = MAX_LOCKS_TOTAL as u64;
        for n in 0..files {
            t.set((2, n), A, R, 0, 1).unwrap();
        }
        assert_eq!(t.set((3, 0), A, R, 0, 1), Err(ENOLCK));
        assert_eq!(t.total_locks(), MAX_LOCKS_TOTAL);
        t.release_process_file(100, (2, 0));
        assert_eq!(t.set((3, 0), A, R, 0, 1), Ok(()));
    }

    #[test]
    fn total_accounting_survives_a_mixed_workload() {
        let mut t = LockTable::new();
        t.set(F, A, W, 0, 99).unwrap();
        t.set(F, B, R, 200, 299).unwrap();
        t.set(G, A, R, 0, 9).unwrap();
        t.set(F, A, LockOp::Unlock, 10, 19).unwrap();
        t.set(F, Owner::Flock(1), R, 0, u64::MAX).unwrap();
        let counted: usize = [F, G].iter().map(|k| t.locks_for(*k).len()).sum();
        assert_eq!(counted, t.total_locks());
        t.release_process(100);
        t.release_description(1);
        t.release_process(200);
        assert_eq!(t.total_locks(), 0);
    }

    #[test]
    fn invalid_ranges_are_rejected() {
        let mut t = LockTable::new();
        assert_eq!(t.set(F, A, W, 10, 5), Err(EINVAL));
    }

    mod wire_tests {
        use super::*;
        use rustos_user_abi::syscall::{
            VFS_FILE_LOCK_CMD_SETLK, VFS_FILE_LOCK_WIRE_VERSION,
        };

        fn ctx(pid: u64, remote_id: u64) -> LockContext {
            LockContext { pid, tid: pid * 10, remote_id, key: F, cursor: 1000, file_len: 5000, now_ns: 0 }
        }

        fn wire(cmd: u16, owner: u16, ty: u16, whence: u16, start: i64, len: i64) -> VfsFileLockWire {
            VfsFileLockWire {
                version: VFS_FILE_LOCK_WIRE_VERSION,
                cmd,
                owner,
                lock_type: ty,
                whence,
                flags: 0,
                start,
                len,
                pid: 0,
            }
        }

        #[test]
        fn getlk_reports_posix_pid_set_range_and_unlck_when_free() {
            let mut t = LockTable::new();
            let w = wire(VFS_FILE_LOCK_CMD_SETLK, VFS_FILE_LOCK_OWNER_POSIX, VFS_FILE_LOCK_TYPE_WRLCK, VFS_FILE_LOCK_WHENCE_SET, 100, 50);
            assert_eq!(execute(&mut t, &ctx(7, 1), &w), Ok(None));
            let probe = wire(VFS_FILE_LOCK_CMD_GETLK, VFS_FILE_LOCK_OWNER_POSIX, VFS_FILE_LOCK_TYPE_RDLCK, VFS_FILE_LOCK_WHENCE_SET, 0, 0);
            let reply = execute(&mut t, &ctx(8, 2), &probe).unwrap().unwrap();
            assert_eq!(
                (reply.lock_type, reply.whence, reply.start, reply.len, reply.pid),
                (VFS_FILE_LOCK_TYPE_WRLCK, VFS_FILE_LOCK_WHENCE_SET, 100, 50, 7)
            );
            // The holder itself sees no conflict; the type becomes UNLCK.
            let own = execute(&mut t, &ctx(7, 1), &probe).unwrap().unwrap();
            assert_eq!(own.lock_type, VFS_FILE_LOCK_TYPE_UNLCK);
            assert_eq!((own.start, own.len), (0, 0), "other fields are left as given");
        }

        #[test]
        fn ofd_conflict_reports_pid_minus_one_and_eof_as_len_zero() {
            let mut t = LockTable::new();
            let w = wire(VFS_FILE_LOCK_CMD_SETLK, VFS_FILE_LOCK_OWNER_OFD, VFS_FILE_LOCK_TYPE_WRLCK, VFS_FILE_LOCK_WHENCE_CUR, -200, 0);
            assert_eq!(execute(&mut t, &ctx(7, 1), &w), Ok(None));
            let probe = wire(VFS_FILE_LOCK_CMD_GETLK, VFS_FILE_LOCK_OWNER_POSIX, VFS_FILE_LOCK_TYPE_WRLCK, VFS_FILE_LOCK_WHENCE_SET, 0, 1 << 20);
            let reply = execute(&mut t, &ctx(8, 2), &probe).unwrap().unwrap();
            assert_eq!((reply.start, reply.len, reply.pid), (800, 0, -1));
        }

        #[test]
        fn whence_uses_the_description_offset_and_the_file_size() {
            let mut t = LockTable::new();
            let cur = wire(VFS_FILE_LOCK_CMD_SETLK, VFS_FILE_LOCK_OWNER_POSIX, VFS_FILE_LOCK_TYPE_WRLCK, VFS_FILE_LOCK_WHENCE_CUR, 0, 10);
            execute(&mut t, &ctx(7, 1), &cur).unwrap();
            assert_eq!(t.locks_for(F)[0].start, 1000);
            let end = wire(VFS_FILE_LOCK_CMD_SETLK, VFS_FILE_LOCK_OWNER_POSIX, VFS_FILE_LOCK_TYPE_WRLCK, VFS_FILE_LOCK_WHENCE_END, -100, 100);
            execute(&mut t, &ctx(7, 1), &end).unwrap();
            assert_eq!(ranges(&t, F, Owner::Process(7)), [(LockKind::Write, 1000, 1009), (LockKind::Write, 4900, 4999)]);
            let bad = wire(VFS_FILE_LOCK_CMD_SETLK, VFS_FILE_LOCK_OWNER_POSIX, VFS_FILE_LOCK_TYPE_WRLCK, VFS_FILE_LOCK_WHENCE_SET, -1, 1);
            assert_eq!(execute(&mut t, &ctx(7, 1), &bad), Err(EINVAL));
        }

        #[test]
        fn flock_is_whole_file_per_description_and_ignores_range_fields() {
            let mut t = LockTable::new();
            let ex = wire(VFS_FILE_LOCK_CMD_SETLK, VFS_FILE_LOCK_OWNER_FLOCK, VFS_FILE_LOCK_TYPE_WRLCK, VFS_FILE_LOCK_WHENCE_SET, 0, 0);
            execute(&mut t, &ctx(7, 1), &ex).unwrap();
            // Another description conflicts even from the same pid.
            assert_eq!(execute(&mut t, &ctx(7, 2), &ex), Err(EAGAIN));
            // The same description (dup/fork) converts freely.
            let sh = wire(VFS_FILE_LOCK_CMD_SETLK, VFS_FILE_LOCK_OWNER_FLOCK, VFS_FILE_LOCK_TYPE_RDLCK, VFS_FILE_LOCK_WHENCE_SET, 0, 0);
            execute(&mut t, &ctx(9, 1), &sh).unwrap();
            execute(&mut t, &ctx(9, 2), &sh).unwrap();
            let un = wire(VFS_FILE_LOCK_CMD_SETLK, VFS_FILE_LOCK_OWNER_FLOCK, VFS_FILE_LOCK_TYPE_UNLCK, VFS_FILE_LOCK_WHENCE_SET, 0, 0);
            execute(&mut t, &ctx(9, 1), &un).unwrap();
            execute(&mut t, &ctx(9, 2), &un).unwrap();
            assert!(t.is_empty());
        }

        #[test]
        fn setlkw_registers_a_waiter_and_cancel_and_release_commands_work() {
            let mut t = LockTable::new();
            let w = wire(VFS_FILE_LOCK_CMD_SETLK, VFS_FILE_LOCK_OWNER_POSIX, VFS_FILE_LOCK_TYPE_WRLCK, VFS_FILE_LOCK_WHENCE_SET, 0, 10);
            execute(&mut t, &ctx(7, 1), &w).unwrap();
            let wait = VfsFileLockWire { cmd: VFS_FILE_LOCK_CMD_SETLKW, ..w };
            assert_eq!(execute(&mut t, &ctx(8, 2), &wait), Err(EAGAIN));
            assert_eq!(t.wait_count(), 1);
            let cancel = wire(VFS_FILE_LOCK_CMD_CANCEL_WAIT, VFS_FILE_LOCK_OWNER_POSIX, VFS_FILE_LOCK_TYPE_UNLCK, 0, 0, 0);
            assert_eq!(execute(&mut t, &ctx(8, 2), &cancel), Ok(None));
            assert_eq!(t.wait_count(), 0);
            let release = wire(VFS_FILE_LOCK_CMD_RELEASE_FILE, VFS_FILE_LOCK_OWNER_POSIX, VFS_FILE_LOCK_TYPE_UNLCK, 0, 0, 0);
            assert_eq!(execute(&mut t, &ctx(7, 1), &release), Ok(None));
            assert!(t.is_empty());
        }

        #[test]
        fn malformed_requests_and_missing_identity_are_einval() {
            let mut t = LockTable::new();
            let good = wire(VFS_FILE_LOCK_CMD_SETLK, VFS_FILE_LOCK_OWNER_POSIX, VFS_FILE_LOCK_TYPE_WRLCK, VFS_FILE_LOCK_WHENCE_SET, 0, 1);
            assert_eq!(execute(&mut t, &ctx(0, 1), &good), Err(EINVAL), "no sender pid");
            assert_eq!(execute(&mut t, &ctx(7, 1), &VfsFileLockWire { pid: 5, ..good }), Err(EINVAL), "wire pid is reply-only");
            assert_eq!(execute(&mut t, &ctx(7, 1), &VfsFileLockWire { version: 9, ..good }), Err(EINVAL));
            let unlck_probe = wire(VFS_FILE_LOCK_CMD_GETLK, VFS_FILE_LOCK_OWNER_POSIX, VFS_FILE_LOCK_TYPE_UNLCK, VFS_FILE_LOCK_WHENCE_SET, 0, 1);
            assert_eq!(execute(&mut t, &ctx(7, 1), &unlck_probe), Err(EINVAL));
            assert!(t.is_empty());
        }
    }
}
