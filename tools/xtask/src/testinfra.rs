use anyhow::{anyhow, bail};
use fs_err as fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::Command;

use crate::Result;
use crate::config::{Config, validate_project_config_text};
use crate::kvm::validate_dvm_manifest_text_for_testinfra;
use crate::package_manifest::validate_manifest_text_for_testinfra;
use crate::util::run_command;
use rustos_driver_domain_host::{ControlContract, DriverDomainPolicy, LaunchPlan};
use rustos_image_admission::{
    ELF64_HEADER_SIZE, ImageRegion, PE64_DOS_HEADER_SIZE, PE64_FILE_HEADER_SIZE, admit_elf64_image,
    admit_image, admit_pe64_image_headers, apply_pe64_base_relocations, validate_pe64_import_table,
};

pub(crate) fn selftest(config: &Config) -> Result<()> {
    for package in [
        "rustos-fault-injection",
        "rustos-driver-domain-host",
        "rustos-image-admission",
        "rustos-user-abi",
        "runtime-control",
        "contract-tests",
        "xtask",
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
            FuzzTarget::DvmManifest,
            FuzzTarget::HostdLaunchPlan,
            FuzzTarget::ImageAdmission,
        ],
        "fault-rules" => vec![FuzzTarget::FaultRules],
        "project-config" => vec![FuzzTarget::ProjectConfig],
        "package-manifest" => vec![FuzzTarget::PackageManifest],
        "dvm-manifest" => vec![FuzzTarget::DvmManifest],
        "hostd-launch-plan" => vec![FuzzTarget::HostdLaunchPlan],
        "image-admission" => vec![FuzzTarget::ImageAdmission],
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
    DvmManifest,
    HostdLaunchPlan,
    ImageAdmission,
}

impl FuzzTarget {
    fn name(self) -> &'static str {
        match self {
            Self::FaultRules => "fault-rules",
            Self::ProjectConfig => "project-config",
            Self::PackageManifest => "package-manifest",
            Self::DvmManifest => "dvm-manifest",
            Self::HostdLaunchPlan => "hostd-launch-plan",
            Self::ImageAdmission => "image-admission",
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
            b"id=\"hidden-app\"\nkind=\"app\"\nstartup=\"session\"\n[build]\nbuilder=\"cargo-kernel-binary\"\npackage=\"hidden-app\"\n[install]\npath=\"apps/hidden-app/hidden-app.elf\"\n[[desktop.entries]]\ndisplay_name=\"hidden\"\nweight_micros=100\nno_display=true\n".to_vec(),
        ],
        FuzzTarget::DvmManifest => vec![
            format!(
                "schema=8\nid=rustos-linux-dvm-x86_64\narchitecture=x86_64\nboot=linux-bzimage+cpio-xz\ndata-plane=hostd-input-ring-msix\ncontrol-plane=agent-v1-control\ncontrol-protocol=agent-v1\ncontrol-state=control\ncontrol-transport=kvm-vsock\ncontrol-authentication=dvm-agent-hmac-sha256-v1\ncontrol-capabilities=health,device-inventory,driver-inventory,input-stream\ncontrol-contract-sha256={0}\nbuildroot_version=2026.05\nlinux_version=6.12.94\nnvidia-open-version=580.173.02\nnvidia-open-sha256=8d8eb9001e05a9a8a663d3d5d304feb64ef2844ee185ccdfd952786820f46e1b\nnvidia-open-redistribute=no\ndisplay-kernel-modules=i915,xe,amdgpu,nvidia-drm\nmodule-signing-enforced=yes\nmodule-signing-cert-sha256={0}\nkernel_sha256={0}\nrootfs_sha256={0}\nconfig_sha256={0}\nkernel-config-sha256={0}\nsources_lock_sha256={0}\n",
                "0".repeat(64)
            )
            .into_bytes(),
            b"schema=8\nschema=8\n".to_vec(),
        ],
        FuzzTarget::HostdLaunchPlan => vec![
            b"LAUNCH_PLAN_SCHEMA=1\nDOMAIN_ID=linux-dvm-net0\nDVM_GUEST_CID=4\nIOMMU_GROUP=15\nASSIGNED_PCI_BDFS=0000:02:00.0\nHOST_PROTECTED_PCI_BDFS=none\n".to_vec(),
            format!("DRIVER_DOMAIN_POLICY_SCHEMA=2\nDOMAIN_ID=linux-dvm-net0\nQEMU_SHA256={}\nINPUT_TRANSPORT=input-ring-msix\nNETWORK_TRANSPORT=disabled\nBLOCK_TRANSPORT=disabled\nDISPLAY_TRANSPORT=disabled\n", "0".repeat(64)).into_bytes(),
            b"CONTROL_SCHEMA=1\nCONTROL_PROTOCOL=agent-v1\nCONTROL_STATE=control\nCONTROL_TRANSPORT=kvm-vsock\nCONTROL_AUTHENTICATION=dvm-agent-hmac-sha256-v1\nCONTROL_CAPABILITIES=health,device-inventory,driver-inventory,input-stream\n".to_vec(),
            b"LAUNCH_PLAN_SCHEMA=1\nLAUNCH_PLAN_SCHEMA=1\n".to_vec(),
        ],
        FuzzTarget::ImageAdmission => vec![
            vec![
                0x05, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
            vec![0xff; 34],
            valid_elf64_fuzz_seed(),
            valid_pe64_fuzz_seed(),
            valid_pe64_relocation_fuzz_seed(),
            valid_pe64_import_fuzz_seed(),
        ],
    }
}

