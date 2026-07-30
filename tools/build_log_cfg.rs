pub const LOG_LEVEL_OFF: u8 = 0;
pub const LOG_LEVEL_TRACE: u8 = 1;
pub const LOG_LEVEL_DEBUG: u8 = 2;
pub const LOG_LEVEL_INFO: u8 = 3;
pub const LOG_LEVEL_WARN: u8 = 4;
pub const LOG_LEVEL_ERROR: u8 = 5;
pub const LOG_LEVEL_FATAL: u8 = 6;

pub const LOG_CATEGORIES: [&str; 17] = [
    "boot",
    "panic",
    "memory",
    "sched",
    "syscall",
    "process",
    "driver",
    "storage",
    "usb",
    "input",
    "display",
    "vfs",
    "console",
    "service",
    "compat",
    "debug",
    "heartbeat",
];

pub const LOG_LEVELS: [(&str, u8); 6] = [
    ("trace", LOG_LEVEL_TRACE),
    ("debug", LOG_LEVEL_DEBUG),
    ("info", LOG_LEVEL_INFO),
    ("warn", LOG_LEVEL_WARN),
    ("error", LOG_LEVEL_ERROR),
    ("fatal", LOG_LEVEL_FATAL),
];

pub fn emit_project_config_rerun(config_path: &std::path::Path) {
    println!("cargo:rerun-if-changed={}", config_path.display());
}

