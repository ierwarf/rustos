//! Text of the kernel-fact `/proc/<pid>` files, as pure functions of facts.
//!
//! - **Owner:** `kernel-compat` formats; `proc_identity` gathers the facts
//!   from the calling process only. Nothing here reads kernel state, so every
//!   rule below is unit-testable on the host.
//! - **Truth rule:** a field is printed only from a real kernel counter or
//!   table. Positional fields (`stat`) that nothing tracks print Linux's own
//!   "not available" value (`0`, or `-1` for `tpgid`); keyed files (`status`,
//!   `smaps`) omit the key instead.
//! - **Bound:** snapshots larger than `PROC_SNAPSHOT_MAX_BYTES` are rejected
//!   by the caller rather than returned as a silently incomplete stream.
//! - **Snapshot semantics:** the caller takes the facts once at `open`; reads
//!   serve those bytes, so every read of one open description is consistent.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;


pub(super) const PAGE_BYTES: u64 = 4096;
const RLIM_INFINITY: u64 = u64::MAX;
/// `USER_HZ`: clock ticks per second that `stat` times are expressed in.
pub(super) const CLOCK_TICKS_PER_SEC: u64 = 100;

/// One mapped range as `maps` shows it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MapRegion {
    pub start: u64,
    pub end: u64,
    pub read: bool,
    pub write: bool,
    pub exec: bool,
    pub shared: bool,
    pub offset: u64,
    pub name: Option<String>,
}

/// Facts about the thread a `status`/`stat`/`comm` read is about.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ThreadFacts {
    pub tid: u64,
    /// At most 15 bytes, no NUL.
    pub comm: Vec<u8>,
    /// `R` when the thread is running or runnable, `S` when it is blocked
    /// (the scheduler's run-intent bit; no finer Linux state is tracked).
    pub state: u8,
    pub pending_signals: u64,
    pub blocked_signals: u64,
    /// Consumed CPU in clock ticks. The scheduler keeps no per-thread CPU
    /// counter, so this is `Some` only for the reading thread (its scheduling
    /// context's consumed budget) and `None` (printed 0) for every other.
    pub utime_ticks: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ProcessFacts {
    pub pid: u64,
    pub ppid: u64,
    pub uid: u32,
    pub gid: u32,
    pub euid: u32,
    pub egid: u32,
    pub threads: u64,
    /// Ticks since boot at process creation.
    pub start_ticks: u64,
    pub rss_pages: Option<u64>,
    /// Bytes `mlock`/`mlockall` hold in the process's lock ledger (`VmLck`).
    pub locked_bytes: u64,
    /// Descriptor slots the fd table has allocated (`FDSize`).
    pub fd_slots: Option<u64>,
    /// `oom_score_adj` recorded for the process.
    pub oom_score_adj: i16,
    pub regions: Vec<MapRegion>,
    pub brk_start: u64,
    pub arg_start: u64,
    pub arg_end: u64,
    pub env_start: u64,
    pub env_end: u64,
    pub sig_ignored: u64,
    pub sig_caught: u64,
    pub no_new_privs: bool,
}

impl ProcessFacts {
    pub(super) fn vm_size_bytes(&self) -> u64 {
        self.regions
            .iter()
            .map(|region| region.end.saturating_sub(region.start))
            .sum()
    }

    /// Top of the `[stack]` mapping: where the initial stack starts.
    pub(super) fn start_stack(&self) -> u64 {
        self.regions
            .iter()
            .find(|region| region.name.as_deref() == Some("[stack]"))
            .map_or(0, |region| region.end)
    }
}

// ---- merging the kernel's mapping views -----------------------------------------

/// Orders and flattens the mapping views into non-overlapping regions.
///
/// Ring0 attests two sources: the Linux memory map (image, TLS, heap, stack,
/// with path/label names) and the pager's published regions (every `mmap`).
/// They should not overlap; if they do, the lower-addressed/first entry keeps
/// the range and the later one is trimmed to what it adds, so no byte of
/// address space is listed twice. Adjacent regions that Linux would show as
/// one are merged.
pub(super) fn merge_regions(mut regions: Vec<MapRegion>) -> Vec<MapRegion> {
    regions.retain(|region| region.start < region.end);
    regions.sort_by_key(|region| (region.start, region.end));
    let mut out: Vec<MapRegion> = Vec::with_capacity(regions.len());
    for mut region in regions {
        if let Some(last) = out.last() {
            if region.end <= last.end {
                continue;
            }
            if region.start < last.end {
                let trim = last.end - region.start;
                if region.name.is_some() {
                    region.offset = region.offset.saturating_add(trim);
                }
                region.start = last.end;
            }
        }
        if let Some(last) = out.last_mut() {
            let contiguous = last.end == region.start
                && last.read == region.read
                && last.write == region.write
                && last.exec == region.exec
                && last.shared == region.shared
                && last.name == region.name
                && if region.name.is_none() {
                    last.offset == 0 && region.offset == 0
                } else {
                    last.offset.saturating_add(last.end - last.start) == region.offset
                };
            if contiguous {
                last.end = region.end;
                continue;
            }
        }
        out.push(region);
    }
    out
}

