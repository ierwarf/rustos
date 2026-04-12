use alloc::vec::Vec;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use spin::Mutex;
use x86_64::instructions::{hlt, interrupts, port::Port};

const CMOS_INDEX_PORT: u16 = 0x70;
const CMOS_DATA_PORT: u16 = 0x71;
const NMI_DISABLE: u8 = 0x80;
const CMOS_REGISTER_MASK: u8 = 0x7f;

const RTC_REG_A: u8 = 0x0A;
const RTC_REG_B: u8 = 0x0B;
const RTC_REG_C: u8 = 0x0C;
const RTC_REG_SECONDS: u8 = 0x00;
const RTC_REG_MINUTES: u8 = 0x02;
const RTC_REG_HOURS: u8 = 0x04;
const RTC_REG_WEEKDAY: u8 = 0x06;
const RTC_REG_DAY: u8 = 0x07;
const RTC_REG_MONTH: u8 = 0x08;
const RTC_REG_YEAR: u8 = 0x09;
const RTC_UPDATE_IN_PROGRESS: u8 = 1 << 7;
const RTC_PERIODIC_INTERRUPT_ENABLE: u8 = 1 << 6;
const RTC_RATE_1024_HZ: u8 = 6;
const RTC_TICKS_PER_SEC: u64 = 1024;

