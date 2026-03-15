use core::hint::spin_loop;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::{hlt, interrupts, port::Port};

const CMOS_INDEX_PORT: u16 = 0x70;
const CMOS_DATA_PORT: u16 = 0x71;
const NMI_DISABLE: u8 = 0x80;

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

impl RtcDateTime {
    fn pack(self) -> u64 {
        ((self.year as u64) << 48)
            | ((self.month as u64) << 40)
            | ((self.day as u64) << 32)
            | ((self.weekday as u64) << 24)
            | ((self.hour as u64) << 16)
            | ((self.minute as u64) << 8)
            | (self.second as u64)
    }
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
        index_port.write(NMI_DISABLE | reg);
        data_port.read()
    }
}

fn cmos_write(reg: u8, value: u8) {
    unsafe {
        let mut index_port: Port<u8> = Port::new(CMOS_INDEX_PORT);
        let mut data_port: Port<u8> = Port::new(CMOS_DATA_PORT);
        index_port.write(NMI_DISABLE | reg);
        data_port.write(value);
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

fn splitmix64(mut state: u64) -> u64 {
    state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub fn now() -> RtcDateTime {
    interrupts::without_interrupts(read_stable_datetime)
}

pub fn seed() -> u64 {
    interrupts::without_interrupts(|| {
        let datetime = read_stable_datetime().pack();
        let ticks = RTC_TICKS.load(Ordering::Acquire);
        splitmix64(datetime ^ ticks.rotate_left(21) ^ ticks.wrapping_mul(0xA076_1D64_78BD_642F))
    })
}

pub fn ticks() -> u64 {
    RTC_TICKS.load(Ordering::Acquire)
}

pub const fn ticks_per_second() -> u64 {
    RTC_TICKS_PER_SEC
}

pub fn init() {
    interrupts::without_interrupts(|| {
        // Program RTC periodic interrupt rate to 1024 Hz.
        let prev_a = cmos_read(RTC_REG_A);
        cmos_write(RTC_REG_A, (prev_a & 0xF0) | RTC_RATE_1024_HZ);

        let prev_b = cmos_read(RTC_REG_B);
        cmos_write(RTC_REG_B, prev_b | RTC_PERIODIC_INTERRUPT_ENABLE);

        // Read C once to clear any pending interrupt latch.
        let _ = cmos_read(RTC_REG_C);
    });

    crate::pic::enable_irq(8);
}

pub fn on_interrupt() {
    RTC_TICKS.fetch_add(1, Ordering::Release);
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
        if restore_disabled {
            interrupts::enable();
            hlt();
            interrupts::disable();
        } else {
            hlt();
        }
        spin_loop();
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
}
