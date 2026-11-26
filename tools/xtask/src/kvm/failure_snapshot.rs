//! Post-failure diagnostics only. A snapshot cannot turn a failed probe into
//! acceptance, and successful probes never connect to the monitor or pause.
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};
use sha2::{Digest, Sha256};

use super::{Config, KvmLayout, Result};

const READ_BUDGET: Duration = Duration::from_secs(4);
/// Added per guest CPU: each costs two monitor round trips, so a fixed budget
/// would run out on a large guest before the tables behind the registers.
const READ_BUDGET_PER_CPU: Duration = Duration::from_millis(250);
const MAX_TRANSCRIPT_BYTES: usize = 2 * 1024 * 1024;
const MAX_SYMBOL_BYTES: u64 = 32 * 1024;
// Diagnostic adapters for the current 64-bit, cache-aligned IPC slabs. Exact
// linked symbol sizes must agree before any handle-derived row is read.
const IPC_SLABS: &[(&str, u64, u64)] = &[
    ("kernel_ipc_runtime::ipc::ENDPOINTS", 512, 256),
    ("kernel_ipc_runtime::ipc::REPLIES", 512, 768),
    ("kernel_ipc_runtime::ipc::ENDPOINT_MESSAGES", 512, 256),
];
const IPC_SLAB_OCCUPANCY_BYTES: u64 = 128;
const MAX_IPC_ROWS: usize = 64;
const MAX_STACK_ROWS: usize = 24;
/// First address of the kernel half. Heap chunks, `Task` objects and guarded
/// kernel stacks all live above it and are read through the monitor's virtual
/// `x` command (kernel mappings are shared by every address space).
const KERNEL_HALF_BASE: u64 = 0xffff_8000_0000_0000;
/// Newest task slots examined per snapshot; each costs one monitor round trip.
const MAX_TASK_SCAN: usize = 256;
const TASK_LAYOUT_VERSION: u64 = 1;
const TASK_CHUNKS: &str = "kernel_ps::multitask::task_table::TASK_CHUNKS";
const TASK_LAYOUT: &str = "kernel_ps::multitask::task_table::TASK_DIAGNOSTIC_LAYOUT";
const SYMBOLS: &[&str] = &[
    TASK_CHUNKS,
    TASK_LAYOUT,
    "kernel_hal::arch::rtc::RTC_TICKS",
    "kernel_hal::arch::rtc::RTC_TICKS_COMPLETED",
    "kernel_hal::arch::rtc::RTC_SLEEP_LOCK_MISSES",
    "kernel_hal::arch::rtc::RTC_SLEEP_WAITERS",
];

/// Byte offsets inside a heap-allocated `Task`, exported by the kernel as
/// `TASK_DIAGNOSTIC_LAYOUT` so this tool never guesses struct layout.
struct TaskLayout {
    size: u64,
    owner: u64,
    saved_rsp: u64,
    stack_base: u64,
    stack_top: u64,
    reason_id: u64,
}

struct TaskRow {
    slot: usize,
    address: u64,
    owner: u64,
}

/// Keep the original result even if the guest makes later progress. In
/// particular, diagnostics must not re-enter readiness or publish success.
pub(super) fn after_failed_probe<T>(result: Result<T>, diagnostic: impl FnOnce()) -> Result<T> {
    if result.is_err() {
        diagnostic();
    }
    result
}

struct Monitor {
    stream: UnixStream,
    deadline: Instant,
    transcript: Vec<u8>,
}

impl Monitor {
    fn connect(path: &Path, vcpus: u8) -> Result<Self> {
        let stream = UnixStream::connect(path).context("connect failed guest monitor")?;
        Self::over(stream, vcpus)
    }

    fn over(stream: UnixStream, vcpus: u8) -> Result<Self> {
        let mut monitor = Self {
            stream,
            deadline: Instant::now() + READ_BUDGET + READ_BUDGET_PER_CPU * u32::from(vcpus),
            transcript: Vec::new(),
        };
        monitor.prompt()?;
        Ok(monitor)
    }

