//! Linux `inotify` instance, watch, and event-queue engine for `vfsd`.
//!
//! - **Owner:** `vfsd` owns every inotify instance (`remote_id`), its
//!   `(mount_id, mount_generation, node/canonical_path)` watches, and its
//!   bounded `struct inotify_event` queue.
//! - **Boundary:** `read()` validates `buf.len() >= 16`, emits only whole
//!   records, and enforces `max_user_instances`, `max_user_watches`, and
//!   `max_queued_events` reported under `/proc/sys/fs/inotify/*`.
//! - **Lifecycle:** `IN_IGNORED` is emitted on `inotify_rm_watch`, target
//!   deletion, and `IN_ONESHOT` delivery. `vfsd` has no mount/umount operation
//!   (both answer `EOPNOTSUPP`), so no `IN_UNMOUNT` event is produced.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use core::mem::size_of;

use rustos_user_abi::linux as linux_abi;

use crate::{EAGAIN, EINVAL};

const EBADF: i32 = 9;
const EEXIST: i32 = 17;
const EMFILE: i32 = 24;
const ENOSPC: i32 = 28;

pub const MAX_USER_INSTANCES: usize = 1024;
pub const MAX_USER_WATCHES: usize = 65_536;
pub const MAX_QUEUED_EVENTS: usize = 16_384;

const EVENT_HEADER_SIZE: usize = size_of::<linux_abi::LinuxInotifyEventHeader>();
const _: () = assert!(EVENT_HEADER_SIZE == 16);

const WATCH_STORED_EVENTS: u32 = linux_abi::IN_ALL_EVENTS
    | linux_abi::IN_ONLYDIR
    | linux_abi::IN_DONT_FOLLOW
    | linux_abi::IN_EXCL_UNLINK
    | linux_abi::IN_ONESHOT;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatchTarget {
    pub mount_id: u64,
    pub mount_generation: u64,
    pub node: u64,
    pub canonical_path: String,
    pub is_dir: bool,
}

impl WatchTarget {
    fn same_object(&self, other: &Self) -> bool {
        if self.mount_id != other.mount_id || self.mount_generation != other.mount_generation {
            return false;
        }
        if self.node != 0 && other.node != 0 {
            self.node == other.node
        } else {
            self.canonical_path == other.canonical_path
        }
    }

    fn matches_target(&self, mount_id: u64, mount_generation: u64, node: u64, path: &str) -> bool {
        if self.mount_id != mount_id || self.mount_generation != mount_generation {
            return false;
        }
        if self.node != 0 && node != 0 {
            self.node == node
        } else {
            self.canonical_path == path
        }
    }
}

#[derive(Clone, Debug)]
struct InotifyWatch {
    mask: u32,
    target: WatchTarget,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct QueuedEvent {
    wd: i32,
    mask: u32,
    cookie: u32,
    name: Vec<u8>,
}

impl QueuedEvent {
    fn padded_name_len(name_len: usize) -> usize {
        if name_len == 0 {
            0
        } else {
            (name_len + 1).next_multiple_of(EVENT_HEADER_SIZE)
        }
    }

    fn encoded_len(&self) -> usize {
        EVENT_HEADER_SIZE + Self::padded_name_len(self.name.len())
    }

