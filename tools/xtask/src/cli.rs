use clap::{CommandFactory, Parser, Subcommand};

use crate::Result;
use crate::build;
use crate::config::{self as config_mod, Config};
use crate::qemu;
use crate::ring3_inventory;
use crate::stage;
use crate::testinfra;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "cargo xtask", disable_version_flag = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<XtaskCommand>,
}

#[derive(Subcommand)]
enum XtaskCommand {
    Build,
    Check,
    Clean,
    Run {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<String>,
    },
    Debug {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<String>,
    },
    #[command(name = "probe-display")]
    ProbeDisplay {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<String>,
    },
    #[command(name = "qemu-scenarios")]
    QemuScenarios {
        #[arg(long)]
        list: bool,
        #[arg(long = "scenario")]
        scenarios: Vec<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, default_value_t = 35)]
        timeout: u64,
    },
    Selftest,
    #[command(name = "fuzz-host")]
    FuzzHost {
        #[arg(long, default_value = "all")]
        target: String,
        #[arg(long, default_value_t = 256)]
        iterations: usize,
        #[arg(long)]
        corpus: Option<PathBuf>,
    },
    Stage,
    #[command(name = "targets", visible_alias = "target")]
    Targets,
    #[command(name = "build-efi")]
    BuildEfi,
    #[command(name = "build-kernel")]
    BuildKernel,
    #[command(name = "build-user")]
    BuildUser,
    #[command(name = "build-console-demo")]
    BuildConsoleDemo,
    #[command(name = "build-driver-modules")]
    BuildDriverModules,
    #[command(name = "ring3-inventory")]
    Ring3Inventory,
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    Check,
    Show,
}

pub(crate) fn run() -> Result<()> {
    let config = Config::from_env()?;
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) if err.use_stderr() => return Err(err.into()),
        Err(err) => {
            err.print()?;
            return Ok(());
        }
    };

    match cli.command {
        Some(XtaskCommand::Build) => build::build(&config),
        Some(XtaskCommand::Check) => build::check(&config),
        Some(XtaskCommand::Clean) => build::clean(&config),
        Some(XtaskCommand::Run { args }) => qemu::run_qemu_command(&config, args.into_iter()),
        Some(XtaskCommand::Debug { args }) => qemu::debug_qemu_command(&config, args.into_iter()),
        Some(XtaskCommand::ProbeDisplay { args }) => {
            qemu::probe_display_command(&config, args.into_iter())
        }
        Some(XtaskCommand::QemuScenarios {
            list,
            scenarios,
            dry_run,
            timeout,
        }) => qemu::scenarios_command(&config, list, scenarios, dry_run, timeout),
        Some(XtaskCommand::Selftest) => testinfra::selftest(&config),
        Some(XtaskCommand::FuzzHost {
            target,
            iterations,
            corpus,
        }) => testinfra::fuzz_host(&config, &target, iterations, corpus.as_deref()),
        Some(XtaskCommand::Stage) => stage::stage(&config),
        Some(XtaskCommand::Targets) => build::ensure_targets(&config),
        Some(XtaskCommand::BuildEfi) => build::build_efi(&config),
        Some(XtaskCommand::BuildKernel) => build::build_nucleus(&config),
        Some(XtaskCommand::BuildUser) => build::build_user(&config),
        Some(XtaskCommand::BuildConsoleDemo) => build::build_console_demo(&config),
        Some(XtaskCommand::BuildDriverModules) => build::build_driver_modules(&config),
        Some(XtaskCommand::Ring3Inventory) => ring3_inventory::print_inventory(&config),
        Some(XtaskCommand::Config { command }) => match command {
            ConfigCommand::Check => config_mod::check(&config),
            ConfigCommand::Show => config_mod::show(&config),
        },
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}
