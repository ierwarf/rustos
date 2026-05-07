use clap::{CommandFactory, Parser, Subcommand};

use crate::Result;
use crate::build;
use crate::config::{self as config_mod, Config};
use crate::qemu;
use crate::stage;

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
        Some(XtaskCommand::Stage) => stage::stage(&config),
        Some(XtaskCommand::Targets) => build::ensure_targets(&config),
        Some(XtaskCommand::BuildEfi) => build::build_efi(&config),
        Some(XtaskCommand::BuildKernel) => build::build_nucleus(&config),
        Some(XtaskCommand::BuildUser) => build::build_user(&config),
        Some(XtaskCommand::BuildConsoleDemo) => build::build_console_demo(&config),
        Some(XtaskCommand::BuildDriverModules) => build::build_driver_modules(&config),
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
