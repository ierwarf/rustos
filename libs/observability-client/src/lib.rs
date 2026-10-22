use std::fmt;
use std::fmt::Write as FmtWrite;
use std::io::Write as IoWrite;
use std::time::{SystemTime, UNIX_EPOCH};

pub use rustos_observability::{LogCategory, LogLevel};

include!(concat!(env!("OUT_DIR"), "/logging_helpers.rs"));

const SYS_RUSTOS_DEBUG_PRINT: libc::c_long = 0x5255_0001;

fn level_prefix(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Trace => "[TRACE]",
        LogLevel::Debug => "[DEBUG]",
        LogLevel::Info => "[INFO ]",
        LogLevel::Warn => "[WARN ]",
        LogLevel::Error | LogLevel::Fatal => "[ERROR]",
    }
}

fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub fn should_emit(category: LogCategory, level: LogLevel) -> bool {
    compiled_level_enabled(category, level)
}

pub fn log_args(service: &str, category: LogCategory, level: LogLevel, args: fmt::Arguments<'_>) {
    if !should_emit(category, level) {
        return;
    }

    let mut line = String::new();
    let _ = write!(
        line,
        "{} ts={} service={} cat={} lvl={} {}",
        level_prefix(level),
        timestamp_millis(),
        service,
        category.as_str(),
        level.as_str(),
        args
    );
    line.push('\n');
    debug_bytes(line.as_bytes());
}

/// Emit already-framed diagnostic bytes with bounded suffix-only retries and
/// stderr fallback for precisely the unaccepted suffix.
pub fn debug_bytes(bytes: &[u8]) {
    let written = write_debug_bytes_with(
        bytes,
        |remaining| {
            let result = unsafe {
                libc::syscall(
                    SYS_RUSTOS_DEBUG_PRINT,
                    remaining.as_ptr() as usize,
                    remaining.len(),
                )
            };
            if result < 0 {
                -(std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO) as i64)
            } else {
                result as i64
            }
        },
        std::thread::yield_now,
    );
    if written < bytes.len() {
        // A short write already emitted its prefix; never duplicate it in the
        // fallback, and never mistake a nonnegative prefix for a full line.
        let _ = std::io::stderr().write_all(&bytes[written..]);
    }
}

fn write_debug_bytes_with(
    bytes: &[u8],
    mut write: impl FnMut(&[u8]) -> i64,
    mut yield_now: impl FnMut(),
) -> usize {
    let mut written = 0;
    for _ in 0..64 {
        if written == bytes.len() {
            break;
        }
        let result = write(&bytes[written..]);
        if result > 0 && result as usize <= bytes.len() - written {
            written += result as usize;
        } else if result != -(libc::EAGAIN as i64) {
            break;
        }
        if written != bytes.len() {
            yield_now();
        }
    }
    written
}

#[cfg(test)]
mod diagnostic_tests {
    #[test]
    fn short_write_then_refusal_never_duplicates_prefix() {
        let mut responses = [2, -(libc::EAGAIN as i64), 3].into_iter();
        let mut lengths = Vec::new();
        assert_eq!(
            super::write_debug_bytes_with(
                b"hello",
                |remaining| {
                    lengths.push(remaining.len());
                    responses.next().unwrap()
                },
                || {}
            ),
            5
        );
        assert_eq!(lengths, [5, 3, 3]);
        let mut calls = 0;
        assert_eq!(
            super::write_debug_bytes_with(
                b"hello",
                |_| {
                    calls += 1;
                    if calls == 1 {
                        2
                    } else {
                        -5
                    }
                },
                || {}
            ),
            2
        );
        assert_eq!(super::write_debug_bytes_with(b"hello", |_| -11, || {}), 0);
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! __observability_level {
    (trace) => {
        $crate::LogLevel::Trace
    };
    (debug) => {
        $crate::LogLevel::Debug
    };
    (info) => {
        $crate::LogLevel::Info
    };
    (warn) => {
        $crate::LogLevel::Warn
    };
    (error) => {
        $crate::LogLevel::Error
    };
    (fatal) => {
        $crate::LogLevel::Fatal
    };
}

#[macro_export]
macro_rules! enabled {
    ($category:ident, $level:ident) => {{
        let mut __enabled = false;
        $crate::__observability_if_enabled!($category, $level, {
            __enabled = $crate::should_emit(
                $crate::__observability_category!($category),
                $crate::__observability_level!($level),
            );
        });
        __enabled
    }};
}

#[macro_export]
macro_rules! log {
    ($service:expr, $category:ident, $level:ident, $($arg:tt)+) => {{
        $crate::__observability_if_enabled!($category, $level, {
            if $crate::should_emit(
                $crate::__observability_category!($category),
                $crate::__observability_level!($level),
            ) {
                $crate::log_args(
                    $service,
                    $crate::__observability_category!($category),
                    $crate::__observability_level!($level),
                    format_args!($($arg)+),
                );
            }
        });
    }};
}

#[macro_export]
macro_rules! trace {
    ($service:expr, $category:ident, $($arg:tt)+) => {
        $crate::log!($service, $category, trace, $($arg)+)
    };
}

#[macro_export]
macro_rules! debug {
    ($service:expr, $category:ident, $($arg:tt)+) => {
        $crate::log!($service, $category, debug, $($arg)+)
    };
}

#[macro_export]
macro_rules! info {
    ($service:expr, $category:ident, $($arg:tt)+) => {
        $crate::log!($service, $category, info, $($arg)+)
    };
}

#[macro_export]
macro_rules! warn {
    ($service:expr, $category:ident, $($arg:tt)+) => {
        $crate::log!($service, $category, warn, $($arg)+)
    };
}

#[macro_export]
macro_rules! error {
    ($service:expr, $category:ident, $($arg:tt)+) => {
        $crate::log!($service, $category, error, $($arg)+)
    };
}