    fn remaining(&self) -> Result<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "failure snapshot read budget expired");
        Ok(remaining)
    }

    fn prompt(&mut self) -> Result<()> {
        let start = self.transcript.len();
        loop {
            self.stream.set_read_timeout(Some(self.remaining()?))?;
            let mut bytes = [0; 4096];
            let count = self.stream.read(&mut bytes)?;
            ensure!(count != 0, "failed guest monitor closed before prompt");
            ensure!(
                self.transcript.len() + count <= MAX_TRANSCRIPT_BYTES,
                "failure snapshot transcript limit exceeded"
            );
            self.transcript.extend_from_slice(&bytes[..count]);
            if self.transcript[start..]
                .windows(7)
                .any(|window| window == b"(qemu) ")
            {
                return Ok(());
            }
        }
    }

    fn command(&mut self, command: &str) -> Result<()> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.transcript
            .extend_from_slice(format!("\nCOMMAND {command}\n").as_bytes());
        self.stream.write_all(command.as_bytes())?;
        self.stream.write_all(b"\n")?;
        self.prompt()
    }

    fn registers(&mut self, vcpus: u8) -> Result<()> {
        // Any guest size: the monitor refuses a CPU the guest does not have,
        // and a hard-coded list silently emptied every 16-vCPU snapshot.
        ensure!(vcpus >= 1, "snapshot needs at least one CPU");
        self.command("stop")?;
        for cpu in 0..vcpus {
            self.command(&format!("cpu {cpu}"))?;
            self.command("info registers")?;
        }
        Ok(())
    }

    fn words(&self, name: &str) -> Result<Vec<u64>> {
        let text = String::from_utf8_lossy(&self.transcript);
        let label = format!("\nSYMBOL {name}\n");
        let (_, data) = text
            .split_once(&label)
            .context("snapshot table is absent")?;
        let mut words = Vec::new();
        for line in data.split("\nSYMBOL ").next().unwrap_or_default().lines() {
            let Some((address, values)) = line.split_once(':') else {
                continue;
            };
            if u64::from_str_radix(address.trim().trim_start_matches("0x"), 16).is_err() {
                continue;
            }
            for value in values
                .split_whitespace()
                .filter_map(|value| value.strip_prefix("0x"))
            {
                words.push(u64::from_str_radix(value, 16)?);
            }
        }
        Ok(words)
    }

    fn layout(&self) -> Result<TaskLayout> {
        let words = self.words(TASK_LAYOUT)?;
        ensure!(
            words.len() == 9 && words[0] == TASK_LAYOUT_VERSION && words[1] == 64,
            "task diagnostic layout changed: {words:x?}"
        );
        let layout = TaskLayout {
            size: words[2],
            owner: words[3],
            saved_rsp: words[4],
            stack_base: words[5],
            stack_top: words[6],
            reason_id: words[7],
        };
        ensure!(
            [
                layout.owner,
                layout.saved_rsp,
                layout.stack_base,
                layout.stack_top,
                layout.reason_id
            ]
            .iter()
            .all(|offset| offset + 8 <= layout.size),
            "task diagnostic offsets exceed the task size"
        );
        Ok(layout)
    }

    /// Reads `bytes` of kernel-half memory through the guest's own page tables.
    fn virtual_words(&mut self, name: &str, address: u64, bytes: u64) -> Result<()> {
        ensure!(
            bytes > 0 && bytes <= MAX_SYMBOL_BYTES && address >= KERNEL_HALF_BASE,
            "invalid snapshot virtual read"
        );
        ensure!(
            address.checked_add(bytes).is_some(),
            "snapshot virtual read overflows"
        );
        self.transcript
            .extend_from_slice(format!("\nSYMBOL {name}\n").as_bytes());
        self.command(&format!("x /{}gx 0x{address:x}", bytes.div_ceil(8)))
    }

    fn word_at(&mut self, name: &str, address: u64) -> Result<u64> {
        self.virtual_words(name, address, 8)?;
        self.words(name)?
            .first()
            .copied()
            .with_context(|| format!("monitor returned no word for {name}"))
    }

    /// Owner words of the newest `MAX_TASK_SCAN` allocated slots, newest first
    /// (later slots are the short-lived workers). The `Task` objects are heap
    /// allocations reached only through the published chunk directory.
    fn task_rows(&mut self, layout: &TaskLayout) -> Result<Vec<TaskRow>> {
        let chunks = self.words(TASK_CHUNKS)?;
        let mut rows = Vec::new();
        let mut skipped = 0;
        for (chunk_idx, &chunk) in chunks.iter().enumerate().rev() {
            if chunk == 0 {
                continue;
            }
            if rows.len() >= MAX_TASK_SCAN {
                skipped += 64;
                continue;
            }
            let label = format!("TASK_CHUNK_{chunk_idx}");
            self.virtual_words(&label, chunk, 64 * 8)?;
            let tasks = self.words(&label)?;
            ensure!(tasks.len() >= 64, "incomplete task chunk {chunk_idx}");
            for (offset, &task) in tasks[..64].iter().enumerate().rev() {
                if task == 0 {
                    continue;
                }
                if rows.len() >= MAX_TASK_SCAN {
                    skipped += 1;
                    continue;
                }
                let slot = chunk_idx * 64 + offset;
                let owner = self.word_at(&format!("TASK_{slot}_owner"), task + layout.owner)?;
                rows.push(TaskRow {
                    slot,
                    address: task,
                    owner,
                });
            }
        }
        if skipped != 0 {
            self.transcript.extend_from_slice(
                format!("\ntask_census=truncated scanned={} older_slots_skipped={skipped}\n", rows.len())
                    .as_bytes(),
            );
        }
        Ok(rows)
    }

    fn published_stacks(&mut self, layout: &TaskLayout, rows: &[TaskRow]) -> Result<()> {
        // Only original published usable stack bounds, never a guessed user or
        // heap address. Local frames expose retrying ready continuations that
        // the blocked-only census missed. Current needs the actual CPU RSP, not
        // its stale saved frame. No unwinder follows pointers; both states share
        // the same row budget.
        let mut emitted = 0;
        for row in rows {
            let label = match row.owner & 15 {
                1 => "READY_STACK",
                5 if (row.owner >> 13) & 7 != 2 => "BLOCKED_STACK",
                _ => continue,
            };
            let slot = row.slot;
            let rsp = self.word_at(&format!("TASK_{slot}_rsp"), row.address + layout.saved_rsp)?;
            let base = self.word_at(&format!("TASK_{slot}_base"), row.address + layout.stack_base)?;
            let top = self.word_at(&format!("TASK_{slot}_top"), row.address + layout.stack_top)?;
            let Some((address, size)) = diagnostic_stack_range(rsp, base, top) else {
                continue;
            };
            if emitted == MAX_STACK_ROWS {
                break;
            }
            self.virtual_words(&format!("{label}_slot_{slot}_rsp_{rsp:x}"), address, size)?;
            emitted += 1;
        }
        Ok(())
    }

    fn ipc_row(
        &mut self,
        tables: &BTreeMap<String, (u64, u64)>,
        table: usize,
        handle: u64,
        seen: &mut BTreeSet<(usize, u64)>,
    ) -> Result<Option<Vec<u64>>> {
        let (name, capacity, stride) = IPC_SLABS[table];
        let Some(index) = diagnostic_handle_index(handle, capacity) else {
            return Ok(None);
        };
        if !seen.insert((table, index)) {
            return Ok(None);
        }
        ensure!(
            seen.len() <= MAX_IPC_ROWS,
            "IPC diagnostic row budget exceeded"
        );
        let &(base, size) = tables.get(name).context("linked IPC slab is absent")?;
        // The C-layout slab begins with slots; the tail is the occupancy
        // bitmap and one 64-byte insertion hint per tracked CPU.
        let hints = size.saturating_sub(capacity * stride + IPC_SLAB_OCCUPANCY_BYTES);
        ensure!(
            hints >= 64 && hints % 64 == 0 && hints <= 64 * 64,
            "linked IPC slab layout changed: {name}"
        );
        let address = base
            .checked_add(index * stride)
            .context("IPC diagnostic address overflow")?;
        let label = format!("IPC_ROW_{name}_handle_{handle:x}");
        self.symbol(&label, address, stride)?;
        let words = self.words(&label)?;
        ensure!(words.len() == (stride / 8) as usize, "incomplete IPC row");
        Ok(Some(words))
    }

    fn ipc_waits(
        &mut self,
        tables: &BTreeMap<String, (u64, u64)>,
        layout: &TaskLayout,
        rows: &[TaskRow],
    ) -> Result<()> {
        let mut seen = BTreeSet::new();
        // Never follow guest/heap pointers. A closed handle's low index selects
        // only a row inside an exact-size linked slab. Payload words that look
        // like message/endpoint handles are labelled raw candidates, not live
        // identities or permission to access any other guest memory.
        for row in rows {
            if row.owner & 15 != 5 {
                continue;
            }
            let kind = (row.owner >> 13) & 7;
            if kind != 2 && kind != 3 {
                continue;
            }
            let handle = self.word_at(
                &format!("TASK_{}_reason", row.slot),
                row.address + layout.reason_id,
            )?;
            if kind == 2 {
                let _ = self.ipc_row(tables, 0, handle, &mut seen)?;
                continue;
            }
            let Some(reply) = self.ipc_row(tables, 1, handle, &mut seen)? else {
                continue;
            };
            for candidate in reply {
                let Some(message) = self.ipc_row(tables, 2, candidate, &mut seen)? else {
                    continue;
                };
                for endpoint_candidate in message {
                    let _ = self.ipc_row(tables, 0, endpoint_candidate, &mut seen)?;
                }
            }
        }
        Ok(())
    }

    fn symbol(&mut self, name: &str, address: u64, size: u64) -> Result<()> {
        ensure!(
            size > 0 && size <= MAX_SYMBOL_BYTES,
            "invalid snapshot symbol size"
        );
        // These are linked kernel scalar tables in the identity-mapped image,
        // not pointers supplied by a guest or heap contents followed recursively.
        ensure!(
            address
                .checked_add(size)
                .is_some_and(|end| end <= 0x1_0000_0000),
            "snapshot symbol is outside kernel identity mapping"
        );
        self.transcript
            .extend_from_slice(format!("\nSYMBOL {name}\n").as_bytes());
        self.command(&format!("xp /{}gx 0x{address:x}", size.div_ceil(8)))
    }
}

