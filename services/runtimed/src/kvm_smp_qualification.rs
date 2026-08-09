//! KVM-only SMP workload admission.
//!
//! This private, unsigned registry is intentionally separate from the existing
//! KVM acceptance/profile contract.  It may add exactly one bounded workload
//! after normal signed launch metadata has been loaded; production boots and
//! malformed snapshots receive no additional launch policy.

use runtime_control::read_bounded_config_snapshot;
use rustos_user_abi::syscall::{
    SMP_QUALIFICATION_MAX_DEADLINE_MS, SMP_QUALIFICATION_MAX_WORK_UNITS,
};

use super::{LaunchEntry, DEFAULT_USER_TASK_WEIGHT_MICROS};

pub(super) const KVM_SMP_QUALIFICATION_CONTRACT_PATH: &str =
    "/system/registry/system/kvm-smp-qualification-v1.env";
const KVM_SMP_QUALIFICATION_CONTRACT_MAX_BYTES: usize = 160;
const MIN_WORK_UNITS: u32 = 1;
const MAX_WORK_UNITS: u32 = SMP_QUALIFICATION_MAX_WORK_UNITS as u32;
const MIN_DEADLINE_MS: u32 = 100;
const MAX_DEADLINE_MS: u32 = SMP_QUALIFICATION_MAX_DEADLINE_MS;
const SMPQUAL_PACKAGE_ID: &str = "smpqual";
const SMPQUAL_DESKTOP_FILE_ID: &str = "kvm-smp-qualification-v1";
const SMPQUAL_DISPLAY_NAME: &str = "KVM SMP qualification";
const SMPQUAL_EXEC: &str = "apps/smpqual/smpqual.elf";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct KvmSmpQualificationContract {
    pub(super) workers: u8,
    pub(super) work_units: u32,
    pub(super) deadline_ms: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ContractParseError {
    Layout,
    Value,
}

/// Loads one immutable snapshot using the positioned, bounded registry path.
/// Only a genuinely absent private file means that no KVM qualification was
/// requested. Every other snapshot or syntax failure remains visible to the
/// catalog retry owner, so an explicitly requested qualification cannot be
/// silently omitted.
pub(super) fn load_kvm_smp_qualification_contract(
) -> Result<Option<KvmSmpQualificationContract>, i32> {
    load_kvm_smp_qualification_contract_from_snapshot(read_bounded_config_snapshot(
        KVM_SMP_QUALIFICATION_CONTRACT_PATH,
        KVM_SMP_QUALIFICATION_CONTRACT_MAX_BYTES,
    ))
}

fn load_kvm_smp_qualification_contract_from_snapshot(
    snapshot: Result<String, std::io::Error>,
) -> Result<Option<KvmSmpQualificationContract>, i32> {
    match snapshot {
        Ok(contents) => parse_kvm_smp_qualification_contract(contents.as_str())
            .map(Some)
            .map_err(|_| libc::EINVAL),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(stable_snapshot_errno(error)),
    }
}

fn stable_snapshot_errno(error: std::io::Error) -> i32 {
    match error.raw_os_error() {
        Some(errno) if errno > 0 => errno,
        _ => match error.kind() {
            std::io::ErrorKind::PermissionDenied => libc::EACCES,
            std::io::ErrorKind::WouldBlock => libc::EAGAIN,
            std::io::ErrorKind::Unsupported => libc::ENOSYS,
            std::io::ErrorKind::InvalidData | std::io::ErrorKind::InvalidInput => libc::EINVAL,
            _ => libc::EIO,
        },
    }
}

pub(super) fn parse_kvm_smp_qualification_contract(
    contents: &str,
) -> Result<KvmSmpQualificationContract, ContractParseError> {
    // `split_terminator` permits one conventional final newline but retains
    // every interior blank/unknown line; CRLF therefore cannot accidentally
    // become an alternate accepted format.
    let mut lines = contents.split_terminator('\n');
    if lines.next() != Some("contract=rustos-kvm-smp-qualification-v1") {
        return Err(ContractParseError::Layout);
    }
    let workers = parse_decimal_line(lines.next(), "workers=", 1, 8)?;
    let work_units = parse_decimal_line(
        lines.next(),
        "work_units=",
        MIN_WORK_UNITS as u64,
        MAX_WORK_UNITS as u64,
    )?;
    let deadline_ms = parse_decimal_line(
        lines.next(),
        "deadline_ms=",
        MIN_DEADLINE_MS as u64,
        MAX_DEADLINE_MS as u64,
    )?;
    if lines.next().is_some() {
        return Err(ContractParseError::Layout);
    }
    let workers = u8::try_from(workers).map_err(|_| ContractParseError::Value)?;
    if !matches!(workers, 1 | 2 | 4 | 8) {
        return Err(ContractParseError::Value);
    }
    Ok(KvmSmpQualificationContract {
        workers,
        work_units: u32::try_from(work_units).map_err(|_| ContractParseError::Value)?,
        deadline_ms: u32::try_from(deadline_ms).map_err(|_| ContractParseError::Value)?,
    })
}

fn parse_decimal_line(
    line: Option<&str>,
    prefix: &str,
    minimum: u64,
    maximum: u64,
) -> Result<u64, ContractParseError> {
    let value = line
        .and_then(|line| line.strip_prefix(prefix))
        .ok_or(ContractParseError::Layout)?;
    if value.is_empty() {
        return Err(ContractParseError::Value);
    }
    if value.len() > 1 && value.as_bytes()[0] == b'0' {
        return Err(ContractParseError::Value);
    }
    let mut parsed = 0_u64;
    for byte in value.bytes() {
        if !byte.is_ascii_digit() {
            return Err(ContractParseError::Value);
        }
        parsed = parsed
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(byte - b'0')))
            .ok_or(ContractParseError::Value)?;
    }
    if !(minimum..=maximum).contains(&parsed) {
        return Err(ContractParseError::Value);
    }
    Ok(parsed)
}

