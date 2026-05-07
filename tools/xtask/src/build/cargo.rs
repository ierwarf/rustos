use std::env;
use std::process::Command;

use crate::Result;
use crate::config::Config;
use crate::util::{env_string, run_command};

pub(crate) fn run_cargo_kernel_rustc(
    config: &Config,
    package: &str,
    rustc_args: &[String],
) -> Result<()> {
    let mut command = Command::new(&config.cargo);
    command
        .arg("rustc")
        .arg("--manifest-path")
        .arg(&config.workspace_manifest);
    for flag in &config.kernel_cargo_zflags {
        command.arg(flag);
    }
    command
        .arg("-p")
        .arg(package)
        .arg("--bin")
        .arg(package)
        .arg("--target")
        .arg(&config.kernel_target)
        .arg("--release")
        .arg("--");
    for arg in rustc_args {
        command.arg(arg);
    }
    apply_kernel_cargo_env(config, &mut command);
    run_command(&mut command)?;
    Ok(())
}

pub(crate) fn run_cargo_kernel_check(config: &Config, package: &str) -> Result<()> {
    let mut command = Command::new(&config.cargo);
    command
        .arg("check")
        .arg("--manifest-path")
        .arg(&config.workspace_manifest);
    for flag in &config.kernel_cargo_zflags {
        command.arg(flag);
    }
    command
        .arg("-p")
        .arg(package)
        .arg("--target")
        .arg(&config.kernel_target);
    apply_kernel_cargo_env(config, &mut command);
    run_command(&mut command)?;
    Ok(())
}

pub(crate) fn apply_kernel_cargo_env<'a>(
    config: &Config,
    command: &'a mut Command,
) -> &'a mut Command {
    let uses_sccache = rustc_wrapper_is_sccache(config);
    command
        .env("CARGO_TARGET_DIR", &config.cargo_target_dir)
        .env("RUSTFLAGS", kernel_rustflags_env(config))
        .env(
            "CARGO_INCREMENTAL",
            if config.project.kernel.build.incremental && !uses_sccache {
                "1"
            } else {
                "0"
            },
        );
    if uses_sccache {
        command
            .env("RUSTC_WRAPPER", "")
            .env("CARGO_BUILD_RUSTC_WRAPPER", "");
    }
    command
}

pub(crate) fn kernel_rustflags_env(config: &Config) -> String {
    let inherited = env::var("RUSTFLAGS").unwrap_or_default();
    config.project.kernel.build.rustflags(&inherited)
}

fn rustc_wrapper_is_sccache(config: &Config) -> bool {
    env_string("RUSTC_WRAPPER")
        .or_else(|| env_string("CARGO_BUILD_RUSTC_WRAPPER"))
        .or_else(|| configured_rustc_wrapper(config))
        .is_some_and(|wrapper| wrapper.contains("sccache"))
}

fn configured_rustc_wrapper(config: &Config) -> Option<String> {
    let cargo_config = config.root_dir.join(".cargo/config.toml");
    let text = fs_err::read_to_string(cargo_config).ok()?;
    let parsed = toml::from_str::<toml::Value>(&text).ok()?;
    parsed
        .get("build")
        .and_then(|build| build.get("rustc-wrapper"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
}
