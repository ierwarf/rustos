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
    #[command(name = "formal-contracts")]
    FormalContracts {
        #[command(subcommand)]
        command: FormalContractsCommand,
    },
    #[command(name = "kvm-smoke", disable_help_flag = true)]
    KvmSmoke {
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        args: Vec<String>,
    },
    #[command(name = "kvm-run")]
    KvmRun {
        #[arg(long = "build")]
        build_image: bool,
        #[arg(long = "rustos-vcpus", default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=8))]
        rustos_vcpus: u8,
        /// Refuse a multicore launch whose formal profile is unsealed instead
        /// of sealing it first.
        #[arg(long = "no-auto-verify")]
        no_auto_verify: bool,
    },
    /// Measure ring3 syscall, scheduler, and IPC round-trip cost.
    ///
    /// The image is always rebuilt. Measuring a stale one is silent: the run
    /// completes, the table looks ordinary, and it describes a binary that no
    /// longer exists in the tree.
    Bench {
        /// Write the rendered table to this path so a regression is visible in
        /// a diff instead of only in a terminal that has scrolled away.
        #[arg(long, conflicts_with = "isolate_probe")]
        baseline: Option<PathBuf>,
        /// Compare this run's `min` column against a previously written table,
        /// with the hardware anchor reported first. Every figure in this lane
        /// is an invariant-TSC tick, so a host clock change moves every probe
        /// at once and reads as an improvement; the anchor is what separates
        /// the two.
        #[arg(long, conflicts_with = "isolate_probe")]
        compare: Option<PathBuf>,
        /// Lock contention only exists with more than one CPU wanting the same
        /// word, so a contention claim needs a run at more than one.
        #[arg(long = "rustos-vcpus", default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=8))]
        rustos_vcpus: u8,
        /// Reboot with `ipcbench` restricted to this one probe, so its
        /// `ipc-call-phase-*` / `usermem-phase-*` counters belong to that probe
        /// alone for the whole boot instead of being summed across every probe
        /// the harness runs. Prints the isolated phase table only; the ordinary
        /// probe-timing table needs the unrestricted run.
        #[arg(long = "isolate-probe")]
        isolate_probe: Option<String>,
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

#[derive(Clone, Subcommand)]
pub(crate) enum FormalContractsCommand {
    /// Validate the typed contract graph and generated documentation.
    Check,
    /// Regenerate the human-readable contract index from the typed graph.
    Generate,
    /// Print the formal gates affected by the current worktree or named paths.
    Impact {
        /// Compare committed paths in BASE...HEAD (for CI); otherwise inspect
        /// the current worktree.
        #[arg(long)]
        base: Option<String>,
        #[arg(value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// Bind formal, build, and runtime evidence into one signed manifest.
    Evidence {
        #[arg(long, default_value = "pr")]
        profile: String,
        #[arg(long, default_value = "qemu-commercial")]
        topology: String,
        #[arg(long)]
        allow_dirty: bool,
        #[arg(long)]
        sign: bool,
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
    if let Some(XtaskCommand::FormalContracts { command }) = &cli.command {
        let root = env_path("ROOT_DIR").unwrap_or_else(default_root_dir);
        return crate::formal_contracts::run(&root, command);
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
        Some(XtaskCommand::FormalContracts { .. }) => {
            unreachable!("formal-contracts returned before loading config")
        }
        Some(XtaskCommand::KvmSmoke { args }) => kvm::kvm_smoke_command(&config, args.into_iter()),
        Some(XtaskCommand::KvmRun {
            build_image,
            rustos_vcpus,
            no_auto_verify,
        }) => kvm::kvm_run_command(&config, build_image, rustos_vcpus, !no_auto_verify),
        Some(XtaskCommand::Bench {
            baseline,
            compare,
            rustos_vcpus,
            isolate_probe,
        }) => crate::bench::bench(
            &config,
            baseline.as_deref(),
            compare.as_deref(),
            rustos_vcpus,
            isolate_probe.as_deref(),
        ),
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
