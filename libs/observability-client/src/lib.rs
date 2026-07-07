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
    let emitted = unsafe {
        libc::syscall(
            SYS_RUSTOS_DEBUG_PRINT,
            line.as_ptr() as usize,
            line.len(),
        )
    };
    if emitted < 0 {
        let _ = std::io::stderr().write_all(line.as_bytes());
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
