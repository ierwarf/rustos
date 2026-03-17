use super::*;

pub(crate) fn clock_gettime(clock_id: u64, timespec_ptr: u64) -> Result<(), LinuxSysopError> {
    let timespec = match clock_id {
        linux_abi::CLOCK_REALTIME => realtime_timespec(),
        linux_abi::CLOCK_MONOTONIC => monotonic_timespec(),
        _ => return Err(LinuxSysopError::InvalidArgument),
    };
    write_user_timespec(timespec_ptr, &timespec)
}

pub(crate) fn nanosleep(request_ptr: u64, remaining_ptr: u64) -> Result<(), LinuxSysopError> {
    let request = read_user_timespec(request_ptr)?;
    sleep_for_timespec(&request)?;
    write_zero_timespec(remaining_ptr)
}

pub(crate) fn clock_nanosleep(
    clock_id: u64,
    flags: u64,
    request_ptr: u64,
    remaining_ptr: u64,
) -> Result<(), LinuxSysopError> {
    let request = read_user_timespec(request_ptr)?;
    match clock_id {
        linux_abi::CLOCK_REALTIME | linux_abi::CLOCK_MONOTONIC => {}
        _ => return Err(LinuxSysopError::InvalidArgument),
    }
    if flags & !linux_abi::TIMER_ABSTIME != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    if flags & linux_abi::TIMER_ABSTIME != 0 {
        let now = match clock_id {
            linux_abi::CLOCK_REALTIME => realtime_timespec(),
            linux_abi::CLOCK_MONOTONIC => monotonic_timespec(),
            _ => unreachable!(),
        };
        if let Some(remaining) = saturating_timespec_sub(&request, &now) {
            sleep_for_timespec(&remaining)?;
        }
    } else {
        sleep_for_timespec(&request)?;
    }

    write_zero_timespec(remaining_ptr)
}

fn write_user_timespec(
    timespec_ptr: u64,
    timespec: &linux_abi::LinuxTimespec,
) -> Result<(), LinuxSysopError> {
    let bytes = unsafe {
        slice::from_raw_parts(
            (timespec as *const linux_abi::LinuxTimespec).cast::<u8>(),
            size_of::<linux_abi::LinuxTimespec>(),
        )
    };
    usermem::write_current_user_bytes(timespec_ptr, bytes)?;
    Ok(())
}

fn realtime_timespec() -> linux_abi::LinuxTimespec {
    let now = rtc::now();
    linux_abi::LinuxTimespec {
        tv_sec: unix_seconds_from_rtc(now),
        tv_nsec: 0,
    }
}

fn monotonic_timespec() -> linux_abi::LinuxTimespec {
    let ticks = rtc::ticks();
    let ticks_per_second = rtc::ticks_per_second().max(1);
    let seconds = ticks / ticks_per_second;
    let tick_remainder = ticks % ticks_per_second;
    let nanoseconds =
        ((tick_remainder as u128) * 1_000_000_000_u128 / (ticks_per_second as u128)) as i64;
    linux_abi::LinuxTimespec {
        tv_sec: seconds.min(i64::MAX as u64) as i64,
        tv_nsec: nanoseconds,
    }
}

fn read_user_timespec(user_ptr: u64) -> Result<linux_abi::LinuxTimespec, LinuxSysopError> {
    let mut request = linux_abi::LinuxTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let request_bytes = unsafe {
        core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(request).cast::<u8>(),
            core::mem::size_of::<linux_abi::LinuxTimespec>(),
        )
    };
    usermem::copy_from_current_user_exact(user_ptr, request_bytes)?;
    validate_timespec(&request)?;
    Ok(request)
}

fn validate_timespec(timespec: &linux_abi::LinuxTimespec) -> Result<(), LinuxSysopError> {
    if timespec.tv_sec < 0 || !(0..1_000_000_000).contains(&timespec.tv_nsec) {
        return Err(LinuxSysopError::InvalidArgument);
    }
    Ok(())
}

fn sleep_for_timespec(timespec: &linux_abi::LinuxTimespec) -> Result<(), LinuxSysopError> {
    validate_timespec(timespec)?;
    let seconds = u64::try_from(timespec.tv_sec).map_err(|_| LinuxSysopError::InvalidArgument)?;
    let nanoseconds =
        u64::try_from(timespec.tv_nsec).map_err(|_| LinuxSysopError::InvalidArgument)?;
    let milliseconds = seconds
        .checked_mul(1_000)
        .and_then(|value| value.checked_add(nanoseconds.div_ceil(1_000_000)))
        .unwrap_or(u64::MAX);
    rtc::sleep(milliseconds);
    Ok(())
}

fn write_zero_timespec(user_ptr: u64) -> Result<(), LinuxSysopError> {
    if user_ptr == 0 {
        return Ok(());
    }

    let zero = linux_abi::LinuxTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    write_user_timespec(user_ptr, &zero)
}

fn saturating_timespec_sub(
    target: &linux_abi::LinuxTimespec,
    current: &linux_abi::LinuxTimespec,
) -> Option<linux_abi::LinuxTimespec> {
    let target_ns = timespec_to_nanos(target)?;
    let current_ns = timespec_to_nanos(current)?;
    if target_ns <= current_ns {
        return None;
    }

    let delta_ns = target_ns - current_ns;
    Some(linux_abi::LinuxTimespec {
        tv_sec: i64::try_from(delta_ns / 1_000_000_000).ok()?,
        tv_nsec: i64::try_from(delta_ns % 1_000_000_000).ok()?,
    })
}

fn timespec_to_nanos(timespec: &linux_abi::LinuxTimespec) -> Option<u128> {
    let seconds = u128::try_from(timespec.tv_sec).ok()?;
    let nanoseconds = u128::try_from(timespec.tv_nsec).ok()?;
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanoseconds))
}

fn unix_seconds_from_rtc(datetime: rtc::RtcDateTime) -> i64 {
    let days = days_from_civil(
        i32::from(datetime.year),
        u32::from(datetime.month),
        u32::from(datetime.day),
    );
    let seconds_in_day = i64::from(datetime.hour) * 3600
        + i64::from(datetime.minute) * 60
        + i64::from(datetime.second);
    days.saturating_mul(86_400).saturating_add(seconds_in_day)
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i32;
    let day = day as i32;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era) * 146_097 + i64::from(doe) - 719_468
}

#[cfg(test)]
mod tests {
    use super::{days_from_civil, unix_seconds_from_rtc};
    use crate::rtc::RtcDateTime;

    #[test]
    fn unix_epoch_day_is_zero() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn rtc_datetime_converts_to_unix_seconds() {
        assert_eq!(
            unix_seconds_from_rtc(RtcDateTime {
                year: 1970,
                month: 1,
                day: 1,
                weekday: 4,
                hour: 0,
                minute: 0,
                second: 0,
            }),
            0
        );
        assert_eq!(
            unix_seconds_from_rtc(RtcDateTime {
                year: 1970,
                month: 1,
                day: 2,
                weekday: 5,
                hour: 1,
                minute: 1,
                second: 1,
            }),
            90_061
        );
    }
}
