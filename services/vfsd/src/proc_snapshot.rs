//! Ring3 `/proc/<pid>` formatting from kernel-attested binary facts.
//!
//! - **Owner:** `vfsd` owns `/proc` formatting and namespace policy; Ring0
//!   (`SYS_RUSTOS_PROC_FACTS_BROKER`) supplies only attested raw binary facts.
//! - **Truth rule:** a field is printed only from a real kernel counter or
//!   table. Positional fields (`stat`) that nothing tracks print Linux's own
//!   "not available" value (`0`, or `-1` for `tpgid`); keyed files (`status`,
//!   `smaps`) omit the key instead.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use rustos_user_abi::procfs::{
    ProcMapRegionWire, ProcProcessFactsWire,
    ProcThreadFactsWire, PROC_REGION_EXEC, PROC_REGION_READ, PROC_REGION_SHARED,
    PROC_REGION_WRITE,
};

pub const PAGE_BYTES: u64 = 4096;
const RLIM_INFINITY: u64 = u64::MAX;

/// One mapped range as `maps` shows it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MapRegion {
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
pub struct ThreadFacts {
    pub tid: u64,
    /// At most 15 bytes, no NUL.
    pub comm: Vec<u8>,
    /// `R` when running or runnable, `S` when blocked.
    pub state: u8,
    pub pending_signals: u64,
    pub blocked_signals: u64,
    pub utime_ticks: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessFacts {
    pub pid: u64,
    pub ppid: u64,
    pub uid: u32,
    pub gid: u32,
    pub euid: u32,
    pub egid: u32,
    pub threads: u64,
    pub start_ticks: u64,
    pub rss_pages: Option<u64>,
    pub locked_bytes: u64,
    pub fd_slots: Option<u64>,
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
    pub fn vm_size_bytes(&self) -> u64 {
        self.regions
            .iter()
            .fold(0_u64, |total, region| total.saturating_add(region.end.saturating_sub(region.start)))
    }

    pub fn start_stack(&self) -> u64 {
        self.regions
            .iter()
            .find(|region| region.name.as_deref() == Some("[stack]"))
            .map_or(0, |region| region.end)
    }
}

/// Decodes a `PROC_FACTS_OP_PROCESS` payload into `ProcessFacts`.
pub fn decode_process_facts(bytes: &[u8]) -> Option<ProcessFacts> {
    let (header, mut rest) = ProcProcessFactsWire::decode(bytes)?;
    let count = usize::try_from(header.region_count).ok()?;
    let mut regions = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let (wire, next) = ProcMapRegionWire::decode(rest)?;
        rest = next;
        let name = wire
            .name
            .map(|n| String::from_utf8_lossy(n).into_owned());
        regions.push(MapRegion {
            start: wire.start,
            end: wire.end,
            read: wire.flags & PROC_REGION_READ != 0,
            write: wire.flags & PROC_REGION_WRITE != 0,
            exec: wire.flags & PROC_REGION_EXEC != 0,
            shared: wire.flags & PROC_REGION_SHARED != 0,
            offset: wire.offset,
            name,
        });
    }
    Some(ProcessFacts {
        pid: header.pid,
        ppid: header.ppid,
        uid: header.uid,
        gid: header.gid,
        euid: header.euid,
        egid: header.egid,
        threads: header.threads,
        start_ticks: header.start_ticks,
        rss_pages: header.rss_pages,
        locked_bytes: header.locked_bytes,
        fd_slots: header.fd_slots,
        oom_score_adj: header.oom_score_adj,
        regions,
        brk_start: header.brk_start,
        arg_start: header.arg_start,
        arg_end: header.arg_end,
        env_start: header.env_start,
        env_end: header.env_end,
        sig_ignored: header.sig_ignored,
        sig_caught: header.sig_caught,
        no_new_privs: header.no_new_privs,
    })
}

