pub mod boot_trace;
mod kdiag_macros;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use boot_protocol::BootInfo;
use core::fmt;
#[cfg(rustos_debug_print_enabled)]
use core::fmt::Write as _;
#[cfg(rustos_debug_print_enabled)]
use core::hint::spin_loop;
#[cfg(rustos_debug_print_enabled)]
use core::sync::atomic::{AtomicU64, Ordering};
#[cfg(rustos_debug_print_enabled)]
use os_observatory::sink::{RingBufferSink as ObservatoryRingBufferSink, Sink as ObservatorySink};
pub use rustos_observability::{LogCategory, LogLevel};
#[cfg(rustos_debug_print_enabled)]
use spin::{Mutex, RwLock};
#[cfg(rustos_debug_print_enabled)]
include!(concat!(env!("OUT_DIR"), "/logging_build.rs"));

pub use crate::__rustos_debug_debug as debug;
pub use crate::__rustos_debug_enabled as enabled;
pub use crate::__rustos_debug_error as error;
pub use crate::__rustos_debug_error_ratelimited as error_ratelimited;
pub use crate::__rustos_debug_info as info;
pub use crate::__rustos_debug_log as log;
pub use crate::__rustos_debug_trace as trace;
pub use crate::__rustos_debug_warn as warn;
pub use crate::__rustos_debug_warn_ratelimited as warn_ratelimited;

