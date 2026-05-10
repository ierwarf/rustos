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
const KERNEL_BUILD_CONFIG_ENV: &str = "KERNEL_BUILD_CONFIG";

#[derive(Clone, Debug)]
pub(crate) struct ProjectConfig {
    pub(crate) source: ProjectConfigSource,
    pub(crate) kernel: KernelConfig,
    pub(crate) fault_injection: FaultInjectionConfig,
}

#[derive(Clone, Debug)]
pub(crate) enum ProjectConfigSource {
    BuiltInDefaults,
    Canonical(PathBuf),
    Override(PathBuf),
    LegacyKernelBuild(PathBuf),
}

impl ProjectConfigSource {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::BuiltInDefaults => String::from("built-in defaults"),
            Self::Canonical(path) => format!("canonical {}", path_label(path)),
            Self::Override(path) => format!("override {}", path_label(path)),
            Self::LegacyKernelBuild(path) => format!("legacy {}", path_label(path)),
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
struct LegacyKernelBuildConfigFile {
    hardening: LegacyKernelHardeningConfigFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LegacyKernelHardeningConfigFile {
    codegen_units: Option<u16>,
}

pub(crate) fn load_project_config(root_dir: &Path) -> Result<ProjectConfig> {
    let (mut config, source) = load_project_config_file(root_dir)?;
    apply_env_overrides(&mut config.kernel.build)?;
    apply_fault_env_overrides(&mut config.fault_injection)?;
    validate_kernel_build(&config.kernel.build)?;
    validate_fault_injection(&config.fault_injection)?;
    Ok(ProjectConfig {
        source,
        kernel: config.kernel,
        fault_injection: config.fault_injection,
    })
}

fn load_project_config_file(root_dir: &Path) -> Result<(ProjectConfig, ProjectConfigSource)> {
    if let Some(path) = env_path(KERNEL_BUILD_CONFIG_ENV) {
        return load_override_config(path);
    }

    let canonical_path = if let Some(path) = env_path(PROJECT_CONFIG_ENV) {
        if !path.is_file() {
            bail!("{PROJECT_CONFIG_ENV} is not a file: {}", path.display());
        }
        path
    } else {
        root_dir.join("config/rustos.toml")
    };
    if canonical_path.is_file() {
        let parsed = parse_project_config(&canonical_path)?;
        return Ok((parsed, ProjectConfigSource::Canonical(canonical_path)));
    }

    let legacy_kernel_path = root_dir.join("config/kernel-build.toml");
    if legacy_kernel_path.is_file() {
        let parsed = parse_legacy_kernel_build_config(&legacy_kernel_path)?;
        return Ok((
            parsed,
            ProjectConfigSource::LegacyKernelBuild(legacy_kernel_path),
        ));
    }

    Ok((
        ProjectConfig::default(),
        ProjectConfigSource::BuiltInDefaults,
    ))
}

fn load_override_config(path: PathBuf) -> Result<(ProjectConfig, ProjectConfigSource)> {
    let text = fs::read_to_string(&path)?;
    match toml::from_str::<ProjectConfigFile>(&text) {
        Ok(parsed) => {
            validate_logging_config(&text, &path)?;
            Ok((
                project_from_file(parsed),
                ProjectConfigSource::Override(path),
            ))
        }
        Err(project_err) => match toml::from_str::<LegacyKernelBuildConfigFile>(&text) {
            Ok(_) => {
                let parsed = parse_legacy_kernel_build_config(&path)?;
                Ok((parsed, ProjectConfigSource::Override(path)))
            }
            Err(_) => Err(anyhow!(
                "invalid RustOS config {}: {project_err}",
                path.display()
            )),
        },
    }
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
    let parsed = toml::from_str::<ProjectConfigFile>(&text)
        .map_err(|err| anyhow!("invalid RustOS config: {err}"))?;
    let config = project_from_file(parsed);
    validate_kernel_build(&config.kernel.build)?;
    validate_fault_injection(&config.fault_injection)?;
    Ok(())
}

fn validate_logging_config(text: &str, path: &Path) -> Result<()> {
    build_log_cfg::try_parse_logging_toml(text)
        .map(|_| ())
        .map_err(|err| anyhow!("invalid logging config {}: {err}", path.display()))
}

fn parse_legacy_kernel_build_config(path: &Path) -> Result<ProjectConfig> {
    let text = fs::read_to_string(path)?;
    let parsed = toml::from_str::<LegacyKernelBuildConfigFile>(&text).map_err(|err| {
        anyhow!(
            "invalid legacy kernel build config {}: {err}",
            path.display()
        )
    })?;
    let mut config = ProjectConfig::default();
    if let Some(value) = parsed.hardening.codegen_units {
        config.kernel.build.codegen_units = value;
    }
    Ok(config)
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
    config
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            source: ProjectConfigSource::BuiltInDefaults,
            kernel: KernelConfig::default(),
            fault_injection: FaultInjectionConfig::default(),
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
        fault.rules.extend(
            value
                .split(';')
                .map(str::trim)
                .filter(|rule| !rule.is_empty())
                .map(str::to_owned),
        );
    }
    Ok(())
}

fn parse_u16_env(name: &str, value: &str) -> Result<u16> {
    value
        .parse::<u16>()
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
    for rule in &fault.rules {
        rustos_fault_injection::parse_rule(rule)
            .map_err(|err| anyhow!("invalid fault_injection.rules entry {rule:?}: {err}"))?;
    }
    Ok(())
}

pub(crate) fn effective_config_toml(config: &ProjectConfig) -> String {
    let build = &config.kernel.build;
    let fault = &config.fault_injection;
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
        "# source: {}\n[kernel.build]\ncodegen_units = {}\nopt_level = {:?}\noverflow_checks = {}\ndebug_assertions = {}\nlto = {:?}\nforce_frame_pointers = {}\nincremental = {}\ndebuginfo = {:?}\nembed_bitcode = {}\npanic = {:?}\nrelocation_model = {:?}\nstrip = {:?}\nextra_rustflags = [{}]\n\n[fault_injection]\nenabled = {}\nrules = [{}]\n",
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
    )
}
