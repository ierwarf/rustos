pub mod boot_trace;
#[cfg(rustos_debug_print_enabled)]
mod deferred;
mod kdiag_macros;
#[cfg(rustos_debug_print_enabled)]
mod milestone_class;
#[cfg(rustos_debug_print_enabled)]
mod milestone_frame;
pub mod phase_profile;
#[cfg(rustos_debug_print_enabled)]
use milestone_class::{MilestoneOutputClass, milestone_loss_snapshot, milestone_output_class};

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
// Every regular diagnostic is rendered before it takes DEBUG_LOCK, so one
// owned buffer can be sent with one debugcon write after the bounded acquire.
// The existing ordinary call sites below admit substantially less than this;
// an oversize line is discarded whole rather than becoming misleading partial
// evidence.
#[cfg(rustos_debug_print_enabled)]
const SERIALIZED_DEBUGCON_LINE_CAPACITY: usize = 512;
#[cfg(rustos_debug_print_enabled)]
const USER_DEBUG_ESCAPED_PAYLOAD_BYTES: usize = 480;
// Milestones duplicate their semantic record in the self-framing evidence
// payload and therefore need more headroom than an ordinary diagnostic. A
// render that exceeds this fixed allocation-free bound panics before emitting
// any bytes; it must never fabricate a truncated acceptance marker.
#[cfg(rustos_debug_print_enabled)]
const MILESTONE_DEBUGCON_LINE_CAPACITY: usize = 1024;
#[cfg(rustos_debug_print_enabled)]
const FNV1A64_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
#[cfg(rustos_debug_print_enabled)]
const FNV1A64_PRIME: u64 = 0x0000_0100_0000_01b3;

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

/// Fixed, stack-backed line storage for a debugcon record.
///
/// The writer rejects the entire next fragment when it cannot fit. Callers
/// only publish after formatting succeeds, so an overflow can never expose a
/// prefix that looks like complete evidence.
#[cfg(rustos_debug_print_enabled)]
struct FixedDebugconLine<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

#[cfg(rustos_debug_print_enabled)]
impl<const N: usize> FixedDebugconLine<N> {
    const fn new() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    fn len(&self) -> usize {
        self.len
    }
}

#[cfg(rustos_debug_print_enabled)]
impl<const N: usize> fmt::Write for FixedDebugconLine<N> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let end = self.len.checked_add(value.len()).ok_or(fmt::Error)?;
        if end > N {
            return Err(fmt::Error);
        }
        self.bytes[self.len..end].copy_from_slice(value.as_bytes());
        self.len = end;
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
    // `cfg(test)` is not the question "does this build own the hardware".
    // It is true only while compiling *this* crate under test, so every
    // dependent's test binary links this crate with `cfg(test)` false - and a
    // host process then executes `rep outsb` and dies on SIGSEGV. That is why
    // no `kernel-ps` scheduler test could touch a path recording a milestone,
    // which is most of the donation and handoff logic. `rustos_boot_image` is
    // the fact that actually decides it.
    #[cfg(all(rustos_boot_image, not(test)))]
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
    // This crate's own tests keep a visible sink. A dependent's test binary
    // gets neither: it is `no_std` here, so there is no stderr to reach, and a
    // discarded diagnostic is the correct outcome for a host process that was
    // never meant to drive a debug port.
    #[cfg(test)]
    {
        for &byte in bytes {
            print_byte(byte);
        }
    }

    #[cfg(all(not(rustos_boot_image), not(test)))]
    {
        let _ = bytes;
    }
}

#[cfg(rustos_debug_print_enabled)]
struct DebugOutputGuard {
    _guard: spin::MutexGuard<'static, ()>,
    #[cfg(all(rustos_boot_image, not(test)))]
    restore_interrupts: bool,
}

#[cfg(all(rustos_debug_print_enabled, rustos_boot_image, not(test)))]
impl Drop for DebugOutputGuard {
    fn drop(&mut self) {
        if self.restore_interrupts {
            x86_64::instructions::interrupts::enable();
        }
    }
}