fn diagnostic_handle_index(handle: u64, capacity: u64) -> Option<u64> {
    let encoded = handle & 0xffff;
    (handle >> 16 != 0 && encoded != 0 && encoded <= capacity).then(|| encoded - 1)
}

pub(super) struct CapturedSnapshot {
    pub path: std::path::PathBuf,
    pub collection: Result<()>,
}

/// Capture only after readiness/recovery has already failed. The caller still
/// stops both guests immediately afterwards; no resume or new acceptance pass.
/// Incomplete captures are explicitly labelled and remain useful diagnostics.
pub(super) fn capture(
    config: &Config,
    layout: &KvmLayout,
    vcpus: u8,
) -> Result<CapturedSnapshot> {
    let kernel = config.artifact_nucleus_elf_path();
    let kernel_sha = format!("{:x}", Sha256::digest(std::fs::read(&kernel)?));
    let mut text = format!(
        "diagnostic_only=true\nprobe_result=failed\nkernel_sha256={kernel_sha}\nvcpus={vcpus}\n"
    );
    let collection = (|| -> Result<()> {
        let mut monitor = Monitor::connect(&layout.rustos_monitor, vcpus)?;
        let result = (|| -> Result<()> {
            monitor.registers(vcpus)?;
            // This optional diagnostic dependency is itself time-bounded. A
            // missing nm/timeout cannot replace the original probe failure.
            let symbols = Command::new("timeout")
                .args(["2s", "nm", "-S", "-C"])
                .arg(&kernel)
                .output()
                .context("read exact linked kernel symbol table")?;
            ensure!(symbols.status.success(), "kernel symbol inspection failed");
            let mut captured = 0;
            let mut tables = BTreeMap::new();
            for line in String::from_utf8_lossy(&symbols.stdout).lines() {
                let mut fields = line.splitn(4, ' ').filter(|field| !field.is_empty());
                let (Some(address), Some(size), Some(kind), Some(name)) =
                    (fields.next(), fields.next(), fields.next(), fields.next())
                else {
                    continue;
                };
                if IPC_SLABS.iter().any(|&(table, _, _)| table == name) {
                    tables.insert(
                        name.to_owned(),
                        (
                            u64::from_str_radix(address, 16)?,
                            u64::from_str_radix(size, 16)?,
                        ),
                    );
                }
                if SYMBOLS.contains(&name) && ["B", "b", "D", "d", "R", "r"].contains(&kind) {
                    monitor.symbol(
                        name,
                        u64::from_str_radix(address, 16)?,
                        u64::from_str_radix(size, 16)?,
                    )?;
                    captured += 1;
                }
            }
            ensure!(
                captured == SYMBOLS.len(),
                "only {captured}/{} kernel tables captured",
                SYMBOLS.len()
            );
            let layout = monitor.layout()?;
            let rows = monitor.task_rows(&layout)?;
            monitor.ipc_waits(&tables, &layout, &rows)?;
            monitor.published_stacks(&layout, &rows)?;
            Ok(())
        })();
        text.push_str(&String::from_utf8_lossy(&monitor.transcript));
        result
    })();
    write_capture(&layout.run_dir, text, collection)
}