static RTC_TICKS: AtomicU64 = AtomicU64::new(0);
static RTC_INITIALIZED: AtomicBool = AtomicBool::new(false);
static RTC_LAST_ALIVE_SECOND: AtomicU64 = AtomicU64::new(u64::MAX);
static RTC_LAST_XHCI_TRANSFER_COUNT: AtomicU64 = AtomicU64::new(0);
static RTC_LAST_HID_POINTER_REPORT_COUNT: AtomicU64 = AtomicU64::new(0);
static RTC_LAST_INPUT_PACKET_SUBMIT_COUNT: AtomicU64 = AtomicU64::new(0);
static RTC_LAST_INPUT_ABSOLUTE_SUBMIT_COUNT: AtomicU64 = AtomicU64::new(0);
static RTC_LAST_INPUT_READ_CALL_COUNT: AtomicU64 = AtomicU64::new(0);
static RTC_LAST_INPUT_READ_EVENT_COUNT: AtomicU64 = AtomicU64::new(0);
static RTC_LAST_LINUX_IRQ_LOCK_DEPTH: AtomicU64 = AtomicU64::new(0);
static RTC_SLEEP_WAITERS: Mutex<Vec<SleepWaiter>> = Mutex::new(Vec::new());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SleepWaiter {
    task_id: u64,
    wake_tick: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcDateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub weekday: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RawRtcDateTime {
    second: u8,
    minute: u8,
    hour: u8,
    weekday: u8,
    day: u8,
    month: u8,
    year: u8,
}

fn cmos_read(reg: u8) -> u8 {
    unsafe {
        let mut index_port: Port<u8> = Port::new(CMOS_INDEX_PORT);
        let mut data_port: Port<u8> = Port::new(CMOS_DATA_PORT);
        select_cmos_register(&mut index_port, reg, true);
        let value = data_port.read();
        select_cmos_register(&mut index_port, reg, false);
        value
    }
}

fn cmos_write(reg: u8, value: u8) {
    unsafe {
        let mut index_port: Port<u8> = Port::new(CMOS_INDEX_PORT);
        let mut data_port: Port<u8> = Port::new(CMOS_DATA_PORT);
        select_cmos_register(&mut index_port, reg, true);
        data_port.write(value);
        select_cmos_register(&mut index_port, reg, false);
    }
}

fn enable_nmi() {
    let mut index_port: Port<u8> = Port::new(CMOS_INDEX_PORT);
    select_cmos_register(&mut index_port, 0, false);
}

fn select_cmos_register(index_port: &mut Port<u8>, reg: u8, disable_nmi: bool) {
    let reg = reg & CMOS_REGISTER_MASK;
    let value = if disable_nmi { NMI_DISABLE | reg } else { reg };
    unsafe {
        index_port.write(value);
    }
}

fn update_in_progress() -> bool {
    (cmos_read(RTC_REG_A) & RTC_UPDATE_IN_PROGRESS) != 0
}

fn read_raw_datetime() -> RawRtcDateTime {
    RawRtcDateTime {
        second: cmos_read(RTC_REG_SECONDS),
        minute: cmos_read(RTC_REG_MINUTES),
        hour: cmos_read(RTC_REG_HOURS),
        weekday: cmos_read(RTC_REG_WEEKDAY),
        day: cmos_read(RTC_REG_DAY),
        month: cmos_read(RTC_REG_MONTH),
        year: cmos_read(RTC_REG_YEAR),
    }
}

fn bcd_to_binary(value: u8) -> u8 {
    (value & 0x0F) + ((value >> 4) * 10)
}

fn expand_year(year: u8) -> u16 {
    if year >= 70 {
        1900 + year as u16
    } else {
        2000 + year as u16
    }
}

fn decode_datetime(raw: RawRtcDateTime, reg_b: u8) -> RtcDateTime {
    let is_binary = (reg_b & (1 << 2)) != 0;
    let is_24_hour = (reg_b & (1 << 1)) != 0;
    let is_pm = (raw.hour & 0x80) != 0;

    let mut second = raw.second;
    let mut minute = raw.minute;
    let mut hour = raw.hour & 0x7F;
    let mut weekday = raw.weekday;
    let mut day = raw.day;
    let mut month = raw.month;
    let mut year = raw.year;

    if !is_binary {
        second = bcd_to_binary(second);
        minute = bcd_to_binary(minute);
        hour = bcd_to_binary(hour);
        weekday = bcd_to_binary(weekday);
        day = bcd_to_binary(day);
        month = bcd_to_binary(month);
        year = bcd_to_binary(year);
    }

    if !is_24_hour {
        hour %= 12;
        if is_pm {
            hour = hour.saturating_add(12);
        }
    }

    RtcDateTime {
        year: expand_year(year),
        month,
        day,
        weekday,
        hour,
        minute,
        second,
    }
}

fn read_stable_datetime() -> RtcDateTime {
    loop {
        while update_in_progress() {
            spin_loop();
        }
        let first = read_raw_datetime();

        while update_in_progress() {
            spin_loop();
        }
        let second = read_raw_datetime();

        if first == second {
            let reg_b = cmos_read(RTC_REG_B);
            return decode_datetime(second, reg_b);
        }

        spin_loop();
    }
}

pub fn now() -> RtcDateTime {
    interrupts::without_interrupts(read_stable_datetime)
}

pub fn ticks() -> u64 {
    RTC_TICKS.load(Ordering::Acquire)
}

pub const fn ticks_per_second() -> u64 {
    RTC_TICKS_PER_SEC
}

pub fn is_initialized() -> bool {
    RTC_INITIALIZED.load(Ordering::Acquire)
}

pub fn init() {
    interrupts::without_interrupts(|| {
        enable_nmi();

        // Program RTC periodic interrupt rate to 1024 Hz.
        let prev_a = cmos_read(RTC_REG_A);
        cmos_write(RTC_REG_A, (prev_a & 0xF0) | RTC_RATE_1024_HZ);

        let prev_b = cmos_read(RTC_REG_B);
        cmos_write(RTC_REG_B, prev_b | RTC_PERIODIC_INTERRUPT_ENABLE);

        // Read C once to clear any pending interrupt latch.
        let _ = cmos_read(RTC_REG_C);

        enable_nmi();
    });

    RTC_INITIALIZED.store(true, Ordering::Release);
    crate::arch::pic::enable_irq(8);
}

pub fn on_interrupt() {
    let ticks = RTC_TICKS.fetch_add(1, Ordering::AcqRel).saturating_add(1);
    wake_ready_sleepers(ticks);
    let current_second = ticks / RTC_TICKS_PER_SEC;
    let last_reported_second = RTC_LAST_ALIVE_SECOND.load(Ordering::Acquire);
    if current_second != last_reported_second
        && RTC_LAST_ALIVE_SECOND
            .compare_exchange(
                last_reported_second,
                current_second,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    {
        if !crate::debug::should_emit(diag_abi::DiagProvider::Heartbeat, diag_abi::DiagLevel::Info)
        {
            let _ = cmos_read(RTC_REG_C);
            return;
        }
        let snapshot = crate::hooks::heartbeat_snapshot();
        let xhci_transfer_count = snapshot.xhci_transfer_count;
        let hid_pointer_report_count = snapshot.hid_pointer_report_count;
        let input_snapshot = snapshot.input;
        let linux_irq_owner_count = snapshot.linux_irq_owner_count;
        let linux_irq_total_depth = snapshot.linux_irq_total_depth as usize;
        let linux_input_lock_active = snapshot.linux_input_lock_active;
        let linux_input_lock_last_seq = snapshot.linux_input_lock_last_seq;
        let xhci_delta = xhci_transfer_count.saturating_sub(
            RTC_LAST_XHCI_TRANSFER_COUNT.swap(xhci_transfer_count, Ordering::AcqRel),
        );
        let hid_pointer_delta = hid_pointer_report_count.saturating_sub(
            RTC_LAST_HID_POINTER_REPORT_COUNT.swap(hid_pointer_report_count, Ordering::AcqRel),
        );
        let input_packet_delta = input_snapshot.pointer_packet_submits.saturating_sub(
            RTC_LAST_INPUT_PACKET_SUBMIT_COUNT
                .swap(input_snapshot.pointer_packet_submits, Ordering::AcqRel),
        );
        let input_absolute_delta = input_snapshot.pointer_absolute_submits.saturating_sub(
            RTC_LAST_INPUT_ABSOLUTE_SUBMIT_COUNT
                .swap(input_snapshot.pointer_absolute_submits, Ordering::AcqRel),
        );
        let input_read_call_delta = input_snapshot.read_calls.saturating_sub(
            RTC_LAST_INPUT_READ_CALL_COUNT.swap(input_snapshot.read_calls, Ordering::AcqRel),
        );
        let input_read_event_delta = input_snapshot.read_events.saturating_sub(
            RTC_LAST_INPUT_READ_EVENT_COUNT.swap(input_snapshot.read_events, Ordering::AcqRel),
        );
        let linux_irq_depth_delta = (linux_irq_total_depth as u64).saturating_sub(
            RTC_LAST_LINUX_IRQ_LOCK_DEPTH.swap(linux_irq_total_depth as u64, Ordering::AcqRel),
        );
        crate::debug::emit_text(
            diag_abi::DiagProvider::Heartbeat,
            diag_abi::DiagLevel::Info,
            0,
            current_second,
            linux_irq_total_depth as u64,
            alloc::format!(
                "second={} userspace_display={} xhci_delta={} hid_ptr_delta={} input_packet_delta={} input_abs_delta={} input_read_calls_delta={} input_read_events_delta={} linux_irq_owners={} linux_irq_depth={} linux_irq_depth_delta={} input_lock_active={} input_lock_last_seq={} eventq_lock_active={} eventq_lock_last_seq={} queued={} pending_coalesced={} pending_pointer_position={} dropped_discrete={} dropped_lossy={}",
                current_second,
                snapshot.userspace_display_active,
                xhci_delta,
                hid_pointer_delta,
                input_packet_delta,
                input_absolute_delta,
                input_read_call_delta,
                input_read_event_delta,
                linux_irq_owner_count,
                linux_irq_total_depth,
                linux_irq_depth_delta,
                linux_input_lock_active,
                linux_input_lock_last_seq,
                input_snapshot.lock_active,
                input_snapshot.lock_last_seq,
                input_snapshot.queued,
                input_snapshot.pending_coalesced,
                input_snapshot.pending_pointer_position,
                input_snapshot.dropped_discrete,
                input_snapshot.dropped_lossy
            )
            .as_str(),
        );
    }
    // Must read register C to acknowledge and re-arm RTC interrupts.
    let _ = cmos_read(RTC_REG_C);
}

pub fn sleep(milliseconds: u64) {
    if milliseconds == 0 {
        return;
    }

    let ticks_needed = (milliseconds.saturating_mul(RTC_TICKS_PER_SEC) + 999) / 1000;
    let ticks_needed = core::cmp::max(1, ticks_needed);
    let target = RTC_TICKS
        .load(Ordering::Acquire)
        .saturating_add(ticks_needed);

    let restore_disabled = !interrupts::are_enabled();
    while RTC_TICKS.load(Ordering::Acquire) < target {
        if !restore_disabled && block_current_user_until(target) {
            continue;
        }
        if restore_disabled {
            interrupts::enable();
            hlt();
            interrupts::disable();
        } else if crate::hooks::is_scheduler_initialized() {
            crate::hooks::yield_now();
        } else {
            hlt();
        }
        spin_loop();
    }
}

fn block_current_user_until(target: u64) -> bool {
    if !crate::hooks::is_scheduler_initialized() {
        return false;
    }

    let Some(task_id) = crate::hooks::current_user_thread_id() else {
        return false;
    };

    let blocked = interrupts::without_interrupts(|| {
        if RTC_TICKS.load(Ordering::Acquire) >= target {
            return true;
        }
        if !crate::hooks::block_current_user_task() {
            return false;
        }
        register_sleep_waiter(task_id, target);
        true
    });
    if !blocked {
        return false;
    }

    crate::hooks::yield_now();
    true
}

fn register_sleep_waiter(task_id: u64, wake_tick: u64) {
    let mut waiters = RTC_SLEEP_WAITERS.lock();
    if let Some(waiter) = waiters.iter_mut().find(|waiter| waiter.task_id == task_id) {
        waiter.wake_tick = wake_tick;
        return;
    }
    waiters.push(SleepWaiter { task_id, wake_tick });
}

fn wake_ready_sleepers(now: u64) {
    loop {
        let task_id = {
            let mut waiters = RTC_SLEEP_WAITERS.lock();
            let Some(index) = waiters.iter().position(|waiter| waiter.wake_tick <= now) else {
                return;
            };
            waiters.swap_remove(index).task_id
        };
        let _ = crate::hooks::wake_user_task(task_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcd_to_binary_decodes_common_values() {
        assert_eq!(bcd_to_binary(0x00), 0);
        assert_eq!(bcd_to_binary(0x42), 42);
        assert_eq!(bcd_to_binary(0x59), 59);
    }

    #[test]
    fn expand_year_uses_expected_century_split() {
        assert_eq!(expand_year(69), 2069);
        assert_eq!(expand_year(70), 1970);
        assert_eq!(expand_year(99), 1999);
    }

    #[test]
    fn decode_datetime_handles_bcd_12_hour_pm() {
        let raw = RawRtcDateTime {
            second: 0x58,
            minute: 0x23,
            hour: 0x89,
            weekday: 0x06,
            day: 0x21,
            month: 0x12,
            year: 0x24,
        };
        let decoded = decode_datetime(raw, 0);

        assert_eq!(
            decoded,
            RtcDateTime {
                year: 2024,
                month: 12,
                day: 21,
                weekday: 6,
                hour: 21,
                minute: 23,
                second: 58,
            }
        );
    }

    #[test]
    fn decode_datetime_preserves_binary_24_hour_input() {
        let raw = RawRtcDateTime {
            second: 7,
            minute: 8,
            hour: 19,
            weekday: 4,
            day: 3,
            month: 2,
            year: 70,
        };
        let decoded = decode_datetime(raw, (1 << 2) | (1 << 1));

        assert_eq!(
            decoded,
            RtcDateTime {
                year: 1970,
                month: 2,
                day: 3,
                weekday: 4,
                hour: 19,
                minute: 8,
                second: 7,
            }
        );
    }

    #[test]
    fn select_cmos_register_masks_register_bit_and_controls_nmi_flag() {
        let disabled = (0x4a & CMOS_REGISTER_MASK) | NMI_DISABLE;
        let enabled = 0x4a & CMOS_REGISTER_MASK;

        assert_eq!(disabled, 0xca);
        assert_eq!(enabled, 0x4a);
        assert_eq!(disabled & NMI_DISABLE, NMI_DISABLE);
        assert_eq!(enabled & NMI_DISABLE, 0);
    }
}