#[cfg(rustos_debug_print_enabled)]
fn try_debug_output_lock() -> Option<DebugOutputGuard> {
    #[cfg(all(rustos_boot_image, not(test)))]
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

    #[cfg(any(not(rustos_boot_image), test))]
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
fn render_serialized_debugcon_line<const N: usize>(
    line: &mut FixedDebugconLine<N>,
    args: fmt::Arguments<'_>,
) -> fmt::Result {
    line.write_fmt(args)?;
    line.write_str("\r\n")
}

#[cfg(rustos_debug_print_enabled)]
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV1A64_OFFSET_BASIS, |checksum, byte| {
        (checksum ^ u64::from(*byte)).wrapping_mul(FNV1A64_PRIME)
    })
}

/// Pure frame verifier used by the unit tests. It accepts only a complete v1
/// milestone frame with an exact suffix and a checksum matching every semantic
/// payload byte.
#[cfg(all(rustos_debug_print_enabled, test))]
fn verify_milestone_debugcon_line(line: &[u8]) -> bool {
    let Some((semantic_start, checksum_offset, expected_checksum)) =
        milestone_frame::parse_milestone_debugcon_checksum(line)
    else {
        return false;
    };
    fnv1a64(&line[semantic_start..checksum_offset]) == expected_checksum
}

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
    let output_class = milestone_output_class(record.name);
    // Measurements are one-shot; Required and QualificationCritical keep the
    // bounded retry.
    for _ in 0..output_class.output_attempts() {
        if let Some(_guard) = try_debug_output_lock() {
            drain_deferred_records();
            // Rendering allocates the output sequence, so it must run under
            // DEBUG_LOCK: rendering first lets a CPU that loses the acquisition
            // race publish a lower sequence after a higher one is already on the
            // wire, which the host validator reads as replayed evidence. `line` is
            // then complete before the first port I/O, so one `rep outsb` publishes
            // the whole frame without a diagnostic splicing bytes into it.
            let line = render_milestone_debugcon_line(record, user_context, output_class);
            print_bytes_unlocked(line.bytes());
            return;
        }
        spin_loop();
    }
    // The sink stayed held. A class whose loss the harness reads as a failure
    // parks its record - not its rendered bytes, so the drainer still
    // allocates the output sequence under the sink.
    if output_class.must_reach_sink()
        && deferred::park_milestone(deferred::ParkedMilestone {
            record,
            user_context,
            output_class,
        })
    {
        return;
    }
    // Critical evidence accounts only for its own rendered loss; the unwritten
    // frame leaves a sequence gap, never a duplicate.
    let discarded_bytes = if output_class == MilestoneOutputClass::QualificationCritical {
        render_milestone_debugcon_line(record, user_context, output_class).len() as u64
    } else {
        0
    };
    record_milestone_output_drop(output_class, discarded_bytes);
}

/// Emit every parked record. The caller must hold the output sink.
#[cfg(rustos_debug_print_enabled)]
fn drain_deferred_records() {
    deferred::drain(print_bytes_unlocked, |parked| {
        let line =
            render_milestone_debugcon_line(parked.record, parked.user_context, parked.output_class);
        print_bytes_unlocked(line.bytes());
    });
}

#[cfg(rustos_debug_print_enabled)]
fn render_milestone_debugcon_line(
    record: MilestoneRecord,
    user_context: Option<CurrentUserLogContext>,
    output_class: MilestoneOutputClass,
) -> FixedDebugconLine<MILESTONE_DEBUGCON_LINE_CAPACITY> {
    let (dropped, discarded_bytes) = milestone_loss_snapshot(
        output_class,
        MILESTONES_DROPPED.load(Ordering::Relaxed),
        DEBUG_BYTES_DISCARDED.load(Ordering::Relaxed),
        QUALIFICATION_MILESTONES_DROPPED.load(Ordering::Relaxed),
        QUALIFICATION_DEBUG_BYTES_DISCARDED.load(Ordering::Relaxed),
    );
    let mut line = FixedDebugconLine::new();
    milestone_frame::render_milestone_debugcon_line(
        &mut line,
        LOG_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        record,
        user_context,
        dropped,
        discarded_bytes,
    )
    .expect("milestone debugcon line exceeds fixed evidence buffer");
    line
}