// ---- maps / smaps -----------------------------------------------------------------

fn perms(region: &MapRegion) -> [u8; 4] {
    [
        if region.read { b'r' } else { b'-' },
        if region.write { b'w' } else { b'-' },
        if region.exec { b'x' } else { b'-' },
        if region.shared { b's' } else { b'p' },
    ]
}

/// Width Linux pads the address/inode columns to before the pathname.
const MAPS_PAD_WIDTH: usize = 72;

fn push_map_line(out: &mut String, region: &MapRegion) {
    let start = out.len();
    let perms = perms(region);
    let _ = write!(
        out,
        "{:08x}-{:08x} {} {:08x} 00:00 0",
        region.start,
        region.end,
        core::str::from_utf8(&perms).unwrap_or("----"),
        region.offset,
    );
    if let Some(name) = region.name.as_deref() {
        let written = out.len() - start;
        for _ in written..MAPS_PAD_WIDTH {
            out.push(' ');
        }
        out.push(' ');
        out.push_str(name);
    }
    out.push('\n');
}

/// `/proc/<pid>/maps`. `dev:inode` is `00:00 0` for every row: Ring0 knows the
/// mapped path (image mappings) but not the file's device or inode, and the
/// pager's file-backed mappings carry no path at all, so they appear unnamed.
pub(super) fn format_maps(regions: &[MapRegion]) -> Vec<u8> {
    let mut out = String::new();
    for region in regions {
        push_map_line(&mut out, region);
    }
    out.into_bytes()
}

/// `/proc/<pid>/smaps`: the `maps` line plus the fields that are exact
/// (`Size`, page sizes, `VmFlags`). Per-mapping `Rss`/`Pss`/`Anonymous` are
/// omitted: Ring0 has no per-mapping resident counter and counting leaves for
/// multi-gigabyte reservations at `open` would be unbounded.
pub(super) fn format_smaps(regions: &[MapRegion]) -> Vec<u8> {
    let mut out = String::new();
    for region in regions {
        push_map_line(&mut out, region);
        let _ = writeln!(out, "Size:           {:>8} kB", (region.end - region.start) / 1024);
        out.push_str("KernelPageSize:        4 kB\n");
        out.push_str("MMUPageSize:           4 kB\n");
        out.push_str("VmFlags:");
        let flags: [(&str, bool); 8] = [
            (" rd", region.read),
            (" wr", region.write),
            (" ex", region.exec),
            (" sh", region.shared),
            (" mr", region.read),
            (" mw", region.write),
            (" me", region.exec),
            (" ms", region.shared),
        ];
        for (flag, set) in flags {
            if set {
                out.push_str(flag);
            }
        }
        out.push_str(" \n");
    }
    out.into_bytes()
}

// ---- status / stat / statm / comm ---------------------------------------------------

fn state_name(state: u8) -> &'static str {
    match state {
        b'R' => "R (running)",
        _ => "S (sleeping)",
    }
}

fn comm_str(comm: &[u8]) -> String {
    String::from_utf8_lossy(comm).into_owned()
}

