use anyhow::{anyhow, bail};
use fs_err as fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::Command;

use crate::Result;
use crate::config::{Config, validate_project_config_text};
use crate::package_manifest::validate_manifest_text_for_testinfra;
use crate::util::run_command;

pub(crate) fn selftest(config: &Config) -> Result<()> {
    for package in [
        "rustos-fault-injection",
        "rustos-user-abi",
        "runtime-control",
        "module-tests",
    ] {
        let mut command = Command::new(&config.cargo);
        command
            .arg("test")
            .arg("-p")
            .arg(package)
            .env("CARGO_TARGET_DIR", &config.cargo_target_dir);
        run_command(&mut command)?;
    }
    Ok(())
}

pub(crate) fn fuzz_host(
    config: &Config,
    target: &str,
    iterations: usize,
    corpus: Option<&Path>,
) -> Result<()> {
    if iterations == 0 {
        bail!("--iterations must be greater than zero");
    }

    let targets = match target {
        "all" => vec![
            FuzzTarget::FaultRules,
            FuzzTarget::ProjectConfig,
            FuzzTarget::PackageManifest,
        ],
        "fault-rules" => vec![FuzzTarget::FaultRules],
        "project-config" => vec![FuzzTarget::ProjectConfig],
        "package-manifest" => vec![FuzzTarget::PackageManifest],
        other => bail!("unknown fuzz target: {other}"),
    };

    for target in targets {
        run_target(config, target, iterations, corpus)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum FuzzTarget {
    FaultRules,
    ProjectConfig,
    PackageManifest,
}

impl FuzzTarget {
    fn name(self) -> &'static str {
        match self {
            Self::FaultRules => "fault-rules",
            Self::ProjectConfig => "project-config",
            Self::PackageManifest => "package-manifest",
        }
    }
}

fn run_target(
    config: &Config,
    target: FuzzTarget,
    iterations: usize,
    corpus: Option<&Path>,
) -> Result<()> {
    let seeds = load_seeds(target, corpus)?;
    let mut state = 0x5255_5354_4f53_u64 ^ iterations as u64;
    for index in 0..iterations {
        let seed = &seeds[index % seeds.len()];
        let bytes = mutate(seed, index, &mut state);
        let result = catch_unwind(AssertUnwindSafe(|| exercise_target(target, &bytes)));
        if result.is_err() {
            let crash_path = write_crash(config, target, index, &bytes)?;
            bail!(
                "host fuzz target {} panicked on iteration {}; input saved to {}",
                target.name(),
                index,
                crash_path.display()
            );
        }
    }
    println!(
        "fuzz-host: target {} completed {} iteration(s)",
        target.name(),
        iterations
    );
    Ok(())
}

fn load_seeds(target: FuzzTarget, corpus: Option<&Path>) -> Result<Vec<Vec<u8>>> {
    let mut seeds = default_seeds(target);
    if let Some(corpus) = corpus {
        for entry in fs::read_dir(corpus)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                seeds.push(fs::read(entry.path())?);
            }
        }
    }
    if seeds.is_empty() {
        return Err(anyhow!("no fuzz seeds available for {}", target.name()));
    }
    Ok(seeds)
}

fn default_seeds(target: FuzzTarget) -> Vec<Vec<u8>> {
    match target {
        FuzzTarget::FaultRules => vec![
            b"display.present=fail".to_vec(),
            b"pci.config.read=fail-after:3;virtio.queue=rate:1".to_vec(),
            b"alloc.page=drop-every:32".to_vec(),
        ],
        FuzzTarget::ProjectConfig => vec![
            b"[kernel.build]\ncodegen_units=1\nopt_level=\"2\"\noverflow_checks=true\ndebug_assertions=false\nlto=\"off\"\nforce_frame_pointers=true\nincremental=false\ndebuginfo=\"1\"\nembed_bitcode=false\npanic=\"abort\"\nrelocation_model=\"none\"\nstrip=\"none\"\nextra_rustflags=[]\n[fault_injection]\nenabled=false\nrules=[]\n".to_vec(),
            b"[fault_injection]\nenabled=true\nrules=[\"display.present=fail\"]\n".to_vec(),
        ],
        FuzzTarget::PackageManifest => vec![
            b"id=\"sample\"\nkind=\"service\"\n[build]\nbuilder=\"cargo-kernel-binary\"\npackage=\"sample\"\n[install]\npath=\"services/sample/sample.elf\"\n".to_vec(),
            b"id=\"sample-driver\"\nkind=\"bridge-driver\"\n[build]\nbuilder=\"module-image\"\npackage=\"sample-driver\"\n[install]\npath=\"system/drivers/sample.ko\"\n".to_vec(),
        ],
    }
}

fn exercise_target(target: FuzzTarget, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    match target {
        FuzzTarget::FaultRules => {
            let _ = rustos_fault_injection::parse_rules(&text);
        }
        FuzzTarget::ProjectConfig => {
            let _ = validate_project_config_text(&text);
        }
        FuzzTarget::PackageManifest => {
            let _ = validate_manifest_text_for_testinfra(&text);
        }
    }
}

fn mutate(seed: &[u8], index: usize, state: &mut u64) -> Vec<u8> {
    let mut out = seed.to_vec();
    if out.is_empty() {
        out.push(next_byte(state));
    }
    match index % 6 {
        0 => out.push(next_byte(state)),
        1 => {
            let pos = (*state as usize) % out.len();
            out[pos] = next_byte(state);
        }
        2 => {
            let pos = (*state as usize) % out.len();
            out.insert(pos, b'\n');
        }
        3 => out.truncate(((*state as usize) % out.len()).max(1)),
        4 => out.extend_from_slice(b"\0\xff[]="),
        _ => out.reverse(),
    }
    out
}

fn next_byte(state: &mut u64) -> u8 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    (*state >> 24) as u8
}

fn write_crash(
    config: &Config,
    target: FuzzTarget,
    index: usize,
    bytes: &[u8],
) -> Result<std::path::PathBuf> {
    fs::create_dir_all(&config.logs_dir)?;
    let path = config
        .logs_dir
        .join(format!("fuzz-crash-{}-{}.bin", target.name(), index));
    fs::write(&path, bytes)?;
    Ok(path)
}