#[cfg(rustos_debug_print_enabled)]
fn record_milestone_output_drop(output_class: MilestoneOutputClass, discarded_bytes: u64) {
    record_milestone_output_drop_to(
        output_class,
        discarded_bytes,
        &MILESTONES_DROPPED,
        &QUALIFICATION_MILESTONES_DROPPED,
        &QUALIFICATION_DEBUG_BYTES_DISCARDED,
    );
}
#[cfg(rustos_debug_print_enabled)]
fn record_milestone_output_drop_to(
    output_class: MilestoneOutputClass,
    discarded_bytes: u64,
    milestones_dropped: &AtomicU64,
    qualification_milestones_dropped: &AtomicU64,
    qualification_discarded_bytes: &AtomicU64,
) {
    milestones_dropped.fetch_add(1, Ordering::Relaxed);
    if output_class == MilestoneOutputClass::QualificationCritical {
        qualification_milestones_dropped.fetch_add(1, Ordering::Relaxed);
        qualification_discarded_bytes.fetch_add(discarded_bytes, Ordering::Relaxed);
    }
}
/// Milestones whose emission lost the debug sink and were never written.
#[cfg(rustos_debug_print_enabled)]
static MILESTONES_DROPPED: AtomicU64 = AtomicU64::new(0);
#[cfg(rustos_debug_print_enabled)]
static QUALIFICATION_MILESTONES_DROPPED: AtomicU64 = AtomicU64::new(0);
#[cfg(rustos_debug_print_enabled)]
static QUALIFICATION_DEBUG_BYTES_DISCARDED: AtomicU64 = AtomicU64::new(0);

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
    println_serialized(args);
}

/// Emits one non-panic diagnostic as one complete, bounded-retry debugcon
/// line. It never publishes a partial formatted line: rendering completes in
/// fixed storage before the port write, then DEBUG_LOCK remains held through
/// exactly one `print_bytes_unlocked` call.
#[cfg(rustos_debug_print_enabled)]
pub fn println_serialized(args: fmt::Arguments<'_>) {
    let mut line = FixedDebugconLine::<SERIALIZED_DEBUGCON_LINE_CAPACITY>::new();
    if render_serialized_debugcon_line(&mut line, args).is_err() {
        // The whole line is intentionally omitted. Reporting a prefix would
        // turn an unavailable diagnostic into false structured evidence.
        DEBUG_BYTES_DISCARDED.fetch_add(line.len() as u64, Ordering::Relaxed);
        return;
    }
    for _ in 0..DEBUG_OUTPUT_ACQUIRE_ATTEMPTS {
        if let Some(_guard) = try_debug_output_lock() {
            drain_deferred_records();
            print_bytes_unlocked(line.bytes());
            return;
        }
        spin_loop();
    }
    // Park for whichever CPU takes the sink next, rather than discarding.
    if !deferred::park(line.bytes()) {
        DEBUG_BYTES_DISCARDED.fetch_add(line.len() as u64, Ordering::Relaxed);
    }
}

#[cfg(rustos_debug_print_enabled)]
struct EscapedUserDebugPayload<'a>(&'a [u8]);

#[cfg(rustos_debug_print_enabled)]
const fn escaped_user_debug_byte_width(byte: u8) -> usize {
    match byte {
        b'\n' | b'\r' | b'\t' | b'"' | b'\\' => 2,
        0x20..=0x7e => 1,
        _ => 4,
    }
}

#[cfg(rustos_debug_print_enabled)]
fn bounded_user_debug_payload_prefix(payload: &[u8]) -> usize {
    let mut encoded = 0;
    let mut count = 0;
    for &byte in payload {
        let width = escaped_user_debug_byte_width(byte);
        if encoded + width > USER_DEBUG_ESCAPED_PAYLOAD_BYTES {
            break;
        }
        encoded += width;
        count += 1;
    }
    count
}

