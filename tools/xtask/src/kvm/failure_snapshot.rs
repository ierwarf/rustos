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
const MAX_TRANSCRIPT_BYTES: usize = 512 * 1024;
const MAX_SYMBOL_BYTES: u64 = 16 * 1024;
// Diagnostic adapters for the current 64-bit, cache-aligned IPC slabs. Exact
// linked symbol sizes must agree before any handle-derived row is read.
const IPC_SLABS: &[(&str, u64, u64)] = &[
    ("kernel_ipc_runtime::ipc::ENDPOINTS", 512, 192),
    ("kernel_ipc_runtime::ipc::REPLIES", 128, 704),
    ("kernel_ipc_runtime::ipc::ENDPOINT_MESSAGES", 128, 192),
];
const MAX_IPC_ROWS: usize = 64;
const MAX_STACK_ROWS: usize = 24;
const DIRECT_MAP_BASE: u64 = 0xffff_8000_0000_0000;
const SYMBOLS: &[&str] = &[
    "kernel_ps::multitask::cpu_local::CURRENT_TASK_SLOTS",
    "kernel_ps::multitask::cpu_local::TRANSITION_ACTIVE",
    "kernel_ps::multitask::current_identity::IDENTITIES",
    "kernel_ps::multitask::scheduler::runqueue::OWNER_WORDS",
    "kernel_ps::multitask::scheduler::runqueue::KERNEL_STACK_BASE",
    "kernel_ps::multitask::scheduler::runqueue::KERNEL_STACK_TOP",
    "kernel_ps::multitask::scheduler::runqueue::wait::REASON_ID",
    "kernel_ps::multitask::scheduler::runqueue::wait::BLOCKED_SINCE_TICKS",
    "kernel_ps::multitask::scheduler::runqueue::wait::epoch::EPOCHS",
    "kernel_ps::multitask::scheduler::frame_publication::OWNER_GENERATION",
    "kernel_ps::multitask::scheduler::frame_publication::SAVED_RSP",
    "kernel_ps::multitask::scheduler::frame_publication::FLAGS",
    "kernel_ps::multitask::scheduler::frame_publication::VERSION",
    "kernel_ps::multitask::scheduler::runqueue::affinity_payload::AFFINITIES",
    "kernel_ps::multitask::scheduler::donation_ledger::CLASS_PUBLICATION_SEQUENCE",
    "kernel_ps::multitask::scheduler::dispatch_policy::ATOMIC_ACTIVATION_PENDING",
    "kernel_hal::arch::rtc::RTC_TICKS",
    "kernel_hal::arch::rtc::RTC_TICKS_COMPLETED",
    "kernel_hal::arch::rtc::RTC_SLEEP_LOCK_MISSES",
    "kernel_hal::arch::rtc::RTC_SLEEP_WAITERS",
];

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
    fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path).context("connect failed guest monitor")?;
        let mut monitor = Self {
            stream,
            deadline: Instant::now() + READ_BUDGET,
            transcript: Vec::new(),
        };
        monitor.prompt()?;
        Ok(monitor)
    }

    fn remaining(&self) -> Result<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "failure snapshot read budget expired");
        Ok(remaining.min(Duration::from_millis(500)))
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
        ensure!(
            [1, 2, 4, 8].contains(&vcpus),
            "unsupported snapshot CPU count"
        );
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

    fn published_stacks(&mut self) -> Result<()> {
        let owners = self.words("kernel_ps::multitask::scheduler::runqueue::OWNER_WORDS")?;
        let rsps = self.words("kernel_ps::multitask::scheduler::frame_publication::SAVED_RSP")?;
        let bases = self.words("kernel_ps::multitask::scheduler::runqueue::KERNEL_STACK_BASE")?;
        let tops = self.words("kernel_ps::multitask::scheduler::runqueue::KERNEL_STACK_TOP")?;
        ensure!(
            [owners.len(), rsps.len(), bases.len(), tops.len()] == [128; 4],
            "incomplete kernel-stack census"
        );
        // Only original published usable stack bounds, never a guessed user or
        // heap address. Local frames expose retrying ready continuations that
        // the blocked-only census missed. Current needs the actual CPU RSP, not
        // its stale saved frame. Later slots first retain short-lived workers.
        // No unwinder follows pointers; both states share the same row budget.
        let mut rows = 0;
        for slot in (0..owners.len()).rev() {
            let owner = owners[slot];
            let label = match owner & 15 {
                1 => "READY_STACK",
                5 if (owner >> 13) & 7 != 2 => "BLOCKED_STACK",
                _ => continue,
            };
            let Some((physical, size)) =
                diagnostic_stack_range(rsps[slot], bases[slot], tops[slot])
            else {
                continue;
            };
            if rows == MAX_STACK_ROWS {
                break;
            }
            self.symbol(
                &format!("{label}_slot_{slot}_rsp_{:x}", rsps[slot]),
                physical,
                size,
            )?;
            rows += 1;
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
        ensure!(
            size == capacity * stride + 64,
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

    fn ipc_waits(&mut self, tables: &BTreeMap<String, (u64, u64)>) -> Result<()> {
        let owners = self.words("kernel_ps::multitask::scheduler::runqueue::OWNER_WORDS")?;
        let reasons = self.words("kernel_ps::multitask::scheduler::runqueue::wait::REASON_ID")?;
        ensure!(
            owners.len() == 128 && reasons.len() == owners.len(),
            "incomplete wait census"
        );
        let mut seen = BTreeSet::new();
        // Never follow guest/heap pointers. A closed handle's low index selects
        // only a row inside an exact-size linked slab. Payload words that look
        // like message/endpoint handles are labelled raw candidates, not live
        // identities or permission to access any other guest memory.
        for (owner, handle) in owners.into_iter().zip(reasons) {
            if owner & 15 != 5 {
                continue;
            }
            match (owner >> 13) & 7 {
                2 => {
                    let _ = self.ipc_row(tables, 0, handle, &mut seen)?;
                }
                3 => {
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
                _ => {}
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

/// Capture only after readiness/recovery has already failed. The caller still
/// stops both guests immediately afterwards; no resume or new acceptance pass.
/// Incomplete captures are explicitly labelled and remain useful diagnostics.
pub(super) fn capture(
    config: &Config,
    layout: &KvmLayout,
    vcpus: u8,
) -> Result<std::path::PathBuf> {
    let kernel = config.artifact_nucleus_elf_path();
    let kernel_sha = format!("{:x}", Sha256::digest(std::fs::read(&kernel)?));
    let mut text = format!(
        "diagnostic_only=true\nprobe_result=failed\nkernel_sha256={kernel_sha}\nvcpus={vcpus}\n"
    );
    let collection = (|| -> Result<()> {
        let mut monitor = Monitor::connect(&layout.rustos_monitor)?;
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
                if SYMBOLS.contains(&name) && ["B", "b", "D", "d"].contains(&kind) {
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
            monitor.ipc_waits(&tables)?;
            monitor.published_stacks()?;
            Ok(())
        })();
        text.push_str(&String::from_utf8_lossy(&monitor.transcript));
        result
    })();
    match collection {
        Ok(()) => text.push_str("\ncollection=complete\n"),
        Err(error) => text.push_str(&format!("\ncollection=partial error={error:#}\n")),
    }
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = layout.run_dir.join(format!("failure-snapshot-{nonce}.log"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(text.as_bytes())?;
    Ok(path)
}

/// A post-failure read window inside the original usable kernel-stack bounds.
/// Never read the guard page, a user stack, or an arbitrary pointer from a frame.
fn diagnostic_stack_range(rsp: u64, base: u64, top: u64) -> Option<(u64, u64)> {
    if base < DIRECT_MAP_BASE || rsp < base || rsp >= top || rsp & 7 != 0 {
        return None;
    }
    let physical = rsp.checked_sub(DIRECT_MAP_BASE)?;
    let physical_top = top.checked_sub(DIRECT_MAP_BASE)?;
    if physical_top > 0x1_0000_0000 {
        return None;
    }
    let size = (top - rsp).min(1024) & !7;
    (size != 0).then_some((physical, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    #[test]
    fn blocked_stack_diagnostic_stays_within_original_usable_kernel_range() {
        let base = DIRECT_MAP_BASE + 0x2000;
        let top = base + 0x4000;
        assert_eq!(
            diagnostic_stack_range(base, base, top),
            Some((0x2000, 1024))
        );
        assert_eq!(
            diagnostic_stack_range(top - 8, base, top),
            Some((0x5ff8, 8))
        );
        for rsp in [base - 8, base + 1, top, top + 8, 0x2000, u64::MAX] {
            assert_eq!(diagnostic_stack_range(rsp, base, top), None);
        }
        assert_eq!(diagnostic_stack_range(0x2000, 0x2000, 0x6000), None);
        assert_eq!(
            diagnostic_stack_range(base, base, DIRECT_MAP_BASE + 0x1_0000_0008),
            None
        );
        assert_eq!(diagnostic_stack_range(top - 8, base, top - 1), None);
    }

    #[test]
    fn failed_probe_reads_ready_and_blocked_frames_but_not_current_or_endpoint_waiters() {
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
                commands.push(line.trim().to_owned());
                stream
                    .write_all(b"0000000000004000: 0x0000000000000000\n(qemu) ")
                    .unwrap();
            }
            commands
        });
        let mut monitor = Monitor::connect(&path).unwrap();
        let base = DIRECT_MAP_BASE + 0x4000;
        let mut owners = [0_u64; 128];
        owners[127] = 1; // Local: a published continuation, not a live CPU RSP.
        owners[126] = 3; // Current: its saved frame is not the actual CPU frame.
        owners[125] = 5 | (2 << 13); // Idle endpoint wait: omit as before.
        owners[124] = 5; // Blocked kernel continuation.
        owners[123] = 7; // Retired/invalid ownership cannot authorize a read.
        let mut rsps = [base; 128];
        rsps[127] = base + 0x1000;
        // These are explicit earlier HMP table inputs, not guest authority.
        for (name, words) in [
            (
                "kernel_ps::multitask::scheduler::runqueue::OWNER_WORDS",
                owners,
            ),
            (
                "kernel_ps::multitask::scheduler::frame_publication::SAVED_RSP",
                rsps,
            ),
            (
                "kernel_ps::multitask::scheduler::runqueue::KERNEL_STACK_BASE",
                [base; 128],
            ),
            (
                "kernel_ps::multitask::scheduler::runqueue::KERNEL_STACK_TOP",
                [base + 0x4000; 128],
            ),
        ] {
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
        monitor.published_stacks().unwrap();
        drop(monitor);
        assert_eq!(
            server.join().unwrap(),
            ["xp /128gx 0x5000", "xp /128gx 0x4000"]
        );
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
        let mut monitor = Monitor::connect(&path).unwrap();
        monitor.registers(1).unwrap();
        monitor.symbol("original-owner", 0x1000, 16).unwrap();
        assert_eq!(monitor.words("original-owner").unwrap(), [0x4005, 0x10002]);
        assert!(!String::from_utf8_lossy(&monitor.transcript).contains("COMMAND cont"));
        let tables = BTreeMap::from([(IPC_SLABS[0].0.to_owned(), (0x1000, 512 * 192 + 64))]);
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