fn write_capture(run_dir: &Path, mut text: String, collection: Result<()>) -> Result<CapturedSnapshot> {
    match &collection {
        Ok(()) => text.push_str("\ncollection=complete\n"),
        Err(error) => text.push_str(&format!("\ncollection=partial error={error:#}\n")),
    }
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = run_dir.join(format!("failure-snapshot-{nonce}.log"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(text.as_bytes())?;
    Ok(CapturedSnapshot { path, collection })
}

/// A post-failure read window inside the original usable kernel-stack bounds.
/// Stacks are guarded virtual mappings, so the window is a virtual address read
/// through the guest's page tables. Never read the guard page, a user stack, or
/// an arbitrary pointer from a frame.
fn diagnostic_stack_range(rsp: u64, base: u64, top: u64) -> Option<(u64, u64)> {
    if base < KERNEL_HALF_BASE || rsp < base || rsp >= top || rsp & 7 != 0 {
        return None;
    }
    let size = (top - rsp).min(1024) & !7;
    (size != 0).then_some((rsp, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    #[test]
    fn snapshot_result_and_artifact_agree_on_collection_completeness() {
        let dir = tempfile::tempdir().unwrap();
        for (collection, footer) in [
            (Ok(()), "collection=complete"),
            (
                Err(anyhow::anyhow!("monitor disconnected")),
                "collection=partial error=monitor disconnected",
            ),
        ] {
            let complete = collection.is_ok();
            let snapshot = write_capture(dir.path(), "diagnostic_only=true\n".to_owned(), collection)
                .unwrap();
            assert_eq!(snapshot.collection.is_ok(), complete);
            let text = std::fs::read_to_string(&snapshot.path).unwrap();
            assert!(text.ends_with(&format!("{footer}\n")), "{text}");
        }
    }

    #[test]
    fn monitor_uses_the_remaining_budget_instead_of_a_half_second_idle_timeout() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let mut monitor = Monitor {
            stream: client,
            deadline: Instant::now() + Duration::from_secs(2),
            transcript: Vec::new(),
        };
        std::thread::scope(|scope| {
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(600));
                server.write_all(b"QEMU monitor\n(qemu) ").unwrap();
            });
            monitor.prompt().unwrap();
        });
        monitor.deadline = Instant::now();
        assert!(monitor.prompt().is_err());
    }

    #[test]
    fn blocked_stack_diagnostic_stays_within_original_usable_kernel_range() {
        let base = KERNEL_HALF_BASE + 0x2000;
        let top = base + 0x4000;
        assert_eq!(diagnostic_stack_range(base, base, top), Some((base, 1024)));
        assert_eq!(
            diagnostic_stack_range(top - 8, base, top),
            Some((top - 8, 8))
        );
        for rsp in [base - 8, base + 1, top, top + 8, 0x2000, u64::MAX] {
            assert_eq!(diagnostic_stack_range(rsp, base, top), None);
        }
        // A stack below the kernel half is never read, whatever its saved RSP.
        assert_eq!(diagnostic_stack_range(0x2000, 0x2000, 0x6000), None);
        assert_eq!(diagnostic_stack_range(top - 8, base, top - 1), None);
    }

    fn preload(monitor: &mut Monitor, name: &str, words: &[u64]) {
        monitor
            .transcript
            .extend_from_slice(format!("\nSYMBOL {name}\n0000000000001000:").as_bytes());
        for value in words {
            monitor
                .transcript
                .extend_from_slice(format!(" 0x{value:016x}").as_bytes());
        }
        monitor.transcript.push(b'\n');
    }

    #[test]
    fn failed_probe_reads_ready_and_blocked_frames_but_not_current_or_endpoint_waiters() {
        const CHUNK: u64 = KERNEL_HALF_BASE + 0x10_0000;
        const TASKS: u64 = KERNEL_HALF_BASE + 0x20_0000;
        const STACKS: u64 = KERNEL_HALF_BASE + 0x40_0000;
        // owner, saved_rsp, stack_base, stack_top, wait_reason_id (distinct 8-byte fields).
        const LAYOUT: [u64; 9] = [1, 64, 0x400, 0, 0x40, 0x48, 0x50, 0x58, 0x60];
        let task = |slot: u64| TASKS + slot * 0x400;
        let base = |slot: u64| STACKS + slot * 0x10000;
        let mut memory = BTreeMap::new();
        for (slot, owner) in [(0, 1), (1, 3), (2, 5 | (2 << 13)), (3, 5), (4, 7)] {
            memory.insert(CHUNK + slot * 8, task(slot));
            memory.insert(task(slot), owner);
            memory.insert(task(slot) + 0x40, base(slot) + 0x1000);
            memory.insert(task(slot) + 0x48, base(slot));
            memory.insert(task(slot) + 0x50, base(slot) + 0x4000);
            memory.insert(task(slot) + 0x58, 0);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("monitor.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream.write_all(b"QEMU monitor\n(qemu) ").unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut commands = Vec::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let line = line.trim().to_owned();
                // `x /<n>gx 0x<address>`: answer from the fake guest memory.
                let (count, address) = line
                    .strip_prefix("x /")
                    .and_then(|rest| rest.split_once("gx 0x"))
                    .map(|(count, address)| {
                        (
                            count.parse::<u64>().unwrap(),
                            u64::from_str_radix(address, 16).unwrap(),
                        )
                    })
                    .unwrap();
                let mut reply = String::new();
                for word in 0..count {
                    let address = address + word * 8;
                    let value = memory.get(&address).copied().unwrap_or(0);
                    reply.push_str(&format!("{address:016x}: 0x{value:016x}\n"));
                }
                commands.push(line);
                stream.write_all(reply.as_bytes()).unwrap();
                stream.write_all(b"(qemu) ").unwrap();
            }
            commands
        });
        let mut monitor = Monitor::connect(&path, 1).unwrap();
        // Earlier static-symbol dumps, not guest authority.
        preload(&mut monitor, TASK_LAYOUT, &LAYOUT);
        let mut directory = vec![0; 4096];
        directory[0] = CHUNK;
        preload(&mut monitor, TASK_CHUNKS, &directory);
        let layout = monitor.layout().unwrap();
        let rows = monitor.task_rows(&layout).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.slot).collect::<Vec<_>>(),
            [4, 3, 2, 1, 0]
        );
        monitor.published_stacks(&layout, &rows).unwrap();
        drop(monitor);
        let commands = server.join().unwrap();
        let stack_reads: Vec<_> = commands
            .iter()
            .filter(|command| command.starts_with("x /128gx"))
            .collect();
        assert_eq!(
            stack_reads,
            [
                &format!("x /128gx 0x{:x}", base(3) + 0x1000),
                &format!("x /128gx 0x{:x}", base(0) + 0x1000),
            ]
        );
        // Current, endpoint-wait and retired slots never had a frame field read.
        for slot in [1, 2, 4] {
            assert!(
                !commands
                    .iter()
                    .any(|command| command.ends_with(&format!("{:x}", task(slot) + 0x40))),
                "slot {slot}"
            );
        }
    }

    #[test]
    fn ipc_diagnostic_indices_are_closed_for_full_generation_domain() {
        assert_eq!(diagnostic_handle_index(0x10001, 128), Some(0));
        assert_eq!(diagnostic_handle_index(0xab0080, 128), Some(127));
        assert_eq!(diagnostic_handle_index(0xffff_ffff_ffff_0001, 128), Some(0));
        assert_eq!(
            diagnostic_handle_index(0xffff_ffff_ffff_0080, 128),
            Some(127)
        );
        // Even pointer-looking words can only select a CLOSED slab index;
        // their value is never interpreted as a physical/virtual address.
        assert_eq!(diagnostic_handle_index(0xffff800000100001, 128), Some(0));
        for handle in [0, 1, 0x10000, 0x10081, u64::MAX] {
            assert_eq!(diagnostic_handle_index(handle, 128), None);
        }
    }

    #[test]
    fn failed_probe_diagnostic_preserves_original_error_and_never_runs_on_success() {
        let success = after_failed_probe(Ok(42), || panic!("successful guest must not pause"));
        assert_eq!(success.unwrap(), 42);
        let mut calls = 0;
        let failure: Result<u8> =
            after_failed_probe(Err(anyhow::anyhow!("original deadline")), || {
                calls += 1; // Even later readiness cannot replace the latched error.
            });
        assert_eq!(calls, 1);
        assert_eq!(failure.unwrap_err().to_string(), "original deadline");
    }

    #[test]
    fn failed_probe_monitor_collects_registers_and_exact_bounded_symbol_without_resume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("monitor.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream.write_all(b"QEMU monitor\n(qemu) ").unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            for expected in ["stop", "cpu 0", "info registers", "xp /2gx 0x1000"] {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert_eq!(line.trim(), expected);
                if expected.starts_with("xp ") {
                    stream
                        .write_all(
                            b"0000000000001000: 0x0000000000004005 0x0000000000010002\n(qemu) ",
                        )
                        .unwrap();
                } else {
                    stream.write_all(b"snapshot-data\n(qemu) ").unwrap();
                }
            }
            let mut extra = String::new();
            assert_eq!(
                reader.read_line(&mut extra).unwrap(),
                0,
                "refused symbol emitted monitor IO: {extra}"
            );
        });
        let mut monitor = Monitor::connect(&path, 1).unwrap();
        monitor.registers(1).unwrap();
        monitor.symbol("original-owner", 0x1000, 16).unwrap();
        assert_eq!(monitor.words("original-owner").unwrap(), [0x4005, 0x10002]);
        assert!(!String::from_utf8_lossy(&monitor.transcript).contains("COMMAND cont"));
        let tables = BTreeMap::from([(
            IPC_SLABS[0].0.to_owned(),
            (0x1000, IPC_SLABS[0].1 * IPC_SLABS[0].2 + IPC_SLAB_OCCUPANCY_BYTES + 64),
        )]);
        let mut seen = (0..MAX_IPC_ROWS as u64).map(|index| (0, index)).collect();
        assert!(monitor.ipc_row(&tables, 0, 0x10065, &mut seen).is_err());
        assert!(
            monitor
                .symbol("too-large", 0x1000, MAX_SYMBOL_BYTES + 1)
                .is_err()
        );
        assert!(monitor.symbol("foreign-pointer", 0x1_0000_0000, 8).is_err());
        assert!(monitor.symbol("overflowing-pointer", u64::MAX, 8).is_err());
        drop(monitor);
        server.join().unwrap();
    }
}