#[cfg(rustos_debug_print_enabled)]
impl fmt::Display for EscapedUserDebugPayload<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &byte in self.0 {
            match byte {
                b'\n' => formatter.write_str("\\n")?,
                b'\r' => formatter.write_str("\\r")?,
                b'\t' => formatter.write_str("\\t")?,
                b'"' => formatter.write_str("\\\"")?,
                b'\\' => formatter.write_str("\\\\")?,
                0x20..=0x7e => formatter.write_char(char::from(byte))?,
                _ => write!(formatter, "\\x{byte:02x}")?,
            }
        }
        Ok(())
    }
}

/// Emits bytes copied from Ring3 without granting them a raw debugcon line.
///
/// The fixed prefix plus control/quote escaping makes it impossible for a
/// userspace console/debug write to create a line that the milestone parser
/// can confuse with a kernel-stamped evidence frame. Printable diagnostic
/// substrings remain visible to existing bounded marker searches.
#[cfg(rustos_debug_print_enabled)]
pub fn write_user_bytes_serialized(bytes: &[u8]) {
    let mut written = 0;
    while written < bytes.len() {
        let payload_len = bounded_user_debug_payload_prefix(&bytes[written..]);
        debug_assert!(payload_len != 0);
        println_serialized(format_args!(
            "user-debug payload={}",
            EscapedUserDebugPayload(&bytes[written..written + payload_len])
        ));
        written += payload_len;
    }
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn write_user_bytes_serialized(_bytes: &[u8]) {}

#[cfg(not(rustos_debug_print_enabled))]
pub fn println_fmt(_args: fmt::Arguments<'_>) {}

#[cfg(not(rustos_debug_print_enabled))]
pub fn println_serialized(_args: fmt::Arguments<'_>) {}

/// Lock-free panic/emergency output that deliberately bypasses DEBUG_LOCK.
///
/// Only panic handling or a re-entrant emergency condition may use this API.
/// Ordinary diagnostics must use [`println_serialized`] so their complete line
/// cannot interleave with a milestone or another normal diagnostic.
#[cfg(rustos_debug_print_enabled)]
pub fn println_emergency(args: fmt::Arguments<'_>) {
    #[cfg(all(rustos_boot_image, not(test)))]
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut writer = DebugconWriter;
        let _ = writer.write_fmt(args);
        let _ = writer.write_str("\r\n");
    });

    #[cfg(any(not(rustos_boot_image), test))]
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

/// Bounded retries before a diagnostic hands its line to [`deferred`].
///
/// The sink is held with interrupts disabled, so a failed try always means a
/// different, running CPU holds it. Retrying is therefore safe and one attempt
/// is not enough: at 8 vCPU the kernel, uiserver, inputd, and the client all
/// print. Retries bound the loss but cannot remove it on an unfair lock, which
/// is why exhausting them parks the line instead of discarding it.
#[cfg(rustos_debug_print_enabled)]
const DEBUG_OUTPUT_ACQUIRE_ATTEMPTS: usize = 4096;

#[cfg(rustos_debug_print_enabled)]
static DEBUG_BYTES_DISCARDED: AtomicU64 = AtomicU64::new(0);

/// Bytes the debug transport discarded because the output lock stayed held.
#[cfg(rustos_debug_print_enabled)]
pub fn discarded_debug_bytes() -> u64 {
    DEBUG_BYTES_DISCARDED.load(Ordering::Relaxed)
}

#[cfg(not(rustos_debug_print_enabled))]
pub fn discarded_debug_bytes() -> u64 {
    0
}

#[cfg(rustos_debug_print_enabled)]
pub fn write_debugcon_only(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    for _ in 0..DEBUG_OUTPUT_ACQUIRE_ATTEMPTS {
        if let Some(_guard) = try_debug_output_lock() {
            drain_deferred_records();
            print_bytes_unlocked(bytes);
            return;
        }
        core::hint::spin_loop();
    }
    if deferred::park(bytes) {
        return;
    }
    // ORDERING: Relaxed. A diagnostic counter with no other state attached,
    // read only by the once-per-second milestone burst.
    DEBUG_BYTES_DISCARDED.fetch_add(bytes.len() as u64, Ordering::Relaxed);
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
#[path = "tests.rs"]
mod tests;
