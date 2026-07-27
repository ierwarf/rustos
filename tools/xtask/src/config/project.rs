use anyhow::{anyhow, bail};
use fs_err as fs;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::Result;
use crate::util::{env_path, env_string, path_label, split_whitespace_owned};

#[allow(dead_code)]
mod build_log_cfg {
    include!("../../../build_log_cfg.rs");
}

const DEFAULT_KERNEL_CODEGEN_UNITS: u16 = 1;
const DEFAULT_KERNEL_OPT_LEVEL: &str = "2";
const DEFAULT_KERNEL_OVERFLOW_CHECKS: bool = true;
const DEFAULT_KERNEL_DEBUG_ASSERTIONS: bool = false;
const DEFAULT_KERNEL_LTO: &str = "off";
const DEFAULT_KERNEL_FORCE_FRAME_POINTERS: bool = false;
const DEFAULT_KERNEL_INCREMENTAL: bool = false;
const DEFAULT_KERNEL_DEBUGINFO: &str = "0";
const DEFAULT_KERNEL_EMBED_BITCODE: bool = false;
const DEFAULT_KERNEL_PANIC: &str = "abort";
const DEFAULT_KERNEL_RELOCATION_MODEL: &str = "none";
const DEFAULT_KERNEL_STRIP: &str = "none";

const PROJECT_CONFIG_ENV: &str = "RUSTOS_CONFIG";

#[derive(Clone, Debug)]
pub(crate) struct ProjectConfig {
    pub(crate) source: ProjectConfigSource,
    pub(crate) kernel: KernelConfig,
    pub(crate) fault_injection: FaultInjectionConfig,
    pub(crate) fuzzing: FuzzingConfig,
    pub(crate) lock_telemetry: LockTelemetryConfig,
}

#[derive(Clone, Debug)]
pub(crate) enum ProjectConfigSource {
    BuiltInDefaults,
    Canonical(PathBuf),
    Override(PathBuf),
}

