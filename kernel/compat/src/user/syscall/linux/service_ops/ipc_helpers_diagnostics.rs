//! Bounded typed-service latency and terminal-failure diagnostics.
//!
//! Slow success samples and terminal failures use independent one-per-second
//! lanes.  Otherwise an earlier slow success can hide the exact operation that
//! crossed its deadline, forcing an SMP failure to be diagnosed by inference.

use super::*;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const EARLY_SERVICE_CALL_SAMPLES: usize = 6;
const SLOW_SERVICE_CALL_THRESHOLD_MS: u64 = 10;
const MAX_SERVICE_CALL_LOGS_PER_SECOND: u8 = 1;

static SERVICE_CALL_SAMPLE_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLOW_SERVICE_CALL_LOG_RATE_STATE: AtomicU64 = AtomicU64::new(u64::MAX);
static FAILED_SERVICE_CALL_LOG_RATE_STATE: AtomicU64 = AtomicU64::new(u64::MAX);

fn rate_limit_permit(state: &AtomicU64) -> bool {
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    let window = crate::arch::rtc::ticks() / ticks_per_second;
    super::super::ipc_ops::diagnostic_rate_limit_permit(
        state,
        window,
        MAX_SERVICE_CALL_LOGS_PER_SECOND,
    )
}

pub(super) fn log_slow_service_call(
    service: &str,
    op: u16,
    elapsed_ms: u64,
    pid: u64,
    tid: u64,
    status_or_len: i64,
    detail: Option<&str>,
) {
    let sample_index = SERVICE_CALL_SAMPLE_COUNT.fetch_add(1, Ordering::Relaxed);
    if sample_index >= EARLY_SERVICE_CALL_SAMPLES && elapsed_ms < SLOW_SERVICE_CALL_THRESHOLD_MS {
        return;
    }
    if !rate_limit_permit(&SLOW_SERVICE_CALL_LOG_RATE_STATE) {
        return;
    }
    emit(
        "slow",
        service,
        op,
        elapsed_ms,
        pid,
        tid,
        status_or_len,
        detail,
    );
}

pub(super) fn log_failed_service_call(
    service: &str,
    op: u16,
    elapsed_ms: u64,
    pid: u64,
    tid: u64,
    errno: i64,
    detail: Option<&str>,
) {
    if !rate_limit_permit(&FAILED_SERVICE_CALL_LOG_RATE_STATE) {
        return;
    }
    emit("failed", service, op, elapsed_ms, pid, tid, errno, detail);
}

fn emit(
    outcome: &str,
    service: &str,
    op: u16,
    elapsed_ms: u64,
    pid: u64,
    tid: u64,
    status_or_len: i64,
    detail: Option<&str>,
) {
    if let Some(detail) = detail {
        debug::println!(
            "service ipc {}: service={} op={} elapsed_ms={} pid={} tid={} status_or_len={} detail={}",
            outcome,
            service,
            op,
            elapsed_ms,
            pid,
            tid,
            status_or_len,
            detail,
        );
    } else {
        debug::println!(
            "service ipc {}: service={} op={} elapsed_ms={} pid={} tid={} status_or_len={}",
            outcome,
            service,
            op,
            elapsed_ms,
            pid,
            tid,
            status_or_len,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_failure_diagnostic_has_an_independent_bounded_lane() {
        assert_eq!(MAX_SERVICE_CALL_LOGS_PER_SECOND, 1);
        assert!(!core::ptr::eq(
            &SLOW_SERVICE_CALL_LOG_RATE_STATE,
            &FAILED_SERVICE_CALL_LOG_RATE_STATE,
        ));
    }
}
