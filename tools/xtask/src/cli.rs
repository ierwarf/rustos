use clap::{CommandFactory, Parser, Subcommand};

use crate::Result;
use crate::build;
use crate::config::{self as config_mod, Config};
use crate::kvm;
use crate::stage;
use crate::testinfra;
use crate::util::{default_root_dir, env_path};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "cargo xtask", disable_version_flag = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<XtaskCommand>,
}

#[derive(Subcommand)]
enum XtaskCommand {
    Build {
        #[arg(long)]
        timings: bool,
    },
    Check {
        #[arg(long)]
        timings: bool,
    },
    Clean,
    #[command(name = "dev-plan")]
    DevPlan,
    #[command(name = "kvm-smoke", disable_help_flag = true)]
    KvmSmoke {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<String>,
    },
    #[command(name = "kvm-run")]
    KvmRun,
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
    #[command(name = "build-efi")]
    BuildEfi,
    #[command(name = "build-kernel")]
    BuildKernel,
    #[command(name = "build-user")]
    BuildUser,
    #[command(name = "build-dvm")]
    BuildDvm,
    #[command(name = "verify-dvm")]
    VerifyDvm,
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
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) if err.use_stderr() => return Err(err.into()),
        Err(err) => {
            err.print()?;
            return Ok(());
        }
    };

    if matches!(&cli.command, Some(XtaskCommand::DevPlan)) {
        let root = env_path("ROOT_DIR").unwrap_or_else(default_root_dir);
        return crate::dev::print_plan(&root);
    }
    if cli.command.is_none() {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    }

    let config = Config::from_env()?;

    match cli.command {
        Some(XtaskCommand::Build { timings }) => build::build(&config, timings),
        Some(XtaskCommand::Check { timings }) => build::check(&config, timings),
        Some(XtaskCommand::Clean) => build::clean(&config),
        Some(XtaskCommand::DevPlan) => unreachable!("dev-plan returned before loading config"),
        Some(XtaskCommand::KvmSmoke { args }) => kvm::kvm_smoke_command(&config, args.into_iter()),
        Some(XtaskCommand::KvmRun) => kvm::kvm_run_command(&config),
        Some(XtaskCommand::Selftest) => testinfra::selftest(&config),
        Some(XtaskCommand::FuzzHost {
            target,
            iterations,
            corpus,
        }) => testinfra::fuzz_host(&config, &target, iterations, corpus.as_deref()),
        Some(XtaskCommand::Stage) => stage::stage(&config),
        Some(XtaskCommand::BuildEfi) => build::build_efi(&config),
        Some(XtaskCommand::BuildKernel) => build::build_nucleus(&config),
        Some(XtaskCommand::BuildUser) => build::build_user(&config),
        Some(XtaskCommand::BuildDvm) => kvm::build_dvm_command(&config),
        Some(XtaskCommand::VerifyDvm) => kvm::verify_dvm_command(&config),
        Some(XtaskCommand::Config { command }) => match command {
            ConfigCommand::Check => config_mod::check(&config),
            ConfigCommand::Show => config_mod::show(&config),
        },
        None => unreachable!("missing command returned before loading config"),
    }
}