pub(super) fn inject_kvm_smp_qualification_launch(
    entries: &mut Vec<LaunchEntry>,
    contract: Option<KvmSmpQualificationContract>,
) -> Result<(), ()> {
    let Some(contract) = contract else {
        return Ok(());
    };
    // The private name/path are reserved.  Failing the catalog avoids making a
    // signed ordinary entry appear to satisfy this KVM-only qualification.
    if entries
        .iter()
        .any(|entry| entry.desktop_file_id == SMPQUAL_DESKTOP_FILE_ID || entry.exec == SMPQUAL_EXEC)
    {
        return Err(());
    }
    entries.push(LaunchEntry {
        package_id: String::from(SMPQUAL_PACKAGE_ID),
        desktop_file_id: String::from(SMPQUAL_DESKTOP_FILE_ID),
        display_name: String::from(SMPQUAL_DISPLAY_NAME),
        exec: String::from(SMPQUAL_EXEC),
        runtime_deps: Vec::new(),
        restart: false,
        weight_micros: DEFAULT_USER_TASK_WEIGHT_MICROS,
        logical_admin: false,
        console_hosted: false,
        args: vec![
            String::from(SMPQUAL_EXEC),
            String::from("--workers"),
            contract.workers.to_string(),
            String::from("--work-units"),
            contract.work_units.to_string(),
            String::from("--deadline-ms"),
            contract.deadline_ms.to_string(),
        ],
        env: Vec::new(),
        private_smp_qualification: Some(contract),
    });
    Ok(())
}

/// Recovers the exact private contract only from the launch entry produced by
/// this module. A partially matching reserved identity is corruption, never an
/// ordinary launch that may bypass the kernel bind transaction.
pub(super) fn qualification_contract_for_launch(
    entry: &LaunchEntry,
) -> Result<Option<KvmSmpQualificationContract>, ()> {
    let reserved = entry.desktop_file_id == SMPQUAL_DESKTOP_FILE_ID || entry.exec == SMPQUAL_EXEC;
    let private_contract = match (reserved, entry.private_smp_qualification) {
        (false, None) => return Ok(None),
        (true, Some(contract)) => contract,
        // A reserved catalog identity without the injector-owned marker, or a
        // marker attached to any other identity, is forged launch provenance.
        _ => return Err(()),
    };
    if entry.package_id != SMPQUAL_PACKAGE_ID
        || entry.desktop_file_id != SMPQUAL_DESKTOP_FILE_ID
        || entry.display_name != SMPQUAL_DISPLAY_NAME
        || entry.exec != SMPQUAL_EXEC
        || !entry.runtime_deps.is_empty()
        || entry.restart
        || entry.weight_micros != DEFAULT_USER_TASK_WEIGHT_MICROS
        || entry.logical_admin
        || entry.console_hosted
        || !entry.env.is_empty()
        || entry.args.len() != 7
        || entry.args[0] != SMPQUAL_EXEC
        || entry.args[1] != "--workers"
        || entry.args[3] != "--work-units"
        || entry.args[5] != "--deadline-ms"
    {
        return Err(());
    }
    let workers = parse_canonical_decimal(entry.args[2].as_str())?;
    let work_units = parse_canonical_decimal(entry.args[4].as_str())?;
    let deadline_ms = parse_canonical_decimal(entry.args[6].as_str())?;
    let workers = u8::try_from(workers).map_err(|_| ())?;
    if !matches!(workers, 1 | 2 | 4 | 8)
        || !(MIN_WORK_UNITS..=MAX_WORK_UNITS).contains(&work_units)
        || !(MIN_DEADLINE_MS..=MAX_DEADLINE_MS).contains(&deadline_ms)
    {
        return Err(());
    }
    let recovered = KvmSmpQualificationContract {
        workers,
        work_units,
        deadline_ms,
    };
    (recovered == private_contract)
        .then_some(recovered)
        .map(Some)
        .ok_or(())
}