#[cfg(all(rustos_debug_print_enabled, not(test)))]
const DEBUGCON_PORT: u16 = 0x00e9;
#[cfg(rustos_debug_print_enabled)]
const TEXT_RING_CAPACITY: usize = RUSTOS_LOGGING_RING_BUFFER_BYTES;
#[cfg(rustos_debug_print_enabled)]
const SYNTHETIC_WARNING_MODULE_PATH: &str = "nucleus_core::debug";
#[cfg(rustos_debug_print_enabled)]
const MILESTONE_CAPACITY: usize = 128;
#[cfg(rustos_debug_print_enabled)]
const REQUIRED_MILESTONE_OUTPUT_ATTEMPTS: usize = 1 << 20;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CurrentUserLogContext {
    pub process_id: u64,
    pub thread_id: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DebugRuntimeHooks {
    pub ticks: Option<fn() -> u64>,
    pub ticks_per_second: Option<fn() -> u64>,
    pub current_user_context: Option<fn() -> Option<CurrentUserLogContext>>,
}

impl DebugRuntimeHooks {
    pub const fn new() -> Self {
        Self {
            ticks: None,
            ticks_per_second: None,
            current_user_context: None,
        }
    }
}

#[cfg(rustos_debug_print_enabled)]
struct KernelTextRing<const N: usize> {
    inner: ObservatoryRingBufferSink<N>,
    total_written: AtomicU64,
}

#[cfg(rustos_debug_print_enabled)]
#[derive(Clone, Copy)]
struct MilestoneRecord {
    seq: u64,
    tick: u64,
    ts_us: u64,
    category: LogCategory,
    name: &'static str,
    arg0: u64,
    arg1: u64,
}

#[cfg(rustos_debug_print_enabled)]
impl MilestoneRecord {
    const EMPTY: Self = Self {
        seq: 0,
        tick: 0,
        ts_us: 0,
        category: LogCategory::Debug,
        name: "",
        arg0: 0,
        arg1: 0,
    };
}

#[cfg(rustos_debug_print_enabled)]
struct MilestoneRing {
    records: [MilestoneRecord; MILESTONE_CAPACITY],
    next: usize,
    len: usize,
}

#[cfg(rustos_debug_print_enabled)]
impl MilestoneRing {
    const fn new() -> Self {
        Self {
            records: [MilestoneRecord::EMPTY; MILESTONE_CAPACITY],
            next: 0,
            len: 0,
        }
    }

    fn push(&mut self, record: MilestoneRecord) {
        self.records[self.next] = record;
        self.next = (self.next + 1) % MILESTONE_CAPACITY;
        self.len = self.len.saturating_add(1).min(MILESTONE_CAPACITY);
    }

    fn snapshot(&self, out: &mut [MilestoneRecord; MILESTONE_CAPACITY]) -> usize {
        let count = self.len.min(MILESTONE_CAPACITY);
        let start = if self.len == MILESTONE_CAPACITY {
            self.next
        } else {
            0
        };
        for (index, slot) in out.iter_mut().enumerate().take(count) {
            *slot = self.records[(start + index) % MILESTONE_CAPACITY];
        }
        count
    }
}

#[cfg(rustos_debug_print_enabled)]
impl<const N: usize> KernelTextRing<N> {
    const fn new() -> Self {
        Self {
            inner: ObservatoryRingBufferSink::new(),
            total_written: AtomicU64::new(0),
        }
    }

    fn snapshot_bytes(&self) -> Vec<u8> {
        let total_written = self.total_written.load(Ordering::Acquire);
        let used = (total_written as usize).min(N);
        if used == 0 {
            return Vec::new();
        }

        let mut snapshot = vec![0_u8; used];
        let copied = self.inner.snapshot(snapshot.as_mut_slice()).min(used);
        snapshot.truncate(copied);

        if total_written <= N as u64 {
            return snapshot;
        }

        if let Some(newline_index) = snapshot.iter().position(|&byte| byte == b'\n') {
            snapshot.drain(..=newline_index);
        } else {
            snapshot.clear();
        }

        let mut prefixed = synthetic_warning_line();
        prefixed.extend_from_slice(snapshot.as_slice());
        prefixed
    }
}

#[cfg(rustos_debug_print_enabled)]
impl<const N: usize> ObservatorySink for KernelTextRing<N> {
    fn write_str(&self, s: &str) -> usize {
        let written = self.inner.write_str(s);
        self.total_written
            .fetch_add(written as u64, Ordering::Relaxed);
        written
    }
}

#[cfg(rustos_debug_print_enabled)]
static TEXT_RING: KernelTextRing<TEXT_RING_CAPACITY> = KernelTextRing::new();
#[cfg(rustos_debug_print_enabled)]
static KERNEL_LOG_SINK: KernelLogSink = KernelLogSink;
#[cfg(rustos_debug_print_enabled)]
static DEBUG_LOCK: Mutex<()> = Mutex::new(());
#[cfg(rustos_debug_print_enabled)]
static LOG_SEQUENCE: AtomicU64 = AtomicU64::new(1);
#[cfg(rustos_debug_print_enabled)]
static MILESTONE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
#[cfg(rustos_debug_print_enabled)]
static MILESTONES: Mutex<MilestoneRing> = Mutex::new(MilestoneRing::new());
#[cfg(rustos_debug_print_enabled)]
static RATE_LIMIT_FALLBACK_MICROS: AtomicU64 =
    AtomicU64::new(DEFAULT_LOG_RATE_LIMIT_INTERVAL_MICROS);
#[cfg(rustos_debug_print_enabled)]
static RUNTIME_HOOKS: RwLock<DebugRuntimeHooks> = RwLock::new(DebugRuntimeHooks::new());

#[cfg(rustos_debug_print_enabled)]
struct KernelLogSink;

#[cfg(rustos_debug_print_enabled)]
struct DebugconSink;

#[cfg(rustos_debug_print_enabled)]
struct SinkWriter<'a, S: ObservatorySink>(&'a S);

#[cfg(rustos_debug_print_enabled)]
impl<'a, S: ObservatorySink> fmt::Write for SinkWriter<'a, S> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let _ = self.0.write_str(s);
        Ok(())
    }
}

#[cfg(rustos_debug_print_enabled)]
struct DebugconWriter;

#[cfg(rustos_debug_print_enabled)]
impl fmt::Write for DebugconWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if !s.is_empty() {
            print_bytes_unlocked(s.as_bytes());
        }
        Ok(())
    }
}

#[cfg(rustos_debug_print_enabled)]
#[derive(Clone, Copy)]
struct RenderedLogMetadata<'a> {
    seq: u64,
    ts_us: u64,
    tick: u64,
    category: LogCategory,
    level: LogLevel,
    module_path: &'a str,
    line: u32,
    process_id: Option<u64>,
    thread_id: Option<u64>,
}

#[cfg(rustos_debug_print_enabled)]
impl ObservatorySink for KernelLogSink {
    fn write_str(&self, s: &str) -> usize {
        let written = TEXT_RING.write_str(s);
        if RUSTOS_LOGGING_SERIAL_MIRROR && !s.is_empty() {
            print_bytes_unlocked(s.as_bytes());
        }
        written
    }
}