pub fn read_project_config(config_path: &std::path::Path) -> std::io::Result<String> {
    std::fs::read_to_string(config_path)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoggingConfig {
    pub enabled: bool,
    pub boot_trace_enabled: bool,
    pub serial_mirror: bool,
    pub ring_buffer_bytes: usize,
    pub min_level: u8,
    pub category_levels: [u8; LOG_CATEGORIES.len()],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LockTelemetryConfig {
    pub enabled: bool,
    pub warn_wait_cycles: u64,
    pub warn_hold_cycles: u64,
}

impl LockTelemetryConfig {
    pub const fn default() -> Self {
        Self {
            enabled: false,
            warn_wait_cycles: 250_000,
            warn_hold_cycles: 250_000,
        }
    }
}

impl LoggingConfig {
    pub const fn default() -> Self {
        Self {
            enabled: true,
            boot_trace_enabled: true,
            serial_mirror: true,
            ring_buffer_bytes: 16 * 1024,
            min_level: LOG_LEVEL_INFO,
            category_levels: [LOG_LEVEL_INFO; LOG_CATEGORIES.len()],
        }
    }

    pub fn level_for_category(&self, category: &str) -> u8 {
        let Some(index) = category_index(category) else {
            panic!("unknown log category: {category}");
        };
        self.category_levels[index]
    }
}

pub fn emit_log_cfgs(logging_toml: &str) {
    let config = parse_logging_toml(logging_toml);
    emit_check_cfgs();
    emit_logging_env(&config);
    emit_lock_telemetry_cfgs(logging_toml);
    if !config.enabled {
        return;
    }

    println!("cargo:rustc-cfg=rustos_debug_print_enabled");
    for category in LOG_CATEGORIES {
        let compile_level = config.level_for_category(category);
        if compile_level == LOG_LEVEL_OFF {
            continue;
        }
        for (level_name, level_value) in LOG_LEVELS {
            if level_value >= compile_level {
                println!("cargo:rustc-cfg=rustos_log_{category}_{level_name}");
            }
        }
    }
}

// GENERATED: Shared build-script source is included by consumers that select
// different logging outputs.
#[allow(dead_code)]
pub fn generate_kernel_logging_build_rs(config: &LoggingConfig) -> String {
    let mut generated = format!(
        "pub const RUSTOS_LOGGING_ENABLED: bool = {};\n\
         pub const RUSTOS_LOGGING_BOOT_TRACE_ENABLED: bool = {};\n\
         pub const RUSTOS_LOGGING_SERIAL_MIRROR: bool = {};\n\
         pub const RUSTOS_LOGGING_RING_BUFFER_BYTES: usize = {};\n\
         pub const RUSTOS_LOGGING_MIN_LEVEL: u8 = {};\n\n",
        config.enabled,
        config.boot_trace_enabled,
        config.serial_mirror,
        config.ring_buffer_bytes,
        config.min_level,
    );
    generated.push_str(&generate_compiled_level_enabled_fn(
        "LogCategory",
        "LogLevel",
    ));
    generated
}

// GENERATED: Used only by the kernel logging-code generation include site.
#[allow(dead_code)]
pub fn generate_kernel_logging_macros_rs() -> String {
    let mut generated = String::new();
    generated.push_str(&generate_category_macro(
        "__rustos_log_category",
        "$crate::debug::LogCategory",
    ));
    generated.push('\n');
    generated.push_str(&generate_if_enabled_macro("__rustos_log_if_enabled"));
    generated
}

// GENERATED: Used only by service/runtime logging-code generation include sites.
#[allow(dead_code)]
pub fn generate_user_logging_helpers_rs() -> String {
    let mut generated = String::new();
    generated.push_str(&generate_compiled_level_enabled_fn(
        "LogCategory",
        "LogLevel",
    ));
    generated.push('\n');
    generated.push_str(&generate_category_macro(
        "__observability_category",
        "$crate::LogCategory",
    ));
    generated.push('\n');
    generated.push_str(&generate_if_enabled_macro("__observability_if_enabled"));
    generated
}

pub fn emit_check_cfgs() {
    println!("cargo:rustc-check-cfg=cfg(rustos_debug_print_enabled)");
    println!("cargo:rustc-check-cfg=cfg(rustos_boot_trace_enabled)");
    println!("cargo:rustc-check-cfg=cfg(rustos_lock_telemetry_enabled)");
    for category in LOG_CATEGORIES {
        for (level, _) in LOG_LEVELS {
            println!("cargo:rustc-check-cfg=cfg(rustos_log_{category}_{level})");
        }
    }
}

pub fn emit_lock_telemetry_cfgs(project_toml: &str) {
    println!("cargo:rerun-if-env-changed=RUSTOS_LOCK_TELEMETRY");
    println!("cargo:rerun-if-env-changed=RUSTOS_LOCK_TELEMETRY_WARN_WAIT_CYCLES");
    println!("cargo:rerun-if-env-changed=RUSTOS_LOCK_TELEMETRY_WARN_HOLD_CYCLES");
    let config = lock_telemetry_with_env_overrides(parse_lock_telemetry_toml(project_toml));
    println!(
        "cargo:rustc-env=RUSTOS_LOCK_TELEMETRY_ENABLED={}",
        bool_name(config.enabled)
    );
    println!(
        "cargo:rustc-env=RUSTOS_LOCK_TELEMETRY_WARN_WAIT_CYCLES={}",
        config.warn_wait_cycles
    );
    println!(
        "cargo:rustc-env=RUSTOS_LOCK_TELEMETRY_WARN_HOLD_CYCLES={}",
        config.warn_hold_cycles
    );
    if config.enabled {
        println!("cargo:rustc-cfg=rustos_lock_telemetry_enabled");
    }
}

// GENERATED: Some build-script include sites emit only the aggregate logging
// configuration and intentionally omit this compatibility cfg.
#[allow(dead_code)]
pub fn emit_boot_trace_cfg(logging_toml: &str) {
    if parse_logging_toml(logging_toml).boot_trace_enabled {
        println!("cargo:rustc-cfg=rustos_boot_trace_enabled");
    }
}

pub fn emit_logging_env(config: &LoggingConfig) {
    println!(
        "cargo:rustc-env=RUSTOS_LOGGING_ENABLED={}",
        bool_name(config.enabled)
    );
    println!(
        "cargo:rustc-env=RUSTOS_LOGGING_BOOT_TRACE_ENABLED={}",
        bool_name(config.boot_trace_enabled)
    );
    println!(
        "cargo:rustc-env=RUSTOS_LOGGING_SERIAL_MIRROR={}",
        bool_name(config.serial_mirror)
    );
    println!(
        "cargo:rustc-env=RUSTOS_LOGGING_RING_BUFFER_BYTES={}",
        config.ring_buffer_bytes
    );
    println!(
        "cargo:rustc-env=RUSTOS_LOGGING_MIN_LEVEL={}",
        level_name(config.min_level)
    );
}

pub fn parse_logging_toml(source: &str) -> LoggingConfig {
    try_parse_logging_toml(source).unwrap_or_else(|err| panic!("{err}"))
}

pub fn try_parse_logging_toml(source: &str) -> Result<LoggingConfig, String> {
    let mut config = LoggingConfig::default();
    config.category_levels = [config.min_level; LOG_CATEGORIES.len()];
    let mut section = "";

    for raw_line in source.lines() {
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }

        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            section = name.trim();
            continue;
        }

        let logging_section = matches!(section, "" | "logging" | "categories" | "logging.categories")
            || section.starts_with("logging.");
        if !logging_section {
            continue;
        }

        let Some((raw_key, raw_value)) = line.split_once('=') else {
            return Err(format!("invalid logging config line: {line}"));
        };
        let key = raw_key.trim();
        let value = raw_value.trim();
        match section {
            "" | "logging" => match key {
                "enabled" => config.enabled = parse_bool_value(value, key)?,
                "boot_trace_enabled" => {
                    config.boot_trace_enabled = parse_bool_value(value, key)?
                }
                "serial_mirror" => config.serial_mirror = parse_bool_value(value, key)?,
                "ring_buffer_bytes" => config.ring_buffer_bytes = parse_usize_value(value, key)?,
                "min_level" => {
                    config.min_level = parse_level_value(value)?;
                    config.category_levels = [config.min_level; LOG_CATEGORIES.len()];
                }
                other => return Err(format!("unknown logging config key: {other}")),
            },
            "categories" | "logging.categories" => {
                let Some(index) = category_index(key) else {
                    return Err(format!("unknown logging category: {key}"));
                };
                config.category_levels[index] = parse_level_value(value)?;
            }
            other if other.starts_with("logging.") => {
                return Err(format!("unsupported logging config section: {other}"));
            }
            _ => {}
        }
    }

    Ok(config)
}