/// `/proc/<pid>/status`. Keys with no real source are omitted (`Groups`:
/// supplementary groups are syscalld policy state; `VmHWM`/`VmPeak`: nothing
/// tracks a high-water mark; `RssAnon`, context-switch counts,
/// `Cpus_allowed`). `FDSize` is the fd table's allocated slot count.
/// `Uid`/`Gid` carry real and effective ids; the saved and filesystem columns
/// repeat the effective id (there is no setuid exec and no `setfsuid`).
pub(super) fn format_status(process: &ProcessFacts, thread: &ThreadFacts) -> Vec<u8> {
    let mut out = String::new();
    let _ = writeln!(out, "Name:\t{}", comm_str(&thread.comm));
    let _ = writeln!(out, "State:\t{}", state_name(thread.state));
    let _ = writeln!(out, "Tgid:\t{}", process.pid);
    let _ = writeln!(out, "Pid:\t{}", thread.tid);
    let _ = writeln!(out, "PPid:\t{}", process.ppid);
    let _ = writeln!(out, "TracerPid:\t0");
    let _ = writeln!(
        out,
        "Uid:\t{}\t{}\t{}\t{}",
        process.uid, process.euid, process.euid, process.euid
    );
    let _ = writeln!(
        out,
        "Gid:\t{}\t{}\t{}\t{}",
        process.gid, process.egid, process.egid, process.egid
    );
    if let Some(slots) = process.fd_slots {
        let _ = writeln!(out, "FDSize:\t{slots}");
    }
    let _ = writeln!(out, "VmSize:\t{:>8} kB", process.vm_size_bytes() / 1024);
    let _ = writeln!(out, "VmLck:\t{:>8} kB", process.locked_bytes / 1024);
    if let Some(pages) = process.rss_pages {
        let _ = writeln!(out, "VmRSS:\t{:>8} kB", pages * (PAGE_BYTES / 1024));
    }
    let _ = writeln!(out, "Threads:\t{}", process.threads);
    let _ = writeln!(out, "SigPnd:\t{:016x}", thread.pending_signals);
    let _ = writeln!(out, "SigBlk:\t{:016x}", thread.blocked_signals);
    let _ = writeln!(out, "SigIgn:\t{:016x}", process.sig_ignored);
    let _ = writeln!(out, "SigCgt:\t{:016x}", process.sig_caught);
    let _ = writeln!(out, "NoNewPrivs:\t{}", u8::from(process.no_new_privs));
    out.into_bytes()
}

/// `/proc/<pid>/stat` (52 fields). Real: pid, comm, state, ppid, `utime`,
/// `num_threads`, `starttime`, `vsize`, `rss`, `startstack`, signal masks,
/// `start_brk` and the argv/envp ranges. Not tracked by Ring0 and therefore
/// zero: `pgrp`, `session`, `tty_nr`, fault counters, `stime`/child times,
/// code/data bounds, `kstkesp`/`kstkeip`, `wchan`, swap, `processor`,
/// scheduling policy and accounting fields (`tpgid` is `-1`, no controlling
/// terminal). `exit_signal` is 17 (`SIGCHLD`), the signal the kernel queues to
/// the parent on exit.
pub(super) fn format_stat(process: &ProcessFacts, thread: &ThreadFacts) -> Vec<u8> {
    let utime = thread.utime_ticks.unwrap_or(0);
    let rss = process.rss_pages.unwrap_or(0);
    let mut out = String::new();
    let _ = write!(
        out,
        "{} ({}) {} {} 0 0 0 -1 0 0 0 0 0 {} 0 0 0 20 0 {} 0 {} {} {} {} 0 0 {} 0 0 {} {} {} {} 0 0 0 17 0 0 0 0 0 0 0 0 {} {} {} {} {} 0\n",
        thread.tid,
        comm_str(&thread.comm),
        thread.state as char,
        process.ppid,
        utime,
        process.threads,
        process.start_ticks,
        process.vm_size_bytes(),
        rss,
        RLIM_INFINITY,
        process.start_stack(),
        thread.pending_signals,
        thread.blocked_signals,
        process.sig_ignored,
        process.sig_caught,
        process.brk_start,
        process.arg_start,
        process.arg_end,
        process.env_start,
        process.env_end,
    );
    out.into_bytes()
}

/// `/proc/<pid>/statm`: `size resident shared text lib data dt` in pages.
/// `size` is the mapped total, `resident` the data-leaf count, `text` the
/// executable mappings, `data` the writable private mappings; `shared`, `lib`
/// and `dt` are not tracked and are 0.
pub(super) fn format_statm(process: &ProcessFacts) -> Vec<u8> {
    let pages = |bytes: u64| bytes / PAGE_BYTES;
    let text: u64 = process
        .regions
        .iter()
        .filter(|region| region.exec)
        .map(|region| region.end - region.start)
        .sum();
    let data: u64 = process
        .regions
        .iter()
        .filter(|region| region.write && !region.shared)
        .map(|region| region.end - region.start)
        .sum();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} {} 0 {} 0 {} 0",
        pages(process.vm_size_bytes()),
        process.rss_pages.unwrap_or(0),
        pages(text),
        pages(data)
    );
    out.into_bytes()
}