#[cfg(rustos_debug_print_enabled)]
impl ObservatorySink for DebugconSink {
    fn write_str(&self, s: &str) -> usize {
        if !s.is_empty() {
            print_bytes_unlocked(s.as_bytes());
        }
        s.len()
    }
}

#[cfg(all(rustos_debug_print_enabled, test))]
fn print_byte(byte: u8) {
    use std::io::Write as _;

    let _ = std::io::stderr().write_all(&[byte]);
}

#[cfg(rustos_debug_print_enabled)]
fn print_bytes_unlocked(bytes: &[u8]) {
    #[cfg(not(test))]
    {
        if bytes.is_empty() {
            return;
        }
        // KVM treats each `outb` as a VMExit, so a per-byte loop turns a
        // 1KB log line into ~1000 VMExits. `rep outsb` lets the CPU push the
        // entire run with a single instruction; KVM still amortizes the I/O
        // emulation but the host-side overhead drops by 3-5×, removing the
        // visible 700-byte stall that was producing per-second stutter.
        unsafe {
            core::arch::asm!(
                "rep outsb",
                in("dx") DEBUGCON_PORT,
                inout("rsi") bytes.as_ptr() => _,
                inout("rcx") bytes.len() => _,
                options(nostack, preserves_flags),
            );
        }
    }
    #[cfg(test)]
    {
        for &byte in bytes {
            print_byte(byte);
        }
    }
}

#[cfg(rustos_debug_print_enabled)]
struct DebugOutputGuard {
    _guard: spin::MutexGuard<'static, ()>,
    #[cfg(not(test))]
    restore_interrupts: bool,
}

#[cfg(all(rustos_debug_print_enabled, not(test)))]
impl Drop for DebugOutputGuard {
    fn drop(&mut self) {
        if self.restore_interrupts {
            x86_64::instructions::interrupts::enable();
        }
    }
}

#[cfg(rustos_debug_print_enabled)]
fn try_debug_output_lock() -> Option<DebugOutputGuard> {
    #[cfg(not(test))]
    {
        let restore_interrupts = x86_64::instructions::interrupts::are_enabled();
        x86_64::instructions::interrupts::disable();
        match DEBUG_LOCK.try_lock() {
            Some(guard) => Some(DebugOutputGuard {
                _guard: guard,
                restore_interrupts,
            }),
            None => {
                if restore_interrupts {
                    x86_64::instructions::interrupts::enable();
                }
                None
            }
        }
    }

    #[cfg(test)]
    {
        Some(DebugOutputGuard {
            _guard: DEBUG_LOCK.lock(),
        })
    }
}

#[cfg(rustos_debug_print_enabled)]
fn current_runtime_hooks() -> DebugRuntimeHooks {
    *RUNTIME_HOOKS.read()
}

#[cfg(rustos_debug_print_enabled)]
fn ticks_to_micros(ticks: u64, ticks_per_second: u64) -> u64 {
    if ticks_per_second == 0 {
        return 0;
    }
    ticks
        .saturating_mul(1_000_000)
        .saturating_div(ticks_per_second)
}

#[cfg(rustos_debug_print_enabled)]
fn current_tick_and_micros() -> (u64, u64) {
    let hooks = current_runtime_hooks();
    let tick = hooks.ticks.map(|ticks| ticks()).unwrap_or(0);
    let ticks_per_second = hooks
        .ticks_per_second
        .map(|ticks_per_second| ticks_per_second())
        .unwrap_or(0);
    (tick, ticks_to_micros(tick, ticks_per_second))
}

#[cfg(rustos_debug_print_enabled)]
fn current_user_context() -> Option<CurrentUserLogContext> {
    current_runtime_hooks()
        .current_user_context
        .and_then(|snapshot| snapshot())
}

#[cfg(rustos_debug_print_enabled)]
fn render_log_line<W: fmt::Write>(
    writer: &mut W,
    metadata: RenderedLogMetadata<'_>,
    args: fmt::Arguments<'_>,
) -> fmt::Result {
    write!(
        writer,
        "seq={} ts_us={} tick={} lvl={} cat={} mod={} line={} pid=",
        metadata.seq,
        metadata.ts_us,
        metadata.tick,
        metadata.level.as_str(),
        metadata.category.as_str(),
        metadata.module_path,
        metadata.line,
    )?;
    match metadata.process_id {
        Some(process_id) => write!(writer, "{process_id}")?,
        None => writer.write_str("-")?,
    }
    writer.write_str(" tid=")?;
    match metadata.thread_id {
        Some(thread_id) => write!(writer, "{thread_id}")?,
        None => writer.write_str("-")?,
    }
    writer.write_str(" msg=\"")?;
    let mut escaped = EscapedMessageWriter { writer };
    fmt::write(&mut escaped, args)?;
    escaped.writer.write_str("\"\n")
}