pub fn parse_lock_telemetry_toml(source: &str) -> LockTelemetryConfig {
    try_parse_lock_telemetry_toml(source).unwrap_or_else(|err| panic!("{err}"))
}

pub fn lock_telemetry_with_env_overrides(
    mut config: LockTelemetryConfig,
) -> LockTelemetryConfig {
    if let Ok(value) = std::env::var("RUSTOS_LOCK_TELEMETRY") {
        config.enabled = parse_bool_value(&value, "RUSTOS_LOCK_TELEMETRY")
            .unwrap_or_else(|err| panic!("invalid RUSTOS_LOCK_TELEMETRY: {err}"));
    }
    if let Ok(value) = std::env::var("RUSTOS_LOCK_TELEMETRY_WARN_WAIT_CYCLES") {
        config.warn_wait_cycles = parse_u64_value(&value, "RUSTOS_LOCK_TELEMETRY_WARN_WAIT_CYCLES")
            .unwrap_or_else(|err| {
                panic!("invalid RUSTOS_LOCK_TELEMETRY_WARN_WAIT_CYCLES: {err}")
            });
    }
    if let Ok(value) = std::env::var("RUSTOS_LOCK_TELEMETRY_WARN_HOLD_CYCLES") {
        config.warn_hold_cycles = parse_u64_value(&value, "RUSTOS_LOCK_TELEMETRY_WARN_HOLD_CYCLES")
            .unwrap_or_else(|err| {
                panic!("invalid RUSTOS_LOCK_TELEMETRY_WARN_HOLD_CYCLES: {err}")
            });
    }
    config
}

