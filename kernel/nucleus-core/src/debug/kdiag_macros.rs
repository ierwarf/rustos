include!(concat!(env!("OUT_DIR"), "/logging_macros.rs"));

#[doc(hidden)]
#[macro_export]
macro_rules! __rustos_log_level {
    (trace) => {
        $crate::debug::LogLevel::Trace
    };
    (debug) => {
        $crate::debug::LogLevel::Debug
    };
    (info) => {
        $crate::debug::LogLevel::Info
    };
    (warn) => {
        $crate::debug::LogLevel::Warn
    };
    (error) => {
        $crate::debug::LogLevel::Error
    };
    (fatal) => {
        $crate::debug::LogLevel::Fatal
    };
}

#[macro_export]
macro_rules! __rustos_debug_enabled {
    ($category:ident, $level:ident) => {{
        let mut __enabled = false;
        $crate::__rustos_log_if_enabled!($category, $level, {
            __enabled = $crate::debug::should_emit(
                $crate::__rustos_log_category!($category),
                $crate::__rustos_log_level!($level),
            );
        });
        __enabled
    }};
}

#[macro_export]
macro_rules! __rustos_debug_log {
    ($category:ident, $level:ident, $message:expr $(,)?) => {{
        $crate::__rustos_log_if_enabled!($category, $level, {
            if $crate::debug::should_emit(
                $crate::__rustos_log_category!($category),
                $crate::__rustos_log_level!($level),
            ) {
                $crate::debug::log_args_site(
                    $crate::__rustos_log_category!($category),
                    $crate::__rustos_log_level!($level),
                    module_path!(),
                    line!(),
                    format_args!("{}", $message),
                );
            }
        });
    }};
    ($category:ident, $level:ident, $($arg:tt)+) => {{
        $crate::__rustos_log_if_enabled!($category, $level, {
            if $crate::debug::should_emit(
                $crate::__rustos_log_category!($category),
                $crate::__rustos_log_level!($level),
            ) {
                $crate::debug::log_args_site(
                    $crate::__rustos_log_category!($category),
                    $crate::__rustos_log_level!($level),
                    module_path!(),
                    line!(),
                    format_args!($($arg)+),
                );
            }
        });
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __rustos_debug_ratelimited {
    ($category:ident, $level:ident, $message:expr $(,)?) => {{
        $crate::__rustos_log_if_enabled!($category, $level, {
            static __RUSTOS_DEBUG_LAST_EMIT: core::sync::atomic::AtomicU64 =
                core::sync::atomic::AtomicU64::new(0);
            if $crate::debug::rate_limit_permit(
                &__RUSTOS_DEBUG_LAST_EMIT,
                $crate::debug::DEFAULT_LOG_RATE_LIMIT_INTERVAL_MICROS,
            ) && $crate::debug::should_emit(
                $crate::__rustos_log_category!($category),
                $crate::__rustos_log_level!($level),
            ) {
                $crate::debug::log_args_site(
                    $crate::__rustos_log_category!($category),
                    $crate::__rustos_log_level!($level),
                    module_path!(),
                    line!(),
                    format_args!("{}", $message),
                );
            }
        });
    }};
    ($category:ident, $level:ident, $($arg:tt)+) => {{
        $crate::__rustos_log_if_enabled!($category, $level, {
            static __RUSTOS_DEBUG_LAST_EMIT: core::sync::atomic::AtomicU64 =
                core::sync::atomic::AtomicU64::new(0);
            if $crate::debug::rate_limit_permit(
                &__RUSTOS_DEBUG_LAST_EMIT,
                $crate::debug::DEFAULT_LOG_RATE_LIMIT_INTERVAL_MICROS,
            ) && $crate::debug::should_emit(
                $crate::__rustos_log_category!($category),
                $crate::__rustos_log_level!($level),
            ) {
                $crate::debug::log_args_site(
                    $crate::__rustos_log_category!($category),
                    $crate::__rustos_log_level!($level),
                    module_path!(),
                    line!(),
                    format_args!($($arg)+),
                );
            }
        });
    }};
}

#[macro_export]
macro_rules! __rustos_debug_trace {
    ($category:ident, $($arg:tt)+) => {
        $crate::__rustos_debug_log!($category, trace, $($arg)+)
    };
}

#[macro_export]
macro_rules! __rustos_debug_debug {
    ($category:ident, $($arg:tt)+) => {
        $crate::__rustos_debug_log!($category, debug, $($arg)+)
    };
}

#[macro_export]
macro_rules! __rustos_debug_info {
    ($category:ident, $($arg:tt)+) => {
        $crate::__rustos_debug_log!($category, info, $($arg)+)
    };
}

#[macro_export]
macro_rules! __rustos_debug_warn {
    ($category:ident, $($arg:tt)+) => {
        $crate::__rustos_debug_log!($category, warn, $($arg)+)
    };
}

#[macro_export]
macro_rules! __rustos_debug_error {
    ($category:ident, $($arg:tt)+) => {
        $crate::__rustos_debug_log!($category, error, $($arg)+)
    };
}

#[macro_export]
macro_rules! __rustos_debug_warn_ratelimited {
    ($category:ident, $($arg:tt)+) => {
        $crate::__rustos_debug_ratelimited!($category, warn, $($arg)+)
    };
}

#[macro_export]
macro_rules! __rustos_debug_error_ratelimited {
    ($category:ident, $($arg:tt)+) => {
        $crate::__rustos_debug_ratelimited!($category, error, $($arg)+)
    };
}