#[cfg(rustos_debug_print_enabled)]
struct EscapedMessageWriter<'a, W: fmt::Write> {
    writer: &'a mut W,
}

#[cfg(rustos_debug_print_enabled)]
impl<'a, W: fmt::Write> fmt::Write for EscapedMessageWriter<'a, W> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for ch in s.chars() {
            match ch {
                '\\' => self.writer.write_str("\\\\")?,
                '"' => self.writer.write_str("\\\"")?,
                '\n' => self.writer.write_str("\\n")?,
                '\r' => self.writer.write_str("\\r")?,
                '\t' => self.writer.write_str("\\t")?,
                _ => self.writer.write_char(ch)?,
            }
        }
        Ok(())
    }
}

#[cfg(rustos_debug_print_enabled)]
fn synthetic_warning_line() -> Vec<u8> {
    let mut line = String::new();
    let _ = render_log_line(
        &mut line,
        RenderedLogMetadata {
            seq: 0,
            ts_us: 0,
            tick: 0,
            category: LogCategory::Debug,
            level: LogLevel::Warn,
            module_path: SYNTHETIC_WARNING_MODULE_PATH,
            line: 0,
            process_id: None,
            thread_id: None,
        },
        format_args!("oldest logs dropped"),
    );
    line.into_bytes()
}

#[cfg(rustos_debug_print_enabled)]
fn rate_limit_clock_micros() -> u64 {
    let (_, ts_us) = current_tick_and_micros();
    if ts_us != 0 {
        return ts_us;
    }
    RATE_LIMIT_FALLBACK_MICROS.fetch_add(DEFAULT_LOG_RATE_LIMIT_INTERVAL_MICROS, Ordering::Relaxed)
}

pub const DEFAULT_LOG_RATE_LIMIT_INTERVAL_MICROS: u64 = 1_000_000;

pub fn init(_boot_info_ptr: *const BootInfo) {}