pub fn try_parse_lock_telemetry_toml(source: &str) -> Result<LockTelemetryConfig, String> {
    let mut config = LockTelemetryConfig::default();
    let mut section = "";

    for raw_line in source.lines() {
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }

        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            section = name.trim();
            continue;
        }

        if section != "lock_telemetry" {
            continue;
        }

        let Some((raw_key, raw_value)) = line.split_once('=') else {
            return Err(format!("invalid lock_telemetry config line: {line}"));
        };
        let key = raw_key.trim();
        let value = raw_value.trim();
        match key {
            "enabled" => config.enabled = parse_bool_value(value, key)?,
            "warn_wait_cycles" => config.warn_wait_cycles = parse_u64_value(value, key)?,
            "warn_hold_cycles" => config.warn_hold_cycles = parse_u64_value(value, key)?,
            other => return Err(format!("unknown lock_telemetry config key: {other}")),
        }
    }

    Ok(config)
}

// GENERATED: Retained for generated-source conformance tests.
#[allow(dead_code)]
pub fn parse_bool(source: &str, name: &str) -> bool {
    let prefix = format!("pub const {name}: bool =");
    for line in source.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix(&prefix) {
            return match value.trim().trim_end_matches(';') {
                "true" => true,
                "false" => false,
                other => panic!("invalid bool for {name}: {other}"),
            };
        }
    }

    panic!("missing bool constant: {name}");
}

fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    for (index, ch) in line.char_indices() {
        match ch {
            '"' => in_string = !in_string,
            '#' if !in_string => return &line[..index],
            _ => {}
        }
    }
    line
}

fn parse_bool_value(value: &str, key: &str) -> Result<bool, String> {
    match trim_string(value) {
        "1" | "true" | "True" | "TRUE" | "yes" | "on" => Ok(true),
        "0" | "false" | "False" | "FALSE" | "no" | "off" => Ok(false),
        other => Err(format!("invalid bool for {key}: {other}")),
    }
}

fn parse_usize_value(value: &str, key: &str) -> Result<usize, String> {
    trim_string(value)
        .parse::<usize>()
        .map_err(|_| format!("invalid usize for {key}: {value}"))
}

fn parse_u64_value(value: &str, key: &str) -> Result<u64, String> {
    trim_string(value)
        .parse::<u64>()
        .map_err(|_| format!("invalid u64 for {key}: {value}"))
}

fn parse_level_value(value: &str) -> Result<u8, String> {
    let level = trim_string(value);
    level_value(level).ok_or_else(|| format!("invalid log level: {level}"))
}

fn trim_string(value: &str) -> &str {
    value.trim().trim_matches('"')
}

fn category_index(category: &str) -> Option<usize> {
    LOG_CATEGORIES
        .iter()
        .position(|candidate| *candidate == category)
}

fn level_value(name: &str) -> Option<u8> {
    match name {
        "off" => Some(LOG_LEVEL_OFF),
        "trace" => Some(LOG_LEVEL_TRACE),
        "debug" => Some(LOG_LEVEL_DEBUG),
        "info" => Some(LOG_LEVEL_INFO),
        "warn" => Some(LOG_LEVEL_WARN),
        "error" => Some(LOG_LEVEL_ERROR),
        "fatal" => Some(LOG_LEVEL_FATAL),
        _ => None,
    }
}

fn level_name(level: u8) -> &'static str {
    match level {
        LOG_LEVEL_OFF => "off",
        LOG_LEVEL_TRACE => "trace",
        LOG_LEVEL_DEBUG => "debug",
        LOG_LEVEL_INFO => "info",
        LOG_LEVEL_WARN => "warn",
        LOG_LEVEL_ERROR => "error",
        LOG_LEVEL_FATAL => "fatal",
        _ => "unknown",
    }
}

// GENERATED: Helper is live only in include sites that emit category macros.
#[allow(dead_code)]
fn category_variant(category: &str) -> &'static str {
    match category {
        "boot" => "Boot",
        "panic" => "Panic",
        "memory" => "Memory",
        "sched" => "Sched",
        "syscall" => "Syscall",
        "process" => "Process",
        "driver" => "Driver",
        "storage" => "Storage",
        "usb" => "Usb",
        "input" => "Input",
        "display" => "Display",
        "vfs" => "Vfs",
        "console" => "Console",
        "service" => "Service",
        "compat" => "Compat",
        "debug" => "Debug",
        "heartbeat" => "Heartbeat",
        other => panic!("unknown log category: {other}"),
    }
}