fn valid_elf64_fuzz_seed() -> Vec<u8> {
    let mut bytes = vec![0_u8; ELF64_HEADER_SIZE + 56];
    bytes[0..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2;
    bytes[5] = 1;
    bytes[6] = 1;
    bytes[16..18].copy_from_slice(&2_u16.to_le_bytes());
    bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
    bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
    bytes[24..32].copy_from_slice(&0x2100_u64.to_le_bytes());
    bytes[32..40].copy_from_slice(&(ELF64_HEADER_SIZE as u64).to_le_bytes());
    bytes[52..54].copy_from_slice(&(ELF64_HEADER_SIZE as u16).to_le_bytes());
    bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
    bytes[56..58].copy_from_slice(&1_u16.to_le_bytes());
    let ph = ELF64_HEADER_SIZE;
    bytes[ph..ph + 4].copy_from_slice(&1_u32.to_le_bytes());
    bytes[ph + 4..ph + 8].copy_from_slice(&5_u32.to_le_bytes());
    bytes[ph + 16..ph + 24].copy_from_slice(&0x2000_u64.to_le_bytes());
    bytes[ph + 32..ph + 40].copy_from_slice(&0x100_u64.to_le_bytes());
    bytes[ph + 40..ph + 48].copy_from_slice(&0x1000_u64.to_le_bytes());
    bytes[ph + 48..ph + 56].copy_from_slice(&0x1000_u64.to_le_bytes());
    bytes
}

fn valid_pe64_fuzz_seed() -> Vec<u8> {
    const OPTIONAL_BYTES: usize = 241;
    let mut bytes = vec![0_u8; PE64_DOS_HEADER_SIZE + PE64_FILE_HEADER_SIZE + OPTIONAL_BYTES + 40];
    bytes[0..2].copy_from_slice(b"MZ");
    bytes[0x3c..0x40].copy_from_slice(&0x80_u32.to_le_bytes());
    let file = PE64_DOS_HEADER_SIZE;
    bytes[file..file + 4].copy_from_slice(b"PE\0\0");
    bytes[file + 4..file + 6].copy_from_slice(&0x8664_u16.to_le_bytes());
    bytes[file + 6..file + 8].copy_from_slice(&1_u16.to_le_bytes());
    bytes[file + 20..file + 22].copy_from_slice(&(OPTIONAL_BYTES as u16).to_le_bytes());
    let optional = file + PE64_FILE_HEADER_SIZE;
    bytes[optional..optional + 2].copy_from_slice(&0x20b_u16.to_le_bytes());
    bytes[optional + 16..optional + 20].copy_from_slice(&0x1000_u32.to_le_bytes());
    bytes[optional + 24..optional + 32].copy_from_slice(&0x400000_u64.to_le_bytes());
    bytes[optional + 32..optional + 36].copy_from_slice(&0x1000_u32.to_le_bytes());
    bytes[optional + 36..optional + 40].copy_from_slice(&0x200_u32.to_le_bytes());
    bytes[optional + 56..optional + 60].copy_from_slice(&0x2000_u32.to_le_bytes());
    bytes[optional + 60..optional + 64].copy_from_slice(&0x200_u32.to_le_bytes());
    bytes[optional + 108..optional + 112].copy_from_slice(&16_u32.to_le_bytes());
    let section = optional + OPTIONAL_BYTES;
    bytes[section + 8..section + 12].copy_from_slice(&0x1000_u32.to_le_bytes());
    bytes[section + 12..section + 16].copy_from_slice(&0x1000_u32.to_le_bytes());
    bytes[section + 16..section + 20].copy_from_slice(&0x200_u32.to_le_bytes());
    bytes[section + 20..section + 24].copy_from_slice(&0x200_u32.to_le_bytes());
    bytes[section + 36..section + 40].copy_from_slice(&0x6000_0000_u32.to_le_bytes());
    bytes
}

fn valid_pe64_relocation_fuzz_seed() -> Vec<u8> {
    let mut bytes = vec![0_u8; 256];
    bytes[0..4].copy_from_slice(&0x20_u32.to_le_bytes());
    bytes[4..8].copy_from_slice(&10_u32.to_le_bytes());
    bytes[0x20..0x24].copy_from_slice(&0_u32.to_le_bytes());
    bytes[0x24..0x28].copy_from_slice(&10_u32.to_le_bytes());
    bytes[0x28..0x2a].copy_from_slice(&0xa080_u16.to_le_bytes());
    bytes[0x80..0x88].copy_from_slice(&0x400000_u64.to_le_bytes());
    bytes
}

fn valid_pe64_import_fuzz_seed() -> Vec<u8> {
    let mut bytes = vec![0_u8; 256];
    bytes[0..4].copy_from_slice(&0x20_u32.to_le_bytes());
    bytes[4..8].copy_from_slice(&40_u32.to_le_bytes());
    bytes[0x20..0x24].copy_from_slice(&0xa0_u32.to_le_bytes());
    bytes[0x2c..0x30].copy_from_slice(&0x80_u32.to_le_bytes());
    bytes[0x30..0x34].copy_from_slice(&0xb0_u32.to_le_bytes());
    bytes[0x80..0x89].copy_from_slice(b"test.dll\0");
    bytes[0xa0..0xa8].copy_from_slice(&0xc0_u64.to_le_bytes());
    bytes[0xc2..0xc7].copy_from_slice(b"Func\0");
    bytes
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
        FuzzTarget::DvmManifest => {
            let _ = validate_dvm_manifest_text_for_testinfra(&text);
        }
        FuzzTarget::HostdLaunchPlan => {
            let _ = LaunchPlan::parse(&text, "fuzz-host");
            let _ = DriverDomainPolicy::parse(&text, "fuzz-host");
            let _ = ControlContract::parse(&text, "fuzz-host");
        }
        FuzzTarget::ImageAdmission => exercise_image_admission(bytes),
    }
}