fn parse_canonical_decimal(text: &str) -> Result<u32, ()> {
    if text.is_empty() || text.len() > 1 && text.as_bytes()[0] == b'0' {
        return Err(());
    }
    let mut value = 0_u32;
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            return Err(());
        }
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u32::from(byte - b'0')))
            .ok_or(())?;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{
        inject_kvm_smp_qualification_launch, load_kvm_smp_qualification_contract_from_snapshot,
        parse_kvm_smp_qualification_contract, qualification_contract_for_launch,
        ContractParseError, KvmSmpQualificationContract, SMPQUAL_DESKTOP_FILE_ID, SMPQUAL_EXEC,
    };
    use crate::LaunchEntry;
    use std::io::{Error, ErrorKind};

    const EXACT: &str =
        "contract=rustos-kvm-smp-qualification-v1\nworkers=4\nwork_units=4096\ndeadline_ms=5000\n";

    fn ordinary_entry() -> LaunchEntry {
        LaunchEntry {
            package_id: String::from("ordinary"),
            desktop_file_id: String::from("ordinary.desktop"),
            display_name: String::from("ordinary"),
            exec: String::from("apps/ordinary/ordinary.elf"),
            runtime_deps: Vec::new(),
            restart: false,
            weight_micros: 100,
            logical_admin: false,
            console_hosted: false,
            args: Vec::new(),
            env: Vec::new(),
            private_smp_qualification: None,
        }
    }

    #[test]
    fn parser_accepts_only_the_canonical_ordered_contract() {
        assert_eq!(
            parse_kvm_smp_qualification_contract(EXACT),
            Ok(KvmSmpQualificationContract {
                workers: 4,
                work_units: 4096,
                deadline_ms: 5000,
            })
        );
        for malformed in [
            "contract=rustos-kvm-smp-qualification-v2\nworkers=4\nwork_units=4096\ndeadline_ms=5000\n",
            "workers=4\ncontract=rustos-kvm-smp-qualification-v1\nwork_units=4096\ndeadline_ms=5000\n",
            "contract=rustos-kvm-smp-qualification-v1\nworkers=4\nworkers=4\nwork_units=4096\ndeadline_ms=5000\n",
            "contract=rustos-kvm-smp-qualification-v1\nworkers=4\nwork_units=4096\ndeadline_ms=5000\nunknown=1\n",
            "contract=rustos-kvm-smp-qualification-v1\nworkers=04\nwork_units=4096\ndeadline_ms=5000\n",
            "contract=rustos-kvm-smp-qualification-v1\nworkers=4\nwork_units=04096\ndeadline_ms=5000\n",
            "contract=rustos-kvm-smp-qualification-v1\nworkers=4\nwork_units=4096\ndeadline_ms=05000\n",
        ] {
            assert!(parse_kvm_smp_qualification_contract(malformed).is_err());
        }
    }

    #[test]
    fn parser_rejects_worker_set_and_safe_bound_violations() {
        assert_eq!(
            parse_kvm_smp_qualification_contract(
                "contract=rustos-kvm-smp-qualification-v1\nworkers=3\nwork_units=4096\ndeadline_ms=5000\n"
            ),
            Err(ContractParseError::Value)
        );
        for malformed in [
            "contract=rustos-kvm-smp-qualification-v1\nworkers=1\nwork_units=0\ndeadline_ms=5000\n",
            "contract=rustos-kvm-smp-qualification-v1\nworkers=1\nwork_units=10000001\ndeadline_ms=5000\n",
            "contract=rustos-kvm-smp-qualification-v1\nworkers=1\nwork_units=1\ndeadline_ms=99\n",
            "contract=rustos-kvm-smp-qualification-v1\nworkers=1\nwork_units=1\ndeadline_ms=5001\n",
        ] {
            assert!(parse_kvm_smp_qualification_contract(malformed).is_err());
        }
    }

    #[test]
    fn missing_contract_is_the_only_normal_no_qualification_result() {
        assert_eq!(
            load_kvm_smp_qualification_contract_from_snapshot(Err(Error::from(
                ErrorKind::NotFound,
            ))),
            Ok(None)
        );
    }

    #[test]
    fn snapshot_failures_preserve_raw_errno_or_use_stable_errno() {
        for errno in [
            libc::EAGAIN,
            libc::ENODEV,
            libc::ENOSYS,
            libc::EACCES,
            libc::EIO,
        ] {
            assert_eq!(
                load_kvm_smp_qualification_contract_from_snapshot(Err(Error::from_raw_os_error(
                    errno
                ),)),
                Err(errno)
            );
        }
        assert_eq!(
            load_kvm_smp_qualification_contract_from_snapshot(Err(Error::new(
                ErrorKind::InvalidData,
                "oversized or invalid UTF-8 snapshot",
            ))),
            Err(libc::EINVAL)
        );
    }

    #[test]
    fn malformed_contract_is_an_einval_catalog_failure() {
        assert_eq!(
            load_kvm_smp_qualification_contract_from_snapshot(Ok(String::from(
                "contract=rustos-kvm-smp-qualification-v1\nworkers=3\nwork_units=1\ndeadline_ms=100\n",
            ))),
            Err(libc::EINVAL)
        );
    }

    #[test]
    fn exact_contract_survives_snapshot_loading() {
        assert_eq!(
            load_kvm_smp_qualification_contract_from_snapshot(Ok(String::from(EXACT))),
            Ok(Some(KvmSmpQualificationContract {
                workers: 4,
                work_units: 4096,
                deadline_ms: 5000,
            }))
        );
    }

    #[test]
    fn absent_or_invalid_contract_injects_nothing() {
        let mut entries = vec![ordinary_entry()];
        inject_kvm_smp_qualification_launch(&mut entries, None).expect("absent is no-op");
        let invalid = parse_kvm_smp_qualification_contract(
            "contract=rustos-kvm-smp-qualification-v1\nworkers=3\nwork_units=1\ndeadline_ms=100\n",
        )
        .ok();
        inject_kvm_smp_qualification_launch(&mut entries, invalid).expect("invalid is no-op");
        assert_eq!(entries, vec![ordinary_entry()]);
    }

    #[test]
    fn exact_contract_injects_one_private_nonprivileged_nonrestarting_launch() {
        let contract = parse_kvm_smp_qualification_contract(EXACT).expect("exact contract");
        let mut entries = vec![ordinary_entry()];
        inject_kvm_smp_qualification_launch(&mut entries, Some(contract)).expect("inject once");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], ordinary_entry());
        let injected = entries[1].clone();
        assert_eq!(injected.desktop_file_id, SMPQUAL_DESKTOP_FILE_ID);
        assert_eq!(injected.exec, SMPQUAL_EXEC);
        assert!(!injected.restart);
        assert!(!injected.logical_admin);
        assert!(!injected.console_hosted);
        assert_eq!(
            injected.args,
            [
                "apps/smpqual/smpqual.elf",
                "--workers",
                "4",
                "--work-units",
                "4096",
                "--deadline-ms",
                "5000",
            ]
            .map(String::from)
        );
        assert!(inject_kvm_smp_qualification_launch(&mut entries, Some(contract)).is_err());
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.desktop_file_id == SMPQUAL_DESKTOP_FILE_ID)
                .count(),
            1
        );
        assert_eq!(
            qualification_contract_for_launch(&injected),
            Ok(Some(contract))
        );
        let mut unmarked_reserved = injected.clone();
        unmarked_reserved.private_smp_qualification = None;
        assert_eq!(
            qualification_contract_for_launch(&unmarked_reserved),
            Err(())
        );
        let mut mismatched_marker = injected.clone();
        mismatched_marker.private_smp_qualification = Some(KvmSmpQualificationContract {
            workers: 2,
            ..contract
        });
        assert_eq!(
            qualification_contract_for_launch(&mismatched_marker),
            Err(())
        );
        let mut wrong_weight = injected.clone();
        wrong_weight.weight_micros = wrong_weight.weight_micros.saturating_add(1);
        assert_eq!(qualification_contract_for_launch(&wrong_weight), Err(()));

        for conflicting in [
            LaunchEntry {
                desktop_file_id: String::from(SMPQUAL_DESKTOP_FILE_ID),
                ..ordinary_entry()
            },
            LaunchEntry {
                exec: String::from(SMPQUAL_EXEC),
                ..ordinary_entry()
            },
        ] {
            let mut entries = vec![conflicting.clone()];
            assert!(inject_kvm_smp_qualification_launch(&mut entries, Some(contract)).is_err());
            assert_eq!(entries, vec![conflicting]);
        }

        let mut malformed = injected;
        malformed.args[2] = String::from("04");
        assert_eq!(qualification_contract_for_launch(&malformed), Err(()));
        let mut foreign = ordinary_entry();
        assert_eq!(qualification_contract_for_launch(&foreign), Ok(None));
        foreign.exec = String::from(SMPQUAL_EXEC);
        assert_eq!(qualification_contract_for_launch(&foreign), Err(()));
    }
}