impl ProjectConfigSource {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::BuiltInDefaults => String::from("internal defaults"),
            Self::Canonical(path) => format!("canonical {}", path_label(path)),
            Self::Override(path) => format!("override {}", path_label(path)),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct KernelConfig {
    pub(crate) build: KernelBuildConfig,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FaultInjectionConfig {
    pub(crate) enabled: bool,
    pub(crate) rules: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FuzzingConfig {
    pub(crate) enabled: bool,
    pub(crate) fd_transfer_stress: bool,
    pub(crate) startup_delay_ms: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct LockTelemetryConfig {
    pub(crate) enabled: bool,
    pub(crate) warn_wait_cycles: u64,
    pub(crate) warn_hold_cycles: u64,
}

impl Default for LockTelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            warn_wait_cycles: 250_000,
            warn_hold_cycles: 250_000,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct KernelBuildConfig {
    pub(crate) codegen_units: u16,
    pub(crate) opt_level: String,
    pub(crate) overflow_checks: bool,
    pub(crate) debug_assertions: bool,
    pub(crate) lto: String,
    pub(crate) force_frame_pointers: bool,
    pub(crate) incremental: bool,
    pub(crate) debuginfo: String,
    pub(crate) embed_bitcode: bool,
    pub(crate) panic: String,
    pub(crate) relocation_model: String,
    pub(crate) strip: String,
    pub(crate) extra_rustflags: Vec<String>,
}

impl Default for KernelBuildConfig {
    fn default() -> Self {
        Self {
            codegen_units: DEFAULT_KERNEL_CODEGEN_UNITS,
            opt_level: String::from(DEFAULT_KERNEL_OPT_LEVEL),
            overflow_checks: DEFAULT_KERNEL_OVERFLOW_CHECKS,
            debug_assertions: DEFAULT_KERNEL_DEBUG_ASSERTIONS,
            lto: String::from(DEFAULT_KERNEL_LTO),
            force_frame_pointers: DEFAULT_KERNEL_FORCE_FRAME_POINTERS,
            incremental: DEFAULT_KERNEL_INCREMENTAL,
            debuginfo: String::from(DEFAULT_KERNEL_DEBUGINFO),
            embed_bitcode: DEFAULT_KERNEL_EMBED_BITCODE,
            panic: String::from(DEFAULT_KERNEL_PANIC),
            relocation_model: String::from(DEFAULT_KERNEL_RELOCATION_MODEL),
            strip: String::from(DEFAULT_KERNEL_STRIP),
            extra_rustflags: Vec::new(),
        }
    }
}

impl KernelBuildConfig {
    pub(crate) fn rustflags(&self, inherited: &str) -> String {
        let mut flags = String::new();
        if !inherited.is_empty() {
            flags.push_str(inherited);
            flags.push(' ');
        }
        flags.push_str("--cfg rustos_boot_image ");
        flags.push_str("-C no-redzone ");
        push_codegen_flag(&mut flags, "codegen-units", self.codegen_units);
        push_codegen_flag(&mut flags, "opt-level", &self.opt_level);
        push_codegen_flag(
            &mut flags,
            "overflow-checks",
            bool_name(self.overflow_checks),
        );
        push_codegen_flag(
            &mut flags,
            "debug-assertions",
            bool_name(self.debug_assertions),
        );
        if self.lto != "off" {
            push_codegen_flag(&mut flags, "lto", &self.lto);
        }
        if self.force_frame_pointers {
            push_codegen_flag(&mut flags, "force-frame-pointers", "yes");
        }
        push_codegen_flag(&mut flags, "debuginfo", &self.debuginfo);
        if self.embed_bitcode {
            push_codegen_flag(&mut flags, "embed-bitcode", "yes");
        }
        push_codegen_flag(&mut flags, "panic", &self.panic);
        if self.relocation_model != "none" {
            push_codegen_flag(&mut flags, "relocation-model", &self.relocation_model);
        }
        if self.strip != "none" {
            push_codegen_flag(&mut flags, "strip", &self.strip);
        }
        for flag in &self.extra_rustflags {
            flags.push_str(flag);
            flags.push(' ');
        }
        flags.trim_end().to_owned()
    }
}

fn push_codegen_flag(flags: &mut String, key: &str, value: impl std::fmt::Display) {
    let _ = write!(flags, "-C {key}={value} ");
}

fn bool_name(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProjectConfigFile {
    logging: Option<toml::Value>,
    kernel: KernelConfigFile,
    fault_injection: FaultInjectionConfigFile,
    fuzzing: FuzzingConfigFile,
    lock_telemetry: LockTelemetryConfigFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct KernelConfigFile {
    build: KernelBuildConfigFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct KernelBuildConfigFile {
    codegen_units: Option<u16>,
    opt_level: Option<String>,
    overflow_checks: Option<bool>,
    debug_assertions: Option<bool>,
    lto: Option<String>,
    force_frame_pointers: Option<bool>,
    incremental: Option<bool>,
    debuginfo: Option<String>,
    embed_bitcode: Option<bool>,
    panic: Option<String>,
    relocation_model: Option<String>,
    strip: Option<String>,
    extra_rustflags: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FaultInjectionConfigFile {
    enabled: Option<bool>,
    rules: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FuzzingConfigFile {
    enabled: Option<bool>,
    fd_transfer_stress: Option<bool>,
    startup_delay_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LockTelemetryConfigFile {
    enabled: Option<bool>,
    warn_wait_cycles: Option<u64>,
    warn_hold_cycles: Option<u64>,
}

pub(crate) fn load_project_config(root_dir: &Path) -> Result<ProjectConfig> {
    let (mut config, source) = load_project_config_file(root_dir)?;
    apply_env_overrides(&mut config.kernel.build)?;
    apply_fault_env_overrides(&mut config.fault_injection)?;
    apply_fuzzing_env_overrides(&mut config.fuzzing)?;
    apply_lock_telemetry_env_overrides(&mut config.lock_telemetry)?;
    validate_kernel_build(&config.kernel.build)?;
    validate_fault_injection(&config.fault_injection)?;
    validate_fuzzing(&config.fuzzing)?;
    validate_lock_telemetry(&config.lock_telemetry)?;
    Ok(ProjectConfig {
        source,
        kernel: config.kernel,
        fault_injection: config.fault_injection,
        fuzzing: config.fuzzing,
        lock_telemetry: config.lock_telemetry,
    })
}

fn load_project_config_file(root_dir: &Path) -> Result<(ProjectConfig, ProjectConfigSource)> {
    let (canonical_path, overridden) = if let Some(path) = env_path(PROJECT_CONFIG_ENV) {
        if !path.is_file() {
            bail!("{PROJECT_CONFIG_ENV} is not a file: {}", path.display());
        }
        (path, true)
    } else {
        (root_dir.join("config/rustos.toml"), false)
    };
    if canonical_path.is_file() {
        let parsed = parse_project_config(&canonical_path)?;
        let source = if overridden {
            ProjectConfigSource::Override(canonical_path)
        } else {
            ProjectConfigSource::Canonical(canonical_path)
        };
        return Ok((parsed, source));
    }

    bail!(
        "missing canonical RustOS config: {}",
        canonical_path.display()
    )
}

fn parse_project_config(path: &Path) -> Result<ProjectConfig> {
    let text = fs::read_to_string(path)?;
    validate_project_config_text(&text)
        .map_err(|err| anyhow!("invalid RustOS config {}: {err}", path.display()))?;
    let parsed = toml::from_str::<ProjectConfigFile>(&text)
        .map_err(|err| anyhow!("invalid RustOS config {}: {err}", path.display()))?;
    validate_logging_config(&text, path)?;
    Ok(project_from_file(parsed))
}

pub(crate) fn validate_project_config_text(text: &str) -> Result<()> {
    let parsed = toml::from_str::<ProjectConfigFile>(text)
        .map_err(|err| anyhow!("invalid RustOS config: {err}"))?;
    let config = project_from_file(parsed);
    validate_kernel_build(&config.kernel.build)?;
    validate_fault_injection(&config.fault_injection)?;
    validate_fuzzing(&config.fuzzing)?;
    validate_lock_telemetry(&config.lock_telemetry)?;
    Ok(())
}

fn validate_logging_config(text: &str, path: &Path) -> Result<()> {
    build_log_cfg::try_parse_logging_toml(text)
        .map(|_| ())
        .map_err(|err| anyhow!("invalid logging config {}: {err}", path.display()))
}

fn project_from_file(file: ProjectConfigFile) -> ProjectConfig {
    let mut config = ProjectConfig::default();
    let _ = file.logging;
    let build = file.kernel.build;
    if let Some(value) = build.codegen_units {
        config.kernel.build.codegen_units = value;
    }
    if let Some(value) = build.opt_level {
        config.kernel.build.opt_level = value;
    }
    if let Some(value) = build.overflow_checks {
        config.kernel.build.overflow_checks = value;
    }
    if let Some(value) = build.debug_assertions {
        config.kernel.build.debug_assertions = value;
    }
    if let Some(value) = build.lto {
        config.kernel.build.lto = value;
    }
    if let Some(value) = build.force_frame_pointers {
        config.kernel.build.force_frame_pointers = value;
    }
    if let Some(value) = build.incremental {
        config.kernel.build.incremental = value;
    }
    if let Some(value) = build.debuginfo {
        config.kernel.build.debuginfo = value;
    }
    if let Some(value) = build.embed_bitcode {
        config.kernel.build.embed_bitcode = value;
    }
    if let Some(value) = build.panic {
        config.kernel.build.panic = value;
    }
    if let Some(value) = build.relocation_model {
        config.kernel.build.relocation_model = value;
    }
    if let Some(value) = build.strip {
        config.kernel.build.strip = value;
    }
    if let Some(value) = build.extra_rustflags {
        config.kernel.build.extra_rustflags = value;
    }
    let fault = file.fault_injection;
    if let Some(value) = fault.enabled {
        config.fault_injection.enabled = value;
    }
    if let Some(value) = fault.rules {
        config.fault_injection.rules = value;
    }
    let fuzzing = file.fuzzing;
    if let Some(value) = fuzzing.enabled {
        config.fuzzing.enabled = value;
    }
    if let Some(value) = fuzzing.fd_transfer_stress {
        config.fuzzing.fd_transfer_stress = value;
    }
    if let Some(value) = fuzzing.startup_delay_ms {
        config.fuzzing.startup_delay_ms = value;
    }
    let lock_telemetry = file.lock_telemetry;
    if let Some(value) = lock_telemetry.enabled {
        config.lock_telemetry.enabled = value;
    }
    if let Some(value) = lock_telemetry.warn_wait_cycles {
        config.lock_telemetry.warn_wait_cycles = value;
    }
    if let Some(value) = lock_telemetry.warn_hold_cycles {
        config.lock_telemetry.warn_hold_cycles = value;
    }
    config
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            source: ProjectConfigSource::BuiltInDefaults,
            kernel: KernelConfig::default(),
            fault_injection: FaultInjectionConfig::default(),
            fuzzing: FuzzingConfig::default(),
            lock_telemetry: LockTelemetryConfig::default(),
        }
    }
}

fn apply_env_overrides(build: &mut KernelBuildConfig) -> Result<()> {
    if let Some(value) =
        env_string("RUSTOS_KERNEL_CODEGEN_UNITS").or_else(|| env_string("KERNEL_CODEGEN_UNITS"))
    {
        build.codegen_units = parse_u16_env("RUSTOS_KERNEL_CODEGEN_UNITS", &value)?;
    }
    if let Some(value) = env_string("RUSTOS_KERNEL_OPT_LEVEL") {
        build.opt_level = value;
    }
    if let Some(value) = env_string("RUSTOS_KERNEL_OVERFLOW_CHECKS") {
        build.overflow_checks = parse_bool_env("RUSTOS_KERNEL_OVERFLOW_CHECKS", &value)?;
    }
    if let Some(value) = env_string("RUSTOS_KERNEL_DEBUG_ASSERTIONS") {
        build.debug_assertions = parse_bool_env("RUSTOS_KERNEL_DEBUG_ASSERTIONS", &value)?;
    }
    if let Some(value) = env_string("RUSTOS_KERNEL_LTO") {
        build.lto = value;
    }
    if let Some(value) = env_string("RUSTOS_KERNEL_FORCE_FRAME_POINTERS") {
        build.force_frame_pointers = parse_bool_env("RUSTOS_KERNEL_FORCE_FRAME_POINTERS", &value)?;
    }
    if let Some(value) = env_string("RUSTOS_KERNEL_INCREMENTAL") {
        build.incremental = parse_bool_env("RUSTOS_KERNEL_INCREMENTAL", &value)?;
    }
    if let Some(value) = env_string("RUSTOS_KERNEL_DEBUGINFO") {
        build.debuginfo = value;
    }
    if let Some(value) = env_string("RUSTOS_KERNEL_EMBED_BITCODE") {
        build.embed_bitcode = parse_bool_env("RUSTOS_KERNEL_EMBED_BITCODE", &value)?;
    }
    if let Some(value) = env_string("RUSTOS_KERNEL_PANIC") {
        build.panic = value;
    }
    if let Some(value) = env_string("RUSTOS_KERNEL_RELOCATION_MODEL") {
        build.relocation_model = value;
    }
    if let Some(value) = env_string("RUSTOS_KERNEL_STRIP") {
        build.strip = value;
    }
    if let Some(value) = env_string("RUSTOS_KERNEL_EXTRA_RUSTFLAGS") {
        build.extra_rustflags = split_whitespace_owned(&value);
    }
    Ok(())
}

fn apply_fault_env_overrides(fault: &mut FaultInjectionConfig) -> Result<()> {
    if let Some(value) = env_string("RUSTOS_FAULT_INJECTION") {
        fault.enabled = parse_bool_env("RUSTOS_FAULT_INJECTION", &value)?;
    }
    if let Some(value) = env_string("RUSTOS_FAULTS") {
        fault.enabled = true;
        apply_fault_rule_overrides(fault, &value)?;
    }
    Ok(())
}

fn apply_fault_rule_overrides(fault: &mut FaultInjectionConfig, value: &str) -> Result<()> {
    let mut override_locations = Vec::new();
    for rule in value
        .split(';')
        .map(str::trim)
        .filter(|rule| !rule.is_empty())
    {
        let parsed = rustos_fault_injection::parse_rule(rule)
            .map_err(|err| anyhow!("invalid RUSTOS_FAULTS entry {rule:?}: {err}"))?;
        if override_locations.contains(&parsed.location) {
            bail!(
                "RUSTOS_FAULTS repeats fault point {} in one override",
                parsed.location
            );
        }
        override_locations.push(parsed.location.clone());
        fault.rules.retain(|current| {
            rustos_fault_injection::parse_rule(current)
                .map(|registered| registered.location != parsed.location)
                .unwrap_or(true)
        });
        fault.rules.push(rule.to_owned());
    }
    Ok(())
}

fn apply_fuzzing_env_overrides(fuzzing: &mut FuzzingConfig) -> Result<()> {
    if let Some(value) = env_string("RUSTOS_FUZZING") {
        fuzzing.enabled = parse_bool_env("RUSTOS_FUZZING", &value)?;
    }
    if let Some(value) = env_string("RUSTOS_FUZZ_FD_TRANSFER_STRESS") {
        fuzzing.fd_transfer_stress = parse_bool_env("RUSTOS_FUZZ_FD_TRANSFER_STRESS", &value)?;
    }
    if let Some(value) = env_string("RUSTOS_FUZZ_STARTUP_DELAY_MS") {
        fuzzing.startup_delay_ms = parse_u64_env("RUSTOS_FUZZ_STARTUP_DELAY_MS", &value)?;
    }
    Ok(())
}

fn apply_lock_telemetry_env_overrides(lock_telemetry: &mut LockTelemetryConfig) -> Result<()> {
    if let Some(value) = env_string("RUSTOS_LOCK_TELEMETRY") {
        lock_telemetry.enabled = parse_bool_env("RUSTOS_LOCK_TELEMETRY", &value)?;
    }
    if let Some(value) = env_string("RUSTOS_LOCK_TELEMETRY_WARN_WAIT_CYCLES") {
        lock_telemetry.warn_wait_cycles =
            parse_u64_env("RUSTOS_LOCK_TELEMETRY_WARN_WAIT_CYCLES", &value)?;
    }
    if let Some(value) = env_string("RUSTOS_LOCK_TELEMETRY_WARN_HOLD_CYCLES") {
        lock_telemetry.warn_hold_cycles =
            parse_u64_env("RUSTOS_LOCK_TELEMETRY_WARN_HOLD_CYCLES", &value)?;
    }
    Ok(())
}

fn parse_u16_env(name: &str, value: &str) -> Result<u16> {
    value
        .parse::<u16>()
        .map_err(|err| anyhow!("invalid {name}={value:?}: {err}"))
}

fn parse_u64_env(name: &str, value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .map_err(|err| anyhow!("invalid {name}={value:?}: {err}"))
}

fn parse_bool_env(name: &str, value: &str) -> Result<bool> {
    match value {
        "1" | "true" | "True" | "TRUE" | "yes" | "on" => Ok(true),
        "0" | "false" | "False" | "FALSE" | "no" | "off" => Ok(false),
        _ => Err(anyhow!("invalid {name}={value:?}: expected boolean")),
    }
}

fn validate_kernel_build(build: &KernelBuildConfig) -> Result<()> {
    if !(1..=256).contains(&build.codegen_units) {
        bail!(
            "kernel.build.codegen_units must be in 1..=256, got {}",
            build.codegen_units
        );
    }
    match build.opt_level.as_str() {
        "0" | "1" | "2" | "3" | "s" | "z" => {}
        other => {
            bail!("kernel.build.opt_level must be 0, 1, 2, 3, s, or z, got {other:?}");
        }
    }
    match build.lto.as_str() {
        "off" | "thin" | "fat" => {}
        other => {
            bail!("kernel.build.lto must be off, thin, or fat, got {other:?}");
        }
    }
    if build.lto != "off" && !build.embed_bitcode {
        bail!("kernel.build.embed_bitcode must be true when kernel.build.lto is not off");
    }
    match build.debuginfo.as_str() {
        "0" | "1" | "2" | "line-directives-only" | "line-tables-only" => {}
        other => {
            bail!(
                "kernel.build.debuginfo must be 0, 1, 2, line-directives-only, or line-tables-only, got {other:?}"
            );
        }
    }
    match build.panic.as_str() {
        "abort" | "unwind" => {}
        other => {
            bail!("kernel.build.panic must be abort or unwind, got {other:?}");
        }
    }
    match build.relocation_model.as_str() {
        "none" | "static" | "pic" | "pie" | "dynamic-no-pic" | "ropi" | "rwpi" | "ropi-rwpi" => {}
        other => {
            bail!(
                "kernel.build.relocation_model must be none, static, pic, pie, dynamic-no-pic, ropi, rwpi, or ropi-rwpi, got {other:?}"
            );
        }
    }
    match build.strip.as_str() {
        "none" | "debuginfo" | "symbols" => {}
        other => {
            bail!("kernel.build.strip must be none, debuginfo, or symbols, got {other:?}");
        }
    }
    Ok(())
}

fn validate_fault_injection(fault: &FaultInjectionConfig) -> Result<()> {
    let mut locations = Vec::new();
    for rule in &fault.rules {
        let parsed = rustos_fault_injection::parse_rule(rule)
            .map_err(|err| anyhow!("invalid fault_injection.rules entry {rule:?}: {err}"))?;
        if !rustos_fault_injection::is_registered_fault_point(parsed.location.as_str()) {
            bail!(
                "fault_injection.rules entry names an unimplemented or retired point: {}",
                parsed.location
            );
        }
        if locations.contains(&parsed.location) {
            bail!(
                "fault_injection.rules repeats point {}; one rule must own each boundary",
                parsed.location
            );
        }
        locations.push(parsed.location);
    }
    Ok(())
}

fn validate_fuzzing(fuzzing: &FuzzingConfig) -> Result<()> {
    if fuzzing.startup_delay_ms > 60_000 {
        bail!(
            "fuzzing.startup_delay_ms must be <= 60000, got {}",
            fuzzing.startup_delay_ms
        );
    }
    Ok(())
}

fn validate_lock_telemetry(lock_telemetry: &LockTelemetryConfig) -> Result<()> {
    if lock_telemetry.enabled
        && (lock_telemetry.warn_wait_cycles == 0 || lock_telemetry.warn_hold_cycles == 0)
    {
        bail!("lock_telemetry thresholds must be non-zero when enabled");
    }
    Ok(())
}

pub(crate) fn effective_config_toml(config: &ProjectConfig) -> String {
    let build = &config.kernel.build;
    let fault = &config.fault_injection;
    let fuzzing = &config.fuzzing;
    let lock_telemetry = &config.lock_telemetry;
    let extra = build
        .extra_rustflags
        .iter()
        .map(|flag| format!("{flag:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let fault_rules = fault
        .rules
        .iter()
        .map(|rule| format!("{rule:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "# source: {}\n[kernel.build]\ncodegen_units = {}\nopt_level = {:?}\noverflow_checks = {}\ndebug_assertions = {}\nlto = {:?}\nforce_frame_pointers = {}\nincremental = {}\ndebuginfo = {:?}\nembed_bitcode = {}\npanic = {:?}\nrelocation_model = {:?}\nstrip = {:?}\nextra_rustflags = [{}]\n\n[fault_injection]\nenabled = {}\nrules = [{}]\n\n[fuzzing]\nenabled = {}\nfd_transfer_stress = {}\nstartup_delay_ms = {}\n\n[lock_telemetry]\nenabled = {}\nwarn_wait_cycles = {}\nwarn_hold_cycles = {}\n",
        config.source.label(),
        build.codegen_units,
        build.opt_level,
        build.overflow_checks,
        build.debug_assertions,
        build.lto,
        build.force_frame_pointers,
        build.incremental,
        build.debuginfo,
        build.embed_bitcode,
        build.panic,
        build.relocation_model,
        build.strip,
        extra,
        fault.enabled,
        fault_rules,
        fuzzing.enabled,
        fuzzing.fd_transfer_stress,
        fuzzing.startup_delay_ms,
        lock_telemetry.enabled,
        lock_telemetry.warn_wait_cycles,
        lock_telemetry.warn_hold_cycles,
    )
}

#[cfg(test)]
mod tests {
    use super::{FaultInjectionConfig, apply_fault_rule_overrides, validate_fault_injection};

    #[test]
    fn fault_override_replaces_the_default_instead_of_being_shadowed() {
        let mut fault = FaultInjectionConfig {
            enabled: false,
            rules: vec![
                "block.flush=off".to_owned(),
                "display.present=off".to_owned(),
            ],
        };
        apply_fault_rule_overrides(&mut fault, "block.flush=fail").unwrap();
        assert_eq!(
            fault.rules,
            vec![
                "display.present=off".to_owned(),
                "block.flush=fail".to_owned()
            ]
        );
        validate_fault_injection(&fault).unwrap();
    }

    #[test]
    fn fault_config_rejects_phantom_and_duplicate_points() {
        let phantom = FaultInjectionConfig {
            enabled: true,
            rules: vec!["socket.send=fail".to_owned()],
        };
        assert!(validate_fault_injection(&phantom).is_err());

        let duplicate = FaultInjectionConfig {
            enabled: true,
            rules: vec!["block.read=off".to_owned(), "block.read=fail".to_owned()],
        };
        assert!(validate_fault_injection(&duplicate).is_err());
    }
}