#[cfg(test)]
mod wide_guest_tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    use super::Monitor;

    /// A monitor that answers every command with a register block and a prompt.
    fn fake_monitor(mut server: UnixStream) -> std::thread::JoinHandle<Vec<String>> {
        std::thread::spawn(move || {
            let mut commands = Vec::new();
            server.write_all(b"QEMU monitor\n(qemu) ").unwrap();
            let mut pending = String::new();
            let mut bytes = [0; 256];
            loop {
                let count = server.read(&mut bytes).unwrap_or(0);
                if count == 0 {
                    return commands;
                }
                pending.push_str(&String::from_utf8_lossy(&bytes[..count]));
                while let Some((line, rest)) = pending.split_once('\n') {
                    commands.push(line.to_owned());
                    let reply = format!("{line}\nRAX=0\n(qemu) ");
                    server.write_all(reply.as_bytes()).unwrap();
                    pending = rest.to_owned();
                }
            }
        })
    }

    #[test]
    fn registers_are_collected_for_every_cpu_of_a_sixteen_vcpu_guest() {
        let (client, server) = UnixStream::pair().unwrap();
        let guest = fake_monitor(server);
        let mut monitor = Monitor::over(client, 16).unwrap();
        monitor.registers(16).unwrap();
        drop(monitor);
        let commands = guest.join().unwrap();
        assert_eq!(commands.first().map(String::as_str), Some("stop"));
        for cpu in 0..16 {
            assert!(commands.contains(&format!("cpu {cpu}")), "cpu {cpu}");
        }
        assert_eq!(
            commands.iter().filter(|c| *c == "info registers").count(),
            16
        );
    }

    #[test]
    fn a_guest_without_cpus_is_rejected_rather_than_snapshotted_empty() {
        let (client, server) = UnixStream::pair().unwrap();
        let guest = fake_monitor(server);
        let mut monitor = Monitor::over(client, 1).unwrap();
        assert!(monitor.registers(0).is_err());
        drop(monitor);
        guest.join().unwrap();
    }
}