#[cfg(rustos_debug_print_enabled)]
pub fn register_runtime_hooks(hooks: DebugRuntimeHooks) {
    *RUNTIME_HOOKS.write() = hooks;
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn register_runtime_hooks(_hooks: DebugRuntimeHooks) {}

#[cfg(rustos_debug_print_enabled)]
pub fn should_emit(category: LogCategory, level: LogLevel) -> bool {
    compiled_level_enabled(category, level)
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn should_emit(_category: LogCategory, _level: LogLevel) -> bool {
    false
}

#[cfg(rustos_debug_print_enabled)]
pub fn log_args_site(
    category: LogCategory,
    level: LogLevel,
    module_path: &'static str,
    line: u32,
    args: fmt::Arguments<'_>,
) {
    if !compiled_level_enabled(category, level) {
        return;
    }

    let (tick, ts_us) = current_tick_and_micros();
    let user_context = current_user_context();
    let metadata = RenderedLogMetadata {
        seq: LOG_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ts_us,
        tick,
        category,
        level,
        module_path,
        line,
        process_id: user_context.map(|context| context.process_id),
        thread_id: user_context.map(|context| context.thread_id),
    };

    if let Some(_guard) = try_debug_output_lock() {
        let mut writer = SinkWriter(&KERNEL_LOG_SINK);
        let _ = render_log_line(&mut writer, metadata, args);
    }
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn log_args_site(
    _category: LogCategory,
    _level: LogLevel,
    _module_path: &'static str,
    _line: u32,
    _args: fmt::Arguments<'_>,
) {
}

#[cfg(rustos_debug_print_enabled)]
pub fn log_args(category: LogCategory, level: LogLevel, args: fmt::Arguments<'_>) {
    log_args_site(category, level, SYNTHETIC_WARNING_MODULE_PATH, 0, args);
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn log_args(_category: LogCategory, _level: LogLevel, _args: fmt::Arguments<'_>) {}

#[cfg(rustos_debug_print_enabled)]
pub fn record_milestone(category: LogCategory, name: &'static str, arg0: u64, arg1: u64) {
    let (tick, ts_us) = current_tick_and_micros();
    let record = MilestoneRecord {
        seq: MILESTONE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        tick,
        ts_us,
        category,
        name,
        arg0,
        arg1,
    };
    if let Some(mut milestones) = MILESTONES.try_lock() {
        milestones.push(record);
    }
    emit_milestone_debugcon_line(record);
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn record_milestone(_category: LogCategory, _name: &'static str, _arg0: u64, _arg1: u64) {}

#[cfg(rustos_debug_print_enabled)]
fn emit_milestone_debugcon_line(record: MilestoneRecord) {
    if !compiled_level_enabled(record.category, LogLevel::Info) {
        return;
    }
    if !milestone_debugcon_visible(record.name) {
        return;
    }
    let user_context = current_user_context();
    // Commercial acceptance gates must not confuse a busy debug sink with a
    // missing CPU or product transition. These milestones are one-shot and
    // retry the nonblocking output lock for a fixed bound; ordinary diagnostic
    // traffic remains best-effort.
    let attempts = if milestone_requires_reliable_output(record.name) {
        REQUIRED_MILESTONE_OUTPUT_ATTEMPTS
    } else {
        1
    };
    for _ in 0..attempts {
        if let Some(_guard) = try_debug_output_lock() {
            let mut writer = DebugconWriter;
            let log_seq = LOG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let _ = write!(
                writer,
                "seq={} ts_us={} tick={} lvl=info cat={} mod={} line=0 pid=",
                log_seq,
                record.ts_us,
                record.tick,
                record.category.as_str(),
                SYNTHETIC_WARNING_MODULE_PATH,
            );
            match user_context.map(|context| context.process_id) {
                Some(process_id) => {
                    let _ = write!(writer, "{process_id}");
                }
                None => {
                    let _ = writer.write_str("-");
                }
            }
            let _ = writer.write_str(" tid=");
            match user_context.map(|context| context.thread_id) {
                Some(thread_id) => {
                    let _ = write!(writer, "{thread_id}");
                }
                None => {
                    let _ = writer.write_str("-");
                }
            }
            let _ = write!(
                writer,
                " msg=\"milestone seq={} cat={} name={} arg0={:#x} arg1={:#x} dropped={}\"\r\n",
                record.seq,
                record.category.as_str(),
                record.name,
                record.arg0,
                record.arg1,
                // ORDERING: Relaxed is sufficient; this is a monotonic loss
                // count, not a publication of any other state.
                MILESTONES_DROPPED.load(Ordering::Relaxed),
            );
            return;
        }
        spin_loop();
    }
    // Every attempt lost the nonblocking sink. Count it: a diagnostic that can
    // vanish without saying so is worse than no diagnostic, because the reader
    // draws conclusions from a record they believe is complete. One 8-vCPU run
    // lost 90 of 351 milestone sequence numbers, and the losses fell at the
    // tail of the per-second scheduler drain, which is exactly where the
    // per-caller acquisition census is emitted.
    // ORDERING: Relaxed; the count is read only for reporting.
    MILESTONES_DROPPED.fetch_add(1, Ordering::Relaxed);
}

/// Milestones whose emission lost the debug sink and were never written.
///
/// Reported on every line that does get out, so a reader can tell a complete
/// record from a truncated one without reconstructing sequence gaps by hand.
#[cfg(rustos_debug_print_enabled)]
static MILESTONES_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Number of milestones lost to a busy debug sink so far.
#[cfg(rustos_debug_print_enabled)]
pub fn milestones_dropped() -> u64 {
    // ORDERING: Relaxed; a monotonic counter with no other state attached.
    MILESTONES_DROPPED.load(Ordering::Relaxed)
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn milestones_dropped() -> u64 {
    0
}

#[cfg(rustos_debug_print_enabled)]
fn milestone_requires_reliable_output(name: &str) -> bool {
    name.starts_with("smp-")
        || name.starts_with("product-")
        // The once-per-second scheduler record is measurement, and a
        // measurement that silently loses its tail is not evidence. These are
        // emitted in one burst of about fourteen lines, so the later ones —
        // the per-caller acquisition census — are the ones that lost the race
        // under 8 vCPU contention.
        || name.starts_with("kernel-scheduler-")
        // Emitted immediately before a fatal activation panic. A diagnostic
        // that explains the panic about to happen is the last thing that may
        // be dropped: the first 8-vCPU run with these records lost all four to
        // a saturated sink and left the panic as unattributable as before.
        || name.starts_with("sched-activation-")
        || name == "dvm-block-first-completion"
        || name == "task-context-corrupted"
        || name == "linux-user-fault"
        || name == "linux-thread-clone-rejected"
        // A degraded donation is the record that a scheduling edge was dropped
        // without failing the call. Losing it turns the degradation invisible,
        // which is how the fail-closed version of this path stayed hidden until
        // it killed the compositor.
        || name.starts_with("ipc-donation-")
        // Input-ring lifecycle transitions. The L0 relay fails the whole proof
        // when readiness disappears under it, and until these existed the only
        // account of the transition lived in `debug::warn!`, which the product
        // configuration does not route anywhere.
        || name.starts_with("dvm-input-")
}

#[cfg(rustos_debug_print_enabled)]
fn milestone_debugcon_visible(name: &str) -> bool {
    !matches!(
        name,
        "boot"
            | "ipc-reply-timeout"
            | "driver-loader"
            | "module-probe-entry"
            | "module-probe-virtio-net"
            | "module-reloc-target"
            | "module-reloc-resolved"
            | "module-reloc-head"
            | "module-init-linux-call"
            | "module-init-return"
            | "module-init-disallowed-external"
            | "module-init-unresolved-external"
            | "linux-virtio-register"
            | "linux-virtio-register-return"
    )
}

#[cfg(rustos_debug_print_enabled)]
pub fn dump_recent_milestones(reason: &str) {
    let mut records = [MilestoneRecord::EMPTY; MILESTONE_CAPACITY];
    let count = MILESTONES
        .try_lock()
        .map(|milestones| milestones.snapshot(&mut records))
        .unwrap_or(0);

    log_args_site(
        LogCategory::Debug,
        LogLevel::Warn,
        module_path!(),
        line!(),
        format_args!("milestone dump requested: {} count={}", reason, count),
    );

    for record in records.iter().take(count) {
        log_args_site(
            record.category,
            LogLevel::Warn,
            module_path!(),
            line!(),
            format_args!(
                "milestone seq={} ts_us={} tick={} cat={} name={} arg0={:#x} arg1={:#x}",
                record.seq,
                record.ts_us,
                record.tick,
                record.category.as_str(),
                record.name,
                record.arg0,
                record.arg1,
            ),
        );
    }
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn dump_recent_milestones(_reason: &str) {}

#[cfg(rustos_debug_print_enabled)]
pub fn rate_limit_permit(
    last_emit_micros: &core::sync::atomic::AtomicU64,
    interval_micros: u64,
) -> bool {
    let now = rate_limit_clock_micros();
    let last = last_emit_micros.load(Ordering::Relaxed);
    if now.saturating_sub(last) < interval_micros {
        return false;
    }
    last_emit_micros.store(now, Ordering::Relaxed);
    true
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn rate_limit_permit(
    _last_emit_micros: &core::sync::atomic::AtomicU64,
    _interval_micros: u64,
) -> bool {
    false
}

#[cfg(rustos_debug_print_enabled)]
pub fn report_panic(info: &core::panic::PanicInfo<'_>) {
    if should_emit(LogCategory::Panic, LogLevel::Fatal) {
        if let Some(location) = info.location() {
            log_args_site(
                LogCategory::Panic,
                LogLevel::Fatal,
                module_path!(),
                line!(),
                format_args!(
                    "panic location={}:{}:{} message={}",
                    location.file(),
                    location.line(),
                    location.column(),
                    info.message()
                ),
            );
        } else {
            log_args_site(
                LogCategory::Panic,
                LogLevel::Fatal,
                module_path!(),
                line!(),
                format_args!("panic location=<unknown> message={}", info.message()),
            );
        }
    }

    if let Some(_guard) = try_debug_output_lock() {
        os_observatory::panic::report_panic(&DebugconSink, info);
    }
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn report_panic(_info: &core::panic::PanicInfo<'_>) {}

#[cfg(rustos_debug_print_enabled)]
pub fn println_newline() {
    write_debugcon_only_line(b"");
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn println_newline() {}

#[cfg(rustos_debug_print_enabled)]
pub fn println_fmt(args: fmt::Arguments<'_>) {
    if let Some(_guard) = try_debug_output_lock() {
        let mut writer = DebugconWriter;
        let _ = writer.write_fmt(args);
        let _ = writer.write_str("\r\n");
    }
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn println_fmt(_args: fmt::Arguments<'_>) {}

#[cfg(rustos_debug_print_enabled)]
pub fn println_emergency(args: fmt::Arguments<'_>) {
    #[cfg(not(test))]
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut writer = DebugconWriter;
        let _ = writer.write_fmt(args);
        let _ = writer.write_str("\r\n");
    });

    #[cfg(test)]
    {
        let mut writer = DebugconWriter;
        let _ = writer.write_fmt(args);
        let _ = writer.write_str("\r\n");
    }
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn println_emergency(_args: fmt::Arguments<'_>) {}

#[cfg(rustos_debug_print_enabled)]
pub fn record_trace_location(file: &'static str, line: u32, column: u32) {
    record_trace_location_with_note(file, line, column, None);
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn record_trace_location(_file: &'static str, _line: u32, _column: u32) {}

#[cfg(rustos_debug_print_enabled)]
pub fn record_trace_location_with_note(
    file: &'static str,
    line: u32,
    column: u32,
    note: Option<&'static str>,
) {
    match note {
        Some(note) => log_args_site(
            LogCategory::Debug,
            LogLevel::Debug,
            module_path!(),
            line!(),
            format_args!("breadcrumb {}:{}:{} {}", file, line, column, note),
        ),
        None => log_args_site(
            LogCategory::Debug,
            LogLevel::Debug,
            module_path!(),
            line!(),
            format_args!("breadcrumb {}:{}:{}", file, line, column),
        ),
    }
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn record_trace_location_with_note(
    _file: &'static str,
    _line: u32,
    _column: u32,
    _note: Option<&'static str>,
) {
}

#[cfg(rustos_debug_print_enabled)]
pub fn dump_recent_trace_locations(reason: &str) {
    log_args_site(
        LogCategory::Debug,
        LogLevel::Warn,
        module_path!(),
        line!(),
        format_args!("breadcrumb dump requested: {}", reason),
    );
    dump_recent_milestones(reason);
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn dump_recent_trace_locations(_reason: &str) {}

#[cfg(rustos_debug_print_enabled)]
pub fn write_bytes(bytes: &[u8]) {
    write_debugcon_only(bytes);
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn write_bytes(_bytes: &[u8]) {}

#[cfg(rustos_debug_print_enabled)]
pub fn write_debugcon_only(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if let Some(_guard) = try_debug_output_lock() {
        print_bytes_unlocked(bytes);
    }
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn write_debugcon_only(_bytes: &[u8]) {}

#[cfg(rustos_debug_print_enabled)]
pub fn write_debugcon_only_line(bytes: &[u8]) {
    if let Some(_guard) = try_debug_output_lock() {
        if !bytes.is_empty() {
            print_bytes_unlocked(bytes);
        }
        print_bytes_unlocked(b"\r\n");
    }
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn write_debugcon_only_line(_bytes: &[u8]) {}

#[cfg(rustos_debug_print_enabled)]
pub fn write_debugcon_only_parts_line(parts: &[&[u8]]) {
    if let Some(_guard) = try_debug_output_lock() {
        for part in parts {
            if !part.is_empty() {
                print_bytes_unlocked(part);
            }
        }
        print_bytes_unlocked(b"\r\n");
    }
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn write_debugcon_only_parts_line(_parts: &[&[u8]]) {}

#[cfg(rustos_debug_print_enabled)]
pub fn snapshot_structured_log_bytes() -> Vec<u8> {
    TEXT_RING.snapshot_bytes()
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn snapshot_structured_log_bytes() -> Vec<u8> {
    Vec::new()
}

#[macro_export]
macro_rules! diag_trace {
    ($category:expr, $($arg:tt)+) => {{
        $crate::debug::log_args_site(
            $category,
            $crate::debug::LogLevel::Trace,
            module_path!(),
            line!(),
            format_args!($($arg)+),
        );
    }};
}

#[macro_export]
macro_rules! diag_debug {
    ($category:expr, $($arg:tt)+) => {{
        $crate::debug::log_args_site(
            $category,
            $crate::debug::LogLevel::Debug,
            module_path!(),
            line!(),
            format_args!($($arg)+),
        );
    }};
}

#[macro_export]
macro_rules! diag_info {
    ($category:expr, $($arg:tt)+) => {{
        $crate::debug::log_args_site(
            $category,
            $crate::debug::LogLevel::Info,
            module_path!(),
            line!(),
            format_args!($($arg)+),
        );
    }};
}

#[macro_export]
macro_rules! diag_warn {
    ($category:expr, $($arg:tt)+) => {{
        $crate::debug::log_args_site(
            $category,
            $crate::debug::LogLevel::Warn,
            module_path!(),
            line!(),
            format_args!($($arg)+),
        );
    }};
}

#[macro_export]
macro_rules! diag_error {
    ($category:expr, $($arg:tt)+) => {{
        $crate::debug::log_args_site(
            $category,
            $crate::debug::LogLevel::Error,
            module_path!(),
            line!(),
            format_args!($($arg)+),
        );
    }};
}

#[cfg(all(test, rustos_debug_print_enabled))]
mod tests {
    use alloc::string::String;

    use super::*;

    #[test]
    fn render_log_line_uses_fixed_field_order() {
        let mut line = String::new();
        let _ = render_log_line(
            &mut line,
            RenderedLogMetadata {
                seq: 7,
                ts_us: 19,
                tick: 23,
                category: LogCategory::Usb,
                level: LogLevel::Warn,
                module_path: "kernel::usb::core",
                line: 41,
                process_id: Some(100),
                thread_id: Some(200),
            },
            format_args!("controller ready"),
        );

        assert_eq!(
            line,
            "seq=7 ts_us=19 tick=23 lvl=warn cat=usb mod=kernel::usb::core line=41 pid=100 tid=200 msg=\"controller ready\"\n"
        );
    }

    #[test]
    fn render_log_line_escapes_message_text() {
        let mut line = String::new();
        let _ = render_log_line(
            &mut line,
            RenderedLogMetadata {
                seq: 1,
                ts_us: 2,
                tick: 3,
                category: LogCategory::Debug,
                level: LogLevel::Info,
                module_path: "kernel::debug",
                line: 9,
                process_id: None,
                thread_id: None,
            },
            format_args!("quote=\" path=\\ newline=\n tab=\t"),
        );

        assert_eq!(
            line,
            "seq=1 ts_us=2 tick=3 lvl=info cat=debug mod=kernel::debug line=9 pid=- tid=- msg=\"quote=\\\" path=\\\\ newline=\\n tab=\\t\"\n"
        );
    }

    #[test]
    fn wrapped_snapshot_prepends_drop_warning() {
        let ring = KernelTextRing::<24>::new();
        let _ = ObservatorySink::write_str(&ring, "alpha line is long enough\n");
        let _ = ObservatorySink::write_str(&ring, "beta line is also long\n");
        let _ = ObservatorySink::write_str(&ring, "gamma\n");

        let snapshot = String::from_utf8(ring.snapshot_bytes()).unwrap();
        assert!(snapshot.starts_with(
            "seq=0 ts_us=0 tick=0 lvl=warn cat=debug mod=nucleus_core::debug line=0 pid=- tid=- msg=\"oldest logs dropped\"\n"
        ));
        assert!(snapshot.contains("gamma"));
    }

    #[test]
    fn high_frequency_ipc_timeout_milestones_stay_off_debugcon() {
        // Timeout evidence remains in the bounded milestone ring and is
        // included in explicit postmortem dumps. Emitting one formatted
        // debugcon line per readiness timeout would turn each byte into a KVM
        // port-I/O exit and make the diagnostic path amplify the overload.
        assert!(!milestone_debugcon_visible("ipc-reply-timeout"));
        assert!(milestone_debugcon_visible("proc-commit-address-space-done"));
    }

    #[test]
    fn acceptance_milestones_retry_the_contended_debug_sink() {
        assert!(milestone_requires_reliable_output("smp-cpu-online"));
        assert!(milestone_requires_reliable_output("product-storage-ready"));
        assert!(milestone_requires_reliable_output(
            "dvm-block-first-completion"
        ));
        assert!(milestone_requires_reliable_output("task-context-corrupted"));
        assert!(milestone_requires_reliable_output("linux-user-fault"));
        assert!(milestone_requires_reliable_output(
            "linux-thread-clone-rejected"
        ));
        assert!(!milestone_requires_reliable_output("ipc-reply-rejected"));
    }
}