/// `/proc/<pid>/comm`.
pub(super) fn format_comm(thread: &ThreadFacts) -> Vec<u8> {
    let mut out = thread.comm.clone();
    out.push(b'\n');
    out
}

/// `/proc/<pid>/oom_score_adj`: the value last written (default 0). It is
/// recorded and inherited but ranks nothing: RustOS has no OOM victim
/// selection for it to influence.
pub(super) fn format_oom_score_adj(process: &ProcessFacts) -> Vec<u8> {
    alloc::format!("{}\n", process.oom_score_adj).into_bytes()
}

// ---- limits ---------------------------------------------------------------------------

/// `(resource index, label, units)` of the rlimits syscalld stores.
pub(super) const LIMIT_ROWS: [(usize, &str, &str); 10] = [
    (0, "Max cpu time", "seconds"),
    (1, "Max file size", "bytes"),
    (2, "Max data size", "bytes"),
    (3, "Max stack size", "bytes"),
    (4, "Max core file size", "bytes"),
    (5, "Max resident set", "bytes"),
    (6, "Max processes", "processes"),
    (7, "Max open files", "files"),
    (8, "Max locked memory", "bytes"),
    (9, "Max address space", "bytes"),
];

/// `/proc/<pid>/limits` from `(soft, hard)` read per resource. Resources
/// syscalld does not store (`LOCKS`, `SIGPENDING`, `MSGQUEUE`, `NICE`,
/// `RTPRIO`, `RTTIME`) have no row. A resource whose query failed has none.
pub(super) fn format_limits(limits: &[Option<(u64, u64)>; 10]) -> Vec<u8> {
    let value = |v: u64| {
        if v == RLIM_INFINITY {
            String::from("unlimited")
        } else {
            alloc::format!("{v}")
        }
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<25} {:<20} {:<20} {:<10}",
        "Limit", "Soft Limit", "Hard Limit", "Units"
    );
    for (index, label, units) in LIMIT_ROWS {
        let Some((soft, hard)) = limits[index] else {
            continue;
        };
        let _ = writeln!(
            out,
            "{:<25} {:<20} {:<20} {:<10}",
            label,
            value(soft),
            value(hard),
            units
        );
    }
    out.into_bytes()
}

// ---- directory snapshots ---------------------------------------------------------------

/// One name of a generated directory.
pub(super) struct DirName {
    pub ino: u64,
    pub d_type: u8,
    pub name: Vec<u8>,
}

/// Pre-encoded `linux_dirent64` stream for a generated directory. `d_off` is
/// the byte offset after each record, so it is monotonic and `getdents64`
/// resumes by the open description's cursor.
pub(super) fn encode_dir_snapshot(names: &[DirName]) -> Vec<u8> {
    use rustos_user_abi::procfs::{dirent64_record_len, encode_dirent64};
    let total: usize = names
        .iter()
        .map(|entry| dirent64_record_len(entry.name.len()))
        .sum();
    let mut out = alloc::vec![0_u8; total];
    let mut at = 0;
    for entry in names {
        let len = dirent64_record_len(entry.name.len());
        let next = (at + len) as i64;
        encode_dirent64(&mut out[at..], entry.ino, next, entry.d_type, &entry.name)
            .expect("record fits the sized buffer");
        at += len;
    }
    out
}

/// Number of bytes of whole `linux_dirent64` records at the start of `bytes`
/// that fit in `limit`.
pub(super) fn whole_dirent_prefix(bytes: &[u8], limit: usize) -> usize {
    let mut at = 0;
    while at + 18 <= bytes.len() {
        let reclen = u16::from_ne_bytes([bytes[at + 16], bytes[at + 17]]) as usize;
        if reclen == 0 || at + reclen > bytes.len() || at + reclen > limit {
            break;
        }
        at += reclen;
    }
    at
}

// ---- argv/envp/auxv bytes -----------------------------------------------------------------