    fn encode_into(&self, out: &mut [u8]) -> Option<usize> {
        let total = self.encoded_len();
        if out.len() < total {
            return None;
        }
        let name_field_len = Self::padded_name_len(self.name.len());
        let header = linux_abi::LinuxInotifyEventHeader {
            wd: self.wd,
            mask: self.mask,
            cookie: self.cookie,
            len: name_field_len as u32,
        };
        out[0..4].copy_from_slice(&header.wd.to_le_bytes());
        out[4..8].copy_from_slice(&header.mask.to_le_bytes());
        out[8..12].copy_from_slice(&header.cookie.to_le_bytes());
        out[12..16].copy_from_slice(&header.len.to_le_bytes());
        if name_field_len > 0 {
            out[EVENT_HEADER_SIZE..total].fill(0);
            out[EVENT_HEADER_SIZE..EVENT_HEADER_SIZE + self.name.len()]
                .copy_from_slice(&self.name);
        }
        Some(total)
    }
}

#[derive(Debug)]
struct InotifyInstance {
    next_wd: i32,
    watches: BTreeMap<i32, InotifyWatch>,
    queue: VecDeque<QueuedEvent>,
    queued_bytes: usize,
    overflow_emitted: bool,
}

impl InotifyInstance {
    fn new() -> Self {
        Self {
            next_wd: 1,
            watches: BTreeMap::new(),
            queue: VecDeque::new(),
            queued_bytes: 0,
            overflow_emitted: false,
        }
    }