// GENERATED: Helper is live only in include sites that emit level macros.
#[allow(dead_code)]
fn level_variant(level: &str) -> &'static str {
    match level {
        "trace" => "Trace",
        "debug" => "Debug",
        "info" => "Info",
        "warn" => "Warn",
        "error" => "Error",
        "fatal" => "Fatal",
        other => panic!("unknown log level: {other}"),
    }
}

// GENERATED: Not every shared-source consumer emits runtime level checks.
#[allow(dead_code)]
fn generate_compiled_level_enabled_fn(category_ty: &str, level_ty: &str) -> String {
    let mut generated = format!(
        "fn compiled_level_enabled(category: {category_ty}, level: {level_ty}) -> bool {{\n    match category {{\n"
    );
    for category in LOG_CATEGORIES {
        generated.push_str(&format!(
            "        {category_ty}::{variant} => match level {{\n",
            variant = category_variant(category)
        ));
        for (level_name, _) in LOG_LEVELS {
            generated.push_str(&format!(
                "            {level_ty}::{variant} => cfg!(rustos_log_{category}_{level_name}),\n",
                variant = level_variant(level_name)
            ));
        }
        generated.push_str("        },\n");
    }
    generated.push_str("    }\n}\n");
    generated
}

// GENERATED: Not every shared-source consumer emits category macros.
#[allow(dead_code)]
fn generate_category_macro(macro_name: &str, category_ty_prefix: &str) -> String {
    let mut generated = format!("#[doc(hidden)]\n#[macro_export]\nmacro_rules! {macro_name} {{\n");
    for category in LOG_CATEGORIES {
        generated.push_str(&format!(
            "    ({category}) => {{ {category_ty_prefix}::{variant} }};\n",
            variant = category_variant(category)
        ));
    }
    generated.push_str("}\n");
    generated
}

// GENERATED: Not every shared-source consumer emits conditional log macros.
#[allow(dead_code)]
fn generate_if_enabled_macro(macro_name: &str) -> String {
    let mut generated = format!("#[doc(hidden)]\n#[macro_export]\nmacro_rules! {macro_name} {{\n");
    for category in LOG_CATEGORIES {
        for (level_name, _) in LOG_LEVELS {
            generated.push_str(&format!(
                "    ({category}, {level_name}, $body:block) => {{ #[cfg(rustos_log_{category}_{level_name})] {{ $body }} #[cfg(not(rustos_log_{category}_{level_name}))] {{}} }};\n"
            ));
        }
    }
    generated.push_str("}\n");
    generated
}

fn bool_name(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

#[cfg(test)]
mod tests {
    use super::{
        LOG_LEVEL_DEBUG, LOG_LEVEL_FATAL, LOG_LEVEL_INFO, LOG_LEVEL_OFF, parse_logging_toml,
    };

    #[test]
    fn parses_root_and_category_levels() {
        let source = r#"
enabled = true
boot_trace_enabled = false
serial_mirror = false
ring_buffer_bytes = 32768
min_level = "warn"

[categories]
boot = "info"
panic = "fatal"
syscall = "off"
storage = "debug"
"#;

        let config = parse_logging_toml(source);
        assert!(config.enabled);
        assert!(!config.boot_trace_enabled);
        assert!(!config.serial_mirror);
        assert_eq!(config.ring_buffer_bytes, 32768);
        assert_eq!(config.level_for_category("boot"), LOG_LEVEL_INFO);
        assert_eq!(config.level_for_category("panic"), LOG_LEVEL_FATAL);
        assert_eq!(config.level_for_category("syscall"), LOG_LEVEL_OFF);
        assert_eq!(config.level_for_category("storage"), LOG_LEVEL_DEBUG);
    }

    #[test]
    fn ignores_multiline_non_logging_sections() {
        let source = r#"
[logging]
min_level = "warn"

[fault_injection]
rules = [
    "alloc.frame=off",
    "block.read=off",
]
"#;

        let config = parse_logging_toml(source);
        assert_eq!(config.min_level, super::LOG_LEVEL_WARN);
        assert_eq!(config.level_for_category("boot"), super::LOG_LEVEL_WARN);
    }
}