/// Returns the recorded range length, or `None` if absent or inverted.
pub(super) fn user_range_len(start: u64, end: u64) -> Option<usize> {
    if start == 0 || end <= start {
        return None;
    }
    usize::try_from(end - start).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    fn region(start: u64, end: u64, perms: &str, name: Option<&str>) -> MapRegion {
        MapRegion {
            start,
            end,
            read: perms.contains('r'),
            write: perms.contains('w'),
            exec: perms.contains('x'),
            shared: perms.contains('s'),
            offset: 0,
            name: name.map(|n| n.to_string()),
        }
    }

    fn facts(regions: Vec<MapRegion>) -> ProcessFacts {
        ProcessFacts {
            pid: 42,
            ppid: 7,
            uid: 1000,
            gid: 100,
            euid: 1001,
            egid: 101,
            threads: 3,
            start_ticks: 555,
            rss_pages: Some(10),
            locked_bytes: 12 * 1024,
            fd_slots: Some(64),
            oom_score_adj: -250,
            regions,
            brk_start: 0x4000_0000,
            arg_start: 0x7fff_f000,
            arg_end: 0x7fff_f010,
            env_start: 0x7fff_f010,
            env_end: 0x7fff_f040,
            sig_ignored: 0x4,
            sig_caught: 0x8000,
            no_new_privs: true,
        }
    }

    fn thread(tid: u64, comm: &str) -> ThreadFacts {
        ThreadFacts {
            tid,
            comm: comm.as_bytes().to_vec(),
            state: b'R',
            pending_signals: 0x2,
            blocked_signals: 0x1000,
            utime_ticks: Some(12),
        }
    }

    #[test]
    fn maps_line_has_linux_columns_and_pads_names() {
        let mut image = region(0x5555_5555_4000, 0x5555_5555_9000, "r-xp", Some("/apps/chrome"));
        image.offset = 0x1000;
        let anon = region(0x7f00_0000_0000, 0x7f00_0000_2000, "rw-p", None);
        let text = String::from_utf8(format_maps(&[image, anon])).unwrap();
        let mut lines = text.lines();
        let first = lines.next().unwrap();
        assert!(first.starts_with("555555554000-555555559000 r-xp 00001000 00:00 0"));
        // Pathname begins at column 74 (index 73), as in Linux.
        assert_eq!(first.find("/apps/chrome"), Some(MAPS_PAD_WIDTH + 1));
        assert_eq!(
            lines.next().unwrap(),
            "7f0000000000-7f0000002000 rw-p 00000000 00:00 0"
        );
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn short_addresses_are_zero_padded_to_eight_digits() {
        let text = String::from_utf8(format_maps(&[region(0x1000, 0x3000, "r--s", None)])).unwrap();
        assert_eq!(text, "00001000-00003000 r--s 00000000 00:00 0\n");
    }

    #[test]
    fn stack_and_heap_labels_are_printed_as_names() {
        let text = String::from_utf8(format_maps(&[
            region(0x4000_0000, 0x4002_0000, "rw-p", Some("[heap]")),
            region(0x7ffd_0000_0000, 0x7ffd_0002_1000, "rw-p", Some("[stack]")),
        ]))
        .unwrap();
        assert!(text.lines().next().unwrap().ends_with(" [heap]"));
        assert!(text.lines().nth(1).unwrap().ends_with(" [stack]"));
    }

    #[test]
    fn smaps_has_exact_fields_only() {
        let text =
            String::from_utf8(format_smaps(&[region(0x1000, 0x5000, "rw-p", None)])).unwrap();
        assert!(text.contains("Size:                 16 kB\n"));
        assert!(text.contains("KernelPageSize:        4 kB\n"));
        assert!(text.contains("VmFlags: rd wr mr mw \n"));
        assert!(!text.contains("Rss:"));
        assert!(!text.contains("Pss:"));
    }

    #[test]
    fn merge_orders_trims_overlap_and_joins_neighbours() {
        let merged = merge_regions(vec![
            region(0x5000, 0x6000, "rw-p", None),
            region(0x1000, 0x2000, "rw-p", None),
            region(0x2000, 0x3000, "rw-p", None),
            region(0x2800, 0x3800, "rw-p", None), // overlaps the previous
            region(0x9000, 0x9000, "rw-p", None), // empty, dropped
            region(0x4000, 0x5000, "r--p", None), // different perms
        ]);
        let spans: Vec<(u64, u64, bool)> =
            merged.iter().map(|r| (r.start, r.end, r.write)).collect();
        assert_eq!(
            spans,
            vec![(0x1000, 0x3800, true), (0x4000, 0x5000, false), (0x5000, 0x6000, true)]
        );
        // Fully covered duplicate is dropped.
        let merged = merge_regions(vec![
            region(0x1000, 0x4000, "rw-p", Some("[x]")),
            region(0x2000, 0x3000, "r--p", Some("[y]")),
        ]);
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn named_neighbours_merge_only_when_file_offsets_continue() {
        let mut a = region(0x1000, 0x2000, "r--p", Some("/lib/a"));
        let mut b = region(0x2000, 0x3000, "r--p", Some("/lib/a"));
        a.offset = 0;
        b.offset = 0x1000;
        assert_eq!(merge_regions(vec![a.clone(), b.clone()]).len(), 1);
        b.offset = 0x8000;
        assert_eq!(merge_regions(vec![a, b]).len(), 2);
    }

    #[test]
    fn trimming_an_overlapping_file_region_advances_its_offset() {
        let a = region(0x1000, 0x3000, "rw-p", None);
        let mut b = region(0x2000, 0x5000, "r--p", Some("/f"));
        b.offset = 0x10_0000;
        let merged = merge_regions(vec![a, b]);
        assert_eq!(merged[1].start, 0x3000);
        assert_eq!(merged[1].offset, 0x10_1000);
    }

    #[test]
    fn status_reports_real_fields_and_omits_untracked_ones() {
        let text = String::from_utf8(format_status(
            &facts(vec![region(0x1000, 0x5000, "rw-p", None)]),
            &thread(43, "chrome"),
        ))
        .unwrap();
        assert!(text.starts_with("Name:\tchrome\nState:\tR (running)\nTgid:\t42\nPid:\t43\nPPid:\t7\n"));
        assert!(text.contains("Uid:\t1000\t1001\t1001\t1001\n"));
        assert!(text.contains("Gid:\t100\t101\t101\t101\n"));
        assert!(text.contains("VmSize:\t      16 kB\n"));
        assert!(text.contains("VmRSS:\t      40 kB\n"));
        assert!(text.contains("VmLck:\t      12 kB\n"));
        assert!(text.contains("Threads:\t3\n"));
        assert!(text.contains("SigPnd:\t0000000000000002\n"));
        assert!(text.contains("SigBlk:\t0000000000001000\n"));
        assert!(text.contains("SigIgn:\t0000000000000004\n"));
        assert!(text.contains("SigCgt:\t0000000000008000\n"));
        assert!(text.contains("NoNewPrivs:\t1\n"));
        assert!(text.contains("Gid:\t100\t101\t101\t101\nFDSize:\t64\nVmSize:"));
        assert!(!text.contains("VmHWM"));
        assert!(!text.contains("Groups"));
    }

    #[test]
    fn status_omits_rss_when_unavailable_and_marks_other_threads_sleeping() {
        let mut process = facts(vec![]);
        process.rss_pages = None;
        process.fd_slots = None;
        let mut other = thread(44, "worker");
        other.state = b'S';
        let text = String::from_utf8(format_status(&process, &other)).unwrap();
        assert!(!text.contains("VmRSS"));
        assert!(!text.contains("FDSize"));
        assert!(text.contains("State:\tS (sleeping)\n"));
    }

    #[test]
    fn stat_has_52_fields_with_real_values_in_the_right_slots() {
        let process = facts(vec![
            region(0x1000, 0x5000, "rw-p", None),
            region(0x7ffd_0000_0000, 0x7ffd_0002_1000, "rw-p", Some("[stack]")),
        ]);
        let line = String::from_utf8(format_stat(&process, &thread(42, "my app"))).unwrap();
        assert!(line.ends_with('\n'));
        let (head, rest) = line.split_once(") ").unwrap();
        assert_eq!(head, "42 (my app");
        let fields: Vec<&str> = rest.trim_end().split(' ').collect();
        // Fields 3..=52 follow the comm; the state is field 3.
        assert_eq!(fields.len(), 50);
        let field = |n: usize| fields[n - 3];
        assert_eq!(field(3), "R");
        assert_eq!(field(4), "7"); // ppid
        assert_eq!(field(8), "-1"); // tpgid
        assert_eq!(field(14), "12"); // utime
        assert_eq!(field(15), "0"); // stime
        assert_eq!(field(20), "3"); // num_threads
        assert_eq!(field(22), "555"); // starttime
        assert_eq!(field(23), (0x4000_u64 + 0x2_1000).to_string()); // vsize
        assert_eq!(field(24), "10"); // rss pages
        assert_eq!(field(25), u64::MAX.to_string()); // rsslim
        assert_eq!(field(28), 0x7ffd_0002_1000_u64.to_string()); // startstack
        assert_eq!(field(31), "2"); // signal (pending)
        assert_eq!(field(32), "4096"); // blocked
        assert_eq!(field(33), "4"); // sigignore
        assert_eq!(field(34), "32768"); // sigcatch
        assert_eq!(field(38), "17"); // exit_signal
        assert_eq!(field(47), 0x4000_0000_u64.to_string()); // start_brk
        assert_eq!(field(48), 0x7fff_f000_u64.to_string()); // arg_start
        assert_eq!(field(49), 0x7fff_f010_u64.to_string()); // arg_end
        assert_eq!(field(50), 0x7fff_f010_u64.to_string()); // env_start
        assert_eq!(field(51), 0x7fff_f040_u64.to_string()); // env_end
        assert_eq!(field(52), "0"); // exit_code
    }

    #[test]
    fn stat_of_another_thread_reports_no_cpu_time() {
        let mut other = thread(50, "w");
        other.utime_ticks = None;
        other.state = b'S';
        let line = String::from_utf8(format_stat(&facts(vec![]), &other)).unwrap();
        assert!(line.starts_with("50 (w) S 7 "));
        let rest = line.split_once(") ").unwrap().1;
        assert_eq!(rest.split(' ').nth(14 - 3), Some("0"));
    }

    #[test]
    fn statm_counts_pages_from_real_mappings() {
        let process = facts(vec![
            region(0x1000, 0x3000, "r-xp", Some("/x")),
            region(0x3000, 0x8000, "rw-p", None),
            region(0x9000, 0xa000, "rw-s", None),
        ]);
        assert_eq!(
            String::from_utf8(format_statm(&process)).unwrap(),
            "8 10 0 2 0 5 0\n"
        );
    }

    #[test]
    fn comm_and_oom_score_adj() {
        assert_eq!(format_comm(&thread(1, "abc")), b"abc\n");
        assert_eq!(format_oom_score_adj(&facts(vec![])), b"-250\n");
    }

    #[test]
    fn limits_table_formats_unlimited_and_skips_failed_rows() {
        let mut limits = [None; 10];
        limits[3] = Some((8 * 1024 * 1024, u64::MAX));
        limits[7] = Some((1024, 4096));
        let text = String::from_utf8(format_limits(&limits)).unwrap();
        let mut lines = text.lines();
        assert_eq!(
            lines.next().unwrap(),
            "Limit                     Soft Limit           Hard Limit           Units     "
        );
        assert_eq!(
            lines.next().unwrap(),
            "Max stack size            8388608              unlimited            bytes     "
        );
        assert_eq!(
            lines.next().unwrap(),
            "Max open files            1024                 4096                 files     "
        );
        assert!(lines.next().is_none());
    }

    #[test]
    fn dir_snapshot_encodes_whole_records_with_monotonic_offsets() {
        let bytes = encode_dir_snapshot(&[
            DirName { ino: 1, d_type: 4, name: b".".to_vec() },
            DirName { ino: 2, d_type: 4, name: b"..".to_vec() },
            DirName { ino: 3, d_type: 10, name: b"17".to_vec() },
        ]);
        assert_eq!(bytes.len(), 24 + 24 + 24);
        let off = |at: usize| i64::from_ne_bytes(bytes[at + 8..at + 16].try_into().unwrap());
        assert_eq!((off(0), off(24), off(48)), (24, 48, 72));
        assert_eq!(bytes[18], 4);
        assert_eq!(bytes[48 + 18], 10);
        assert_eq!(&bytes[48 + 19..48 + 21], b"17");
        assert_eq!(whole_dirent_prefix(&bytes, 100), 72);
        assert_eq!(whole_dirent_prefix(&bytes, 50), 48);
        assert_eq!(whole_dirent_prefix(&bytes, 23), 0);
        assert_eq!(whole_dirent_prefix(&bytes[..60], 100), 48);
    }

    #[test]
    fn user_range_requires_a_recorded_nonempty_layout() {
        assert_eq!(user_range_len(0, 100), None);
        assert_eq!(user_range_len(100, 100), None);
        assert_eq!(user_range_len(200, 100), None);
        assert_eq!(user_range_len(100, 164), Some(64));
    }
}