fn exercise_image_admission(bytes: &[u8]) {
    let mut regions = Vec::new();
    for chunk in bytes.chunks_exact(17).take(128) {
        regions.push(ImageRegion {
            flags: chunk[0],
            start: u64::from_le_bytes(chunk[1..9].try_into().unwrap()),
            len: u64::from_le_bytes(chunk[9..17].try_into().unwrap()),
        });
    }
    let entry_point = bytes
        .get(..8)
        .map(|value| u64::from_le_bytes(value.try_into().unwrap()))
        .unwrap_or(0);
    let _ = admit_image(
        entry_point,
        &regions,
        0x1000,
        0x1_0000,
        bytes.len().is_multiple_of(2),
    );

    let mut elf_header = [0_u8; ELF64_HEADER_SIZE];
    let elf_header_len = bytes.len().min(elf_header.len());
    elf_header[..elf_header_len].copy_from_slice(&bytes[..elf_header_len]);
    let elf_program_headers = bytes.get(ELF64_HEADER_SIZE..).unwrap_or_default();
    let _ = admit_elf64_image(&elf_header, elf_program_headers, 0x400000, 0x1000, 0x800000);

    let mut dos_header = [0_u8; PE64_DOS_HEADER_SIZE];
    let dos_len = bytes.len().min(dos_header.len());
    dos_header[..dos_len].copy_from_slice(&bytes[..dos_len]);
    let mut file_header = [0_u8; PE64_FILE_HEADER_SIZE];
    if let Some(file_bytes) = bytes.get(PE64_DOS_HEADER_SIZE..) {
        let file_len = file_bytes.len().min(file_header.len());
        file_header[..file_len].copy_from_slice(&file_bytes[..file_len]);
    }
    let optional_start = PE64_DOS_HEADER_SIZE + PE64_FILE_HEADER_SIZE;
    let optional_size = usize::from(u16::from_le_bytes([file_header[20], file_header[21]]));
    let optional_end = optional_start
        .checked_add(optional_size)
        .filter(|end| *end <= bytes.len())
        .unwrap_or(bytes.len());
    let optional_header = bytes.get(optional_start..optional_end).unwrap_or_default();
    let section_headers = bytes.get(optional_end..).unwrap_or_default();
    let _ = admit_pe64_image_headers(
        &dos_header,
        &file_header,
        optional_header,
        section_headers,
        0x400000,
        0x1000,
        0x800000,
        128 * 1024 * 1024,
        bytes.len().is_multiple_of(2),
    );

    let mut image = bytes.iter().copied().take(64 * 1024).collect::<Vec<_>>();
    let reloc_rva = bytes
        .get(..4)
        .map(|raw| u32::from_le_bytes(raw.try_into().unwrap()))
        .unwrap_or(0);
    let reloc_size = bytes
        .get(4..8)
        .map(|raw| u32::from_le_bytes(raw.try_into().unwrap()))
        .unwrap_or(0);
    let _ = apply_pe64_base_relocations(&mut image, 0x400000, 0x500000, reloc_rva, reloc_size, 0);
    let _ = validate_pe64_import_table(&image, reloc_rva, reloc_size, 65_536);
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