/// Decodes a `PROC_FACTS_OP_THREAD` payload into `ThreadFacts`.
pub fn decode_thread_facts(bytes: &[u8]) -> Option<ThreadFacts> {
    let wire = ProcThreadFactsWire::decode(bytes)?;
    let len = usize::from(wire.comm_len).min(wire.comm.len());
    Some(ThreadFacts {
        tid: wire.tid,
        comm: wire.comm[..len].to_vec(),
        state: wire.state,
        pending_signals: wire.pending_signals,
        blocked_signals: wire.blocked_signals,
        utime_ticks: wire.utime_ticks,
    })
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

pub fn format_maps(regions: &[MapRegion]) -> Vec<u8> {
    let mut out = String::new();
    for region in regions {
        push_map_line(&mut out, region);
    }
    out.into_bytes()
}

pub fn format_smaps(regions: &[MapRegion]) -> Vec<u8> {
    let mut out = String::new();
    for region in regions {
        push_map_line(&mut out, region);
        let _ = writeln!(out, "Size:           {:>8} kB", region.end.saturating_sub(region.start) / 1024);
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

/// `/proc/<pid>/smaps_rollup`: the `[rollup]` header plus only the counters
/// Ring0 can attest for the whole process. Per-mapping `Pss`, `Shared_*`,
/// `Private_*`, `Anonymous`, `Referenced` and `Swap` are not tracked per
/// region, so they are omitted rather than reported as zero; consumers parse
/// the file key by key. `Rss` is the process-wide resident leaf count.
pub fn format_smaps_rollup(process: &ProcessFacts) -> Vec<u8> {
    let mut out = String::new();
    out.push_str("00400000-ffffffff ---p 00000000 00:00 0                          [rollup]\n");
    if let Some(pages) = process.rss_pages {
        let _ = writeln!(out, "Rss:            {:>8} kB", pages.saturating_mul(PAGE_BYTES / 1024));
    }
    let _ = writeln!(out, "Locked:         {:>8} kB", process.locked_bytes / 1024);
    out.into_bytes()
}

// ---- status / stat / statm / comm / oom_score_adj ----------------------------------

fn state_name(state: u8) -> &'static str {
    match state {
        b'R' => "R (running)",
        _ => "S (sleeping)",
    }
}

fn comm_str(comm: &[u8]) -> String {
    String::from_utf8_lossy(comm).into_owned()
}

pub fn format_status(process: &ProcessFacts, thread: &ThreadFacts) -> Vec<u8> {
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
        let _ = writeln!(out, "VmRSS:\t{:>8} kB", pages.saturating_mul(PAGE_BYTES / 1024));
    }
    let _ = writeln!(out, "Threads:\t{}", process.threads);
    let _ = writeln!(out, "SigPnd:\t{:016x}", thread.pending_signals);
    let _ = writeln!(out, "SigBlk:\t{:016x}", thread.blocked_signals);
    let _ = writeln!(out, "SigIgn:\t{:016x}", process.sig_ignored);
    let _ = writeln!(out, "SigCgt:\t{:016x}", process.sig_caught);
    let _ = writeln!(out, "NoNewPrivs:\t{}", u8::from(process.no_new_privs));
    out.into_bytes()
}

pub fn format_stat(process: &ProcessFacts, thread: &ThreadFacts) -> Vec<u8> {
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

pub fn format_statm(process: &ProcessFacts) -> Vec<u8> {
    let pages = |bytes: u64| bytes / PAGE_BYTES;
    let text: u64 = process
        .regions
        .iter()
        .filter(|region| region.exec)
        .fold(0_u64, |total, region| total.saturating_add(region.end.saturating_sub(region.start)));
    let data: u64 = process
        .regions
        .iter()
        .filter(|region| region.write && !region.shared)
        .fold(0_u64, |total, region| total.saturating_add(region.end.saturating_sub(region.start)));
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

pub fn format_comm(thread: &ThreadFacts) -> Vec<u8> {
    let mut out = thread.comm.clone();
    out.push(b'\n');
    out
}

pub fn format_oom_score_adj(process: &ProcessFacts) -> Vec<u8> {
    format!("{}\n", process.oom_score_adj).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring3_decodes_and_formats_process_and_thread_facts() {
        let header = ProcProcessFactsWire {
            pid: 7,
            ppid: 1,
            uid: 1000,
            gid: 1000,
            euid: 1000,
            egid: 1000,
            threads: 2,
            start_ticks: 555,
            rss_pages: Some(10),
            locked_bytes: 4096,
            fd_slots: Some(64),
            oom_score_adj: -250,
            brk_start: 0x4000_0000,
            arg_start: 0x7fff_f000,
            arg_end: 0x7fff_f010,
            env_start: 0x7fff_f010,
            env_end: 0x7fff_f020,
            sig_ignored: 4,
            sig_caught: 8,
            no_new_privs: true,
            region_count: 1,
        };
        let region = ProcMapRegionWire {
            start: 0x40_0000,
            end: 0x40_2000,
            offset: 0,
            flags: PROC_REGION_READ | PROC_REGION_EXEC,
            name: Some(b"/bin/app"),
        };
        let mut buf = [0_u8; 512];
        let mut at = header.encode(&mut buf).unwrap();
        at += region.encode(&mut buf[at..]).unwrap();

        let process = decode_process_facts(&buf[..at]).unwrap();
        assert_eq!(process.fd_slots, Some(64));
        assert_eq!(process.oom_score_adj, -250);
        assert_eq!(format_oom_score_adj(&process), b"-250\n");

        let twire = ProcThreadFactsWire {
            tid: 7,
            state: b'R',
            comm_len: 3,
            comm: *b"app\0\0\0\0\0\0\0\0\0\0\0\0\0",
            pending_signals: 1,
            blocked_signals: 2,
            utime_ticks: Some(9),
        };
        let tn = twire.encode(&mut buf).unwrap();
        let thread = decode_thread_facts(&buf[..tn]).unwrap();

        let status = String::from_utf8(format_status(&process, &thread)).unwrap();
        assert!(status.contains("FDSize:\t64\n"));
        assert!(status.contains("State:\tR (running)\n"));
        assert!(status.contains("NoNewPrivs:\t1\n"));

        let maps = String::from_utf8(format_maps(&process.regions)).unwrap();
        assert!(maps.contains("00400000-00402000 r-xp 00000000 00:00 0"));
        assert!(maps.ends_with(" /bin/app\n"));

        let stat = String::from_utf8(format_stat(&process, &thread)).unwrap();
        assert_eq!(stat.split(' ').count(), 52);

        let statm = String::from_utf8(format_statm(&process)).unwrap();
        assert_eq!(statm, "2 10 0 2 0 0 0\n");
    }

    #[test]
    fn smaps_rollup_has_rollup_header_and_only_attested_counters() {
        let mut process = ProcessFacts {
            pid: 1, ppid: 0, uid: 0, gid: 0, euid: 0, egid: 0, threads: 1, start_ticks: 0,
            rss_pages: Some(3), locked_bytes: 8192, fd_slots: None, oom_score_adj: 0,
            regions: Vec::new(), brk_start: 0, arg_start: 0, arg_end: 0, env_start: 0,
            env_end: 0, sig_ignored: 0, sig_caught: 0, no_new_privs: false,
        };
        let text = String::from_utf8(format_smaps_rollup(&process)).unwrap();
        let first = text.lines().next().unwrap();
        assert!(first.starts_with("00400000-ffffffff ---p 00000000 00:00 0 "));
        assert!(first.ends_with("[rollup]"));
        assert!(text.lines().any(|l| l.split_whitespace().eq(["Rss:", "12", "kB"])));
        assert!(text.lines().any(|l| l.split_whitespace().eq(["Locked:", "8", "kB"])));
        assert!(!text.contains("Pss") && !text.contains("Swap") && !text.contains("Anonymous"));
        process.rss_pages = None;
        assert!(!String::from_utf8(format_smaps_rollup(&process)).unwrap().contains("Rss:"));
        process.rss_pages = Some(u64::MAX);
        assert!(String::from_utf8(format_smaps_rollup(&process)).unwrap().contains("Rss:"));
    }

    #[test]
    fn inverted_or_oversized_regions_saturate_instead_of_wrapping() {
        let region = |start, end| MapRegion {
            start,
            end,
            read: true,
            write: true,
            exec: true,
            shared: false,
            offset: 0,
            name: None,
        };
        let regions = [region(0x2000, 0x1000), region(0, u64::MAX), region(0, u64::MAX)];
        let smaps = String::from_utf8(format_smaps(&regions)).unwrap();
        assert!(smaps.contains("Size:                  0 kB\n"));
        let process = ProcessFacts {
            pid: 1, ppid: 0, uid: 0, gid: 0, euid: 0, egid: 0, threads: 1, start_ticks: 0,
            rss_pages: Some(u64::MAX), locked_bytes: 0, fd_slots: None, oom_score_adj: 0,
            regions: regions.to_vec(), brk_start: 0, arg_start: 0, arg_end: 0, env_start: 0,
            env_end: 0, sig_ignored: 0, sig_caught: 0, no_new_privs: false,
        };
        assert_eq!(process.vm_size_bytes(), u64::MAX);
        assert!(!format_statm(&process).is_empty());
        let thread = ThreadFacts {
            tid: 1, comm: b"x".to_vec(), state: b'S', pending_signals: 0, blocked_signals: 0,
            utime_ticks: None,
        };
        assert!(!format_status(&process, &thread).is_empty());
    }
}
