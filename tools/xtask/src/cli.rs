use clap::{CommandFactory, Parser, Subcommand};

use crate::Result;
use crate::build;
use crate::config::{self as config_mod, Config};
use crate::kvm;
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
    #[command(name = "kvm-smoke", disable_help_flag = true)]
    KvmSmoke {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<String>,
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
    #[command(name = "build-dvm")]
    BuildDvm,
    #[command(name = "verify-dvm")]
    VerifyDvm,
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
        Some(XtaskCommand::KvmSmoke { args }) => kvm::kvm_smoke_command(&config, args.into_iter()),
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
        Some(XtaskCommand::BuildDvm) => kvm::build_dvm_command(&config),
        Some(XtaskCommand::VerifyDvm) => kvm::verify_dvm_command(&config),
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