    fn alloc_wd(&mut self) -> Option<i32> {
        for _ in 0..=i32::MAX {
            let candidate = self.next_wd;
            self.next_wd = if self.next_wd == i32::MAX {
                1
            } else {
                self.next_wd + 1
            };
            if candidate > 0 && !self.watches.contains_key(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    fn push_event(&mut self, wd: i32, mask: u32, cookie: u32, name: &[u8]) -> bool {
        // Coalesce consecutive identical unread events (same wd/mask/cookie/name),
        // matching Linux inotify queue coalescing.
        if mask != linux_abi::IN_Q_OVERFLOW {
            if let Some(back) = self.queue.back() {
                if back.wd == wd
                    && back.mask == mask
                    && back.cookie == cookie
                    && back.name.as_slice() == name
                {
                    return false;
                }
            }
        }
        if self.queue.len() >= MAX_QUEUED_EVENTS {
            if !self.overflow_emitted {
                let overflow = QueuedEvent {
                    wd: -1,
                    mask: linux_abi::IN_Q_OVERFLOW,
                    cookie: 0,
                    name: Vec::new(),
                };
                self.queued_bytes += overflow.encoded_len();
                self.queue.push_back(overflow);
                self.overflow_emitted = true;
                return true;
            }
            return false;
        }
        let ev = QueuedEvent {
            wd,
            mask,
            cookie,
            name: name.to_vec(),
        };
        self.queued_bytes += ev.encoded_len();
        self.queue.push_back(ev);
        true
    }
}

#[derive(Debug, Default)]
pub struct InotifyRegistry {
    instances: BTreeMap<u64, InotifyInstance>,
    next_cookie: u32,
}

impl InotifyRegistry {
    pub const fn new() -> Self {
        Self {
            instances: BTreeMap::new(),
            next_cookie: 1,
        }
    }

    pub fn instance_count(&self) -> usize {
        self.instances.len()
    }

    pub fn total_watch_count(&self) -> usize {
        self.instances.values().map(|inst| inst.watches.len()).sum()
    }

    pub fn create_instance(&mut self, remote_id: u64) -> Result<(), i32> {
        if self.instances.contains_key(&remote_id) {
            return Ok(());
        }
        if self.instances.len() >= MAX_USER_INSTANCES {
            return Err(EMFILE);
        }
        self.instances.insert(remote_id, InotifyInstance::new());
        Ok(())
    }

    pub fn remove_instance(&mut self, remote_id: u64) {
        self.instances.remove(&remote_id);
    }

    pub fn add_watch(
        &mut self,
        remote_id: u64,
        target: WatchTarget,
        mask: u32,
    ) -> Result<i32, i32> {
        if (mask & (linux_abi::IN_MASK_ADD | linux_abi::IN_MASK_CREATE))
            == (linux_abi::IN_MASK_ADD | linux_abi::IN_MASK_CREATE)
            || mask & linux_abi::IN_ALL_EVENTS == 0
        {
            return Err(EINVAL);
        }
        let stored_mask = mask & WATCH_STORED_EVENTS;
        let total_watches = self.total_watch_count();
        let instance = self.instances.get_mut(&remote_id).ok_or(EBADF)?;

        let existing_wd = instance
            .watches
            .iter()
            .find(|(_, watch)| watch.target.same_object(&target))
            .map(|(wd, _)| *wd);

        if let Some(wd) = existing_wd {
            if mask & linux_abi::IN_MASK_CREATE != 0 {
                return Err(EEXIST);
            }
            if let Some(watch) = instance.watches.get_mut(&wd) {
                if mask & linux_abi::IN_MASK_ADD != 0 {
                    watch.mask |= stored_mask;
                } else {
                    watch.mask = stored_mask;
                }
                watch.target.canonical_path = target.canonical_path;
            }
            return Ok(wd);
        }

        if total_watches >= MAX_USER_WATCHES {
            return Err(ENOSPC);
        }
        let wd = instance.alloc_wd().ok_or(ENOSPC)?;
        instance.watches.insert(
            wd,
            InotifyWatch {
                mask: stored_mask,
                target,
            },
        );
        Ok(wd)
    }

    pub fn rm_watch(&mut self, remote_id: u64, wd: i32) -> Result<bool, i32> {
        if wd <= 0 {
            return Err(EINVAL);
        }
        let instance = self.instances.get_mut(&remote_id).ok_or(EBADF)?;
        if instance.watches.remove(&wd).is_none() {
            return Err(EINVAL);
        }
        Ok(instance.push_event(wd, linux_abi::IN_IGNORED, 0, &[]))
    }

    pub fn alloc_cookie(&mut self) -> u32 {
        let cookie = self.next_cookie.max(1);
        self.next_cookie = cookie.wrapping_add(1).max(1);
        cookie
    }

    pub fn is_readable(&self, remote_id: u64) -> Option<bool> {
        self.instances
            .get(&remote_id)
            .map(|inst| !inst.queue.is_empty())
    }

    pub fn pending_bytes(&self, remote_id: u64) -> Option<usize> {
        self.instances
            .get(&remote_id)
            .map(|inst| inst.queued_bytes)
    }

    pub fn read_events(&mut self, remote_id: u64, out: &mut [u8]) -> Result<usize, i32> {
        if out.len() < EVENT_HEADER_SIZE {
            return Err(EINVAL);
        }
        let instance = self.instances.get_mut(&remote_id).ok_or(EBADF)?;
        let Some(front) = instance.queue.front() else {
            return Err(EAGAIN);
        };
        if front.encoded_len() > out.len() {
            return Err(EINVAL);
        }
        let mut written = 0usize;
        while let Some(front) = instance.queue.front() {
            let need = front.encoded_len();
            if written + need > out.len() {
                break;
            }
            let ev = instance.queue.pop_front().unwrap();
            let encoded = ev.encode_into(&mut out[written..]).ok_or(EINVAL)?;
            instance.queued_bytes = instance.queued_bytes.saturating_sub(encoded);
            written += encoded;
        }
        if instance.queue.len() < MAX_QUEUED_EVENTS {
            instance.overflow_emitted = false;
        }
        Ok(written)
    }

    /// Emits a filesystem event to matching watches on the target itself and/or
    /// its parent directory, returning `remote_id`s whose queue changed.
    #[allow(clippy::too_many_arguments)]
    pub fn emit_path_event(
        &mut self,
        mount_id: u64,
        mount_generation: u64,
        node: u64,
        canonical_path: &str,
        parent_node: u64,
        parent_path: Option<&str>,
        child_name: Option<&[u8]>,
        event_mask: u32,
        is_dir: bool,
        cookie: u32,
        new_path_on_move_self: Option<&str>,
    ) -> Vec<u64> {
        let mut woken = Vec::new();
        let dir_flag = if is_dir { linux_abi::IN_ISDIR } else { 0 };
        let parent_watch_events = event_mask & linux_abi::IN_ALL_EVENTS;
        let self_event_bit = match event_mask & linux_abi::IN_ALL_EVENTS {
            e if e & linux_abi::IN_DELETE != 0 => linux_abi::IN_DELETE_SELF,
            // The object moved once: `IN_MOVE_SELF` comes from the source-side
            // `IN_MOVED_FROM` emission only, so the destination-side
            // `IN_MOVED_TO` pass does not repeat it for the same watch.
            e if e & linux_abi::IN_MOVED_FROM != 0 => linux_abi::IN_MOVE_SELF,
            e if e & linux_abi::IN_MOVED_TO != 0 => 0,
            e => e,
        };

        for (&remote_id, instance) in &mut self.instances {
            if instance.watches.is_empty() {
                continue;
            }
            // One pass over the watch table; allocates only when something matches.
            let hits: Vec<(i32, u32, bool, bool, bool)> = instance
                .watches
                .iter()
                .filter_map(|(&wd, watch)| {
                    let self_match = watch.target.matches_target(
                        mount_id,
                        mount_generation,
                        node,
                        canonical_path,
                    );
                    let parent_match = !self_match
                        && child_name.is_some()
                        && watch.target.is_dir
                        && parent_path.is_some_and(|parent| {
                            watch.target.matches_target(
                                mount_id,
                                mount_generation,
                                parent_node,
                                parent,
                            )
                        });
                    (self_match || parent_match).then_some((
                        wd,
                        watch.mask,
                        watch.target.is_dir,
                        self_match,
                        parent_match,
                    ))
                })
                .collect();
            let mut changed = false;
            let mut remove_wds = Vec::new();
            for (wd, watch_mask, watch_is_dir, is_self_match, is_parent_match) in hits {
                if is_self_match
                    && (self_event_bit
                        & (linux_abi::IN_ACCESS
                            | linux_abi::IN_MODIFY
                            | linux_abi::IN_ATTRIB
                            | linux_abi::IN_CLOSE_WRITE
                            | linux_abi::IN_CLOSE_NOWRITE
                            | linux_abi::IN_OPEN
                            | linux_abi::IN_DELETE_SELF
                            | linux_abi::IN_MOVE_SELF)
                        != 0)
                {
                    let matched = watch_mask & self_event_bit;
                    if matched != 0 {
                        let out_mask =
                            matched | if watch_is_dir { linux_abi::IN_ISDIR } else { 0 };
                        let out_cookie = if matched & linux_abi::IN_MOVE_SELF != 0 {
                            0
                        } else {
                            cookie
                        };
                        if instance.push_event(wd, out_mask, out_cookie, &[]) {
                            changed = true;
                        }
                        if watch_mask & linux_abi::IN_ONESHOT != 0 {
                            remove_wds.push(wd);
                        }
                    }
                    if self_event_bit & linux_abi::IN_DELETE_SELF != 0 {
                        remove_wds.push(wd);
                    } else if self_event_bit & linux_abi::IN_MOVE_SELF != 0 {
                        if let Some(new_path) = new_path_on_move_self {
                            if let Some(watch) = instance.watches.get_mut(&wd) {
                                watch.target.canonical_path = String::from(new_path);
                            }
                        }
                    }
                    continue;
                }

                // 2. Parent directory watch delivery for child entry events.
                if is_parent_match && (watch_mask & parent_watch_events != 0) {
                    if let Some(child_name) = child_name {
                        let matched = watch_mask & parent_watch_events;
                        let out_mask = matched | dir_flag;
                        if instance.push_event(wd, out_mask, cookie, child_name) {
                            changed = true;
                        }
                        if watch_mask & linux_abi::IN_ONESHOT != 0 {
                            remove_wds.push(wd);
                        }
                    }
                }
            }
            for wd in remove_wds {
                if instance.watches.remove(&wd).is_some()
                    && instance.push_event(wd, linux_abi::IN_IGNORED, 0, &[])
                {
                    changed = true;
                }
            }
            if changed {
                woken.push(remote_id);
            }
        }
        woken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_target(path: &str) -> WatchTarget {
        WatchTarget {
            mount_id: 1,
            mount_generation: 1,
            node: 0,
            canonical_path: String::from(path),
            is_dir: true,
        }
    }

    fn file_target(path: &str, node: u64) -> WatchTarget {
        WatchTarget {
            mount_id: 2,
            mount_generation: 1,
            node,
            canonical_path: String::from(path),
            is_dir: false,
        }
    }

    fn decode_header(buf: &[u8]) -> (linux_abi::LinuxInotifyEventHeader, &[u8]) {
        let wd = i32::from_le_bytes(buf[0..4].try_into().unwrap());
        let mask = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let cookie = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        let len = u32::from_le_bytes(buf[12..16].try_into().unwrap());
        let name_end = 16 + len as usize;
        let name = buf[16..name_end]
            .split(|&b| b == 0)
            .next()
            .unwrap_or(&[]);
        (
            linux_abi::LinuxInotifyEventHeader {
                wd,
                mask,
                cookie,
                len,
            },
            name,
        )
    }

    #[test]
    fn watch_create_modify_delete_and_whole_record_read() {
        let mut reg = InotifyRegistry::new();
        reg.create_instance(10).unwrap();
        let dir_wd = reg
            .add_watch(
                10,
                dir_target("/tmp/dir"),
                linux_abi::IN_CREATE | linux_abi::IN_MODIFY | linux_abi::IN_DELETE,
            )
            .unwrap();
        assert_eq!(reg.is_readable(10), Some(false));

        let woken = reg.emit_path_event(
            1,
            1,
            0,
            "/tmp/dir/hello.txt",
            0,
            Some("/tmp/dir"),
            Some(b"hello.txt"),
            linux_abi::IN_CREATE,
            false,
            0,
            None,
        );
        assert_eq!(woken, alloc::vec![10]);
        assert_eq!(reg.is_readable(10), Some(true));

        // Buffer smaller than 16 bytes -> EINVAL.
        let mut tiny = [0_u8; 15];
        assert_eq!(reg.read_events(10, &mut tiny), Err(EINVAL));

        // Buffer >= 16 bytes but smaller than record (16 + 16 = 32) -> EINVAL.
        let mut small = [0_u8; 16];
        assert_eq!(reg.read_events(10, &mut small), Err(EINVAL));

        let mut buf = [0_u8; 64];
        let n = reg.read_events(10, &mut buf).unwrap();
        assert_eq!(n, 32);
        let (hdr, name) = decode_header(&buf[..n]);
        assert_eq!(hdr.wd, dir_wd);
        assert_eq!(hdr.mask, linux_abi::IN_CREATE);
        assert_eq!(hdr.cookie, 0);
        assert_eq!(hdr.len, 16);
        assert_eq!(name, b"hello.txt");
        assert_eq!(reg.is_readable(10), Some(false));
        assert_eq!(reg.read_events(10, &mut buf), Err(EAGAIN));
    }

    #[test]
    fn rename_emits_paired_moved_from_and_moved_to_with_cookie() {
        let mut reg = InotifyRegistry::new();
        reg.create_instance(11).unwrap();
        let wd = reg
            .add_watch(
                11,
                dir_target("/tmp/a"),
                linux_abi::IN_MOVED_FROM | linux_abi::IN_MOVED_TO,
            )
            .unwrap();
        let cookie = reg.alloc_cookie();
        assert_ne!(cookie, 0);
        reg.emit_path_event(
            1,
            1,
            0,
            "/tmp/a/old",
            0,
            Some("/tmp/a"),
            Some(b"old"),
            linux_abi::IN_MOVED_FROM,
            false,
            cookie,
            Some("/tmp/a/new"),
        );
        reg.emit_path_event(
            1,
            1,
            0,
            "/tmp/a/new",
            0,
            Some("/tmp/a"),
            Some(b"new"),
            linux_abi::IN_MOVED_TO,
            false,
            cookie,
            None,
        );
        let mut buf = [0_u8; 128];
        let n = reg.read_events(11, &mut buf).unwrap();
        assert_eq!(n, 64);
        let (h1, n1) = decode_header(&buf[0..32]);
        let (h2, n2) = decode_header(&buf[32..64]);
        assert_eq!(h1.wd, wd);
        assert_eq!(h1.mask, linux_abi::IN_MOVED_FROM);
        assert_eq!(h1.cookie, cookie);
        assert_eq!(n1, b"old");
        assert_eq!(h2.wd, wd);
        assert_eq!(h2.mask, linux_abi::IN_MOVED_TO);
        assert_eq!(h2.cookie, cookie);
        assert_eq!(n2, b"new");
    }

    #[test]
    fn rm_watch_and_oneshot_emit_in_ignored_and_overflow_bounds_queue() {
        let mut reg = InotifyRegistry::new();
        reg.create_instance(12).unwrap();
        let wd_oneshot = reg
            .add_watch(
                12,
                file_target("/mem/f", 77),
                linux_abi::IN_MODIFY | linux_abi::IN_ONESHOT,
            )
            .unwrap();
        let wd_rm = reg
            .add_watch(12, file_target("/mem/g", 88), linux_abi::IN_ATTRIB)
            .unwrap();
        // IN_MASK_CREATE rejects duplicate target.
        assert_eq!(
            reg.add_watch(
                12,
                file_target("/mem/g", 88),
                linux_abi::IN_ATTRIB | linux_abi::IN_MASK_CREATE,
            ),
            Err(EEXIST)
        );
        reg.emit_path_event(
            2,
            1,
            77,
            "/mem/f",
            0,
            Some("/mem"),
            Some(b"f"),
            linux_abi::IN_MODIFY,
            false,
            0,
            None,
        );
        assert!(reg.rm_watch(12, wd_rm).unwrap());

        let mut buf = [0_u8; 128];
        let n = reg.read_events(12, &mut buf).unwrap();
        assert_eq!(n, 48);
        let (e1, _) = decode_header(&buf[0..16]);
        let (e2, _) = decode_header(&buf[16..32]);
        let (e3, _) = decode_header(&buf[32..48]);
        assert_eq!((e1.wd, e1.mask), (wd_oneshot, linux_abi::IN_MODIFY));
        assert_eq!((e2.wd, e2.mask), (wd_oneshot, linux_abi::IN_IGNORED));
        assert_eq!((e3.wd, e3.mask), (wd_rm, linux_abi::IN_IGNORED));

        // Fill queue past MAX_QUEUED_EVENTS -> single trailing IN_Q_OVERFLOW.
        let wd_fill = reg
            .add_watch(12, dir_target("/tmp/fill"), linux_abi::IN_CREATE)
            .unwrap();
        for i in 0..(MAX_QUEUED_EVENTS + 10) {
            let name = alloc::format!("f{i}");
            reg.emit_path_event(
                1,
                1,
                0,
                &alloc::format!("/tmp/fill/{name}"),
                0,
                Some("/tmp/fill"),
                Some(name.as_bytes()),
                linux_abi::IN_CREATE,
                false,
                0,
                None,
            );
        }
        assert_eq!(
            reg.instances.get(&12).unwrap().queue.back().map(|e| (e.wd, e.mask)),
            Some((-1, linux_abi::IN_Q_OVERFLOW))
        );
        assert_eq!(
            reg.instances.get(&12).unwrap().queue.len(),
            MAX_QUEUED_EVENTS + 1
        );
        assert_eq!(wd_fill > 0, true);
    }

    #[test]
    fn rename_reports_move_self_once_and_updates_path_watch() {
        let mut reg = InotifyRegistry::new();
        reg.create_instance(13).unwrap();
        let self_wd = reg
            .add_watch(13, dir_target("/tmp/b/old"), linux_abi::IN_MOVE_SELF)
            .unwrap();
        let parent_wd = reg
            .add_watch(
                13,
                dir_target("/tmp/b"),
                linux_abi::IN_MOVED_FROM | linux_abi::IN_MOVED_TO,
            )
            .unwrap();
        let cookie = reg.alloc_cookie();
        for (path, mask, new_path) in [
            ("/tmp/b/old", linux_abi::IN_MOVED_FROM, Some("/tmp/b/new")),
            ("/tmp/b/new", linux_abi::IN_MOVED_TO, None),
        ] {
            let name: &[u8] = if mask == linux_abi::IN_MOVED_FROM { b"old" } else { b"new" };
            reg.emit_path_event(
                1, 1, 0, path, 0, Some("/tmp/b"), Some(name), mask, true, cookie, new_path,
            );
        }
        let mut buf = [0_u8; 256];
        let n = reg.read_events(13, &mut buf).unwrap();
        let mut masks = alloc::vec::Vec::new();
        let mut off = 0;
        while off < n {
            let (h, _) = decode_header(&buf[off..n]);
            masks.push((h.wd, h.mask & !linux_abi::IN_ISDIR));
            off += 16 + h.len as usize;
        }
        assert_eq!(
            masks,
            alloc::vec![
                (self_wd, linux_abi::IN_MOVE_SELF),
                (parent_wd, linux_abi::IN_MOVED_FROM),
                (parent_wd, linux_abi::IN_MOVED_TO),
            ]
        );
    }

    #[test]
    fn instance_is_independent_of_creating_process_and_removed_on_close() {
        let mut reg = InotifyRegistry::new();
        reg.create_instance(14).unwrap();
        reg.remove_instance(14);
        assert_eq!(reg.read_events(14, &mut [0_u8; 32]), Err(EBADF));
    }

    #[test]
    fn add_watch_validation_mask_add_pending_bytes_and_rm_errors() {
        let mut reg = InotifyRegistry::new();
        reg.create_instance(15).unwrap();
        assert_eq!(reg.add_watch(15, dir_target("/d"), linux_abi::IN_ONESHOT), Err(EINVAL));
        assert_eq!(
            reg.add_watch(
                15,
                dir_target("/d"),
                linux_abi::IN_CREATE | linux_abi::IN_MASK_ADD | linux_abi::IN_MASK_CREATE
            ),
            Err(EINVAL)
        );
        assert_eq!(reg.add_watch(99, dir_target("/d"), linux_abi::IN_CREATE), Err(EBADF));
        let wd = reg.add_watch(15, dir_target("/d"), linux_abi::IN_CREATE).unwrap();
        // Same object again: same wd, mask replaced, then merged with MASK_ADD.
        assert_eq!(
            reg.add_watch(15, dir_target("/d"), linux_abi::IN_DELETE).unwrap(),
            wd
        );
        assert_eq!(
            reg.add_watch(15, dir_target("/d"), linux_abi::IN_MODIFY | linux_abi::IN_MASK_ADD)
                .unwrap(),
            wd
        );
        assert_eq!(
            reg.instances[&15].watches[&wd].mask,
            linux_abi::IN_DELETE | linux_abi::IN_MODIFY
        );
        assert_eq!(reg.rm_watch(15, 0), Err(EINVAL));
        assert_eq!(reg.rm_watch(15, wd + 100), Err(EINVAL));
        // "ab" -> 16 header + 4 padded name bytes (name + NUL rounded to 16 = 16).
        reg.emit_path_event(
            1, 1, 0, "/d/ab", 0, Some("/d"), Some(b"ab"), linux_abi::IN_DELETE, false, 0, None,
        );
        assert_eq!(reg.pending_bytes(15), Some(32));
        // An identical unread event is coalesced.
        reg.emit_path_event(
            1, 1, 0, "/d/ab", 0, Some("/d"), Some(b"ab"), linux_abi::IN_DELETE, false, 0, None,
        );
        assert_eq!(reg.pending_bytes(15), Some(32));
        let mut buf = [0_u8; 64];
        assert_eq!(reg.read_events(15, &mut buf), Ok(32));
        assert_eq!(reg.pending_bytes(15), Some(0));
    }
}
