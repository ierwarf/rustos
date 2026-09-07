#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one match, found {count}")
    p.write_text(text.replace(old, new, 1))


# The benchmark must keep its longer runtime timeout. Its bounded formal seal
# is an internal launch policy, not the runtime --smp-iteration mode.
replace_once(
    "tools/xtask/src/bench.rs",
    '''    if rustos_vcpus > 1 {
        // Multi-vCPU benchmark runs are iterative performance evidence, not a
        // product/release admission lane. Use the bounded SMP verification
        // profile that kvm-run already uses for iterative multicore boots;
        // otherwise an unsealed tree launches the exhaustive PR verifier
        // before a benchmark and can spend minutes proving unrelated gates.
        kvm_args.push("--smp-iteration".to_owned());
    }
''',
    '''''',
)

# Add an internal-only profile override to the parsed launch request. No CLI
# spelling can set this field; only a trusted lane wrapper can do so.
replace_once(
    "tools/xtask/src/kvm/smoke_args.rs",
    '''    rustos_vcpus: u8,
    smp_iteration: bool,
    smp_ring3_qualification: bool,
''',
    '''    rustos_vcpus: u8,
    smp_iteration: bool,
    /// Internal launch-policy override. This is deliberately not parsed from
    /// CLI input: benchmark may select the bounded formal model without also
    /// enabling the 30-second runtime `smp_iteration` evidence mode.
    smp_formal_profile_override: Option<&'static str>,
    smp_ring3_qualification: bool,
''',
)
replace_once(
    "tools/xtask/src/kvm/smoke_args.rs",
    '''        rustos_vcpus: 1,
        smp_iteration: false,
        smp_ring3_qualification: false,
''',
    '''        rustos_vcpus: 1,
        smp_iteration: false,
        smp_formal_profile_override: None,
        smp_ring3_qualification: false,
''',
)

# Ordinary smoke retains the existing PR/iteration choice. Benchmark alone
# overrides formal admission to the bounded iterative profile.
replace_once(
    "tools/xtask/src/kvm/options.rs",
    '''    kvm_smoke_command_with_runtime_trace(config, args, Some(true))
''',
    '''    kvm_smoke_command_with_runtime_trace(config, args, Some(true), None)
''',
)
replace_once(
    "tools/xtask/src/kvm/options.rs",
    '''    kvm_smoke_command_with_runtime_trace(config, args, None)
''',
    '''    kvm_smoke_command_with_runtime_trace(config, args, None, Some("smp-iteration"))
''',
)
replace_once(
    "tools/xtask/src/kvm/options.rs",
    '''fn kvm_smoke_command_with_runtime_trace<I>(
    config: &Config,
    args: I,
    runtime_trace_deadlines: Option<bool>,
) -> Result<()>
''',
    '''fn kvm_smoke_command_with_runtime_trace<I>(
    config: &Config,
    args: I,
    runtime_trace_deadlines: Option<bool>,
    smp_formal_profile_override: Option<&'static str>,
) -> Result<()>
''',
)
replace_once(
    "tools/xtask/src/kvm/options.rs",
    '''    let options = parse_smoke_options(args.into_iter())?;
    let _launch_lock = acquire_kvm_launch_lock(&config.build_dir.join("kvm"))?;
''',
    '''    let mut options = parse_smoke_options(args.into_iter())?;
    options.smp_formal_profile_override = smp_formal_profile_override;
    let _launch_lock = acquire_kvm_launch_lock(&config.build_dir.join("kvm"))?;
''',
)
replace_once(
    "tools/xtask/src/kvm/options.rs",
    '''    if options.rustos_vcpus > 1 && options.auto_verify {
        let profile = if options.smp_iteration {
            "smp-iteration"
        } else {
            "pr"
        };
        crate::formal_contracts::ensure_smp_launch_evidence(&config.root_dir, profile)?;
    }
''',
    '''    if options.rustos_vcpus > 1 && options.auto_verify {
        crate::formal_contracts::ensure_smp_launch_evidence(
            &config.root_dir,
            smp_formal_profile(&options),
        )?;
    }
''',
)
# Put the policy helper beside the wrappers so guest/recovery paths consume
# exactly the same identity that pre-launch sealing used.
replace_once(
    "tools/xtask/src/kvm/options.rs",
    '''pub(crate) fn kvm_smoke_command<I>(config: &Config, args: I) -> Result<()>
''',
    '''fn smp_formal_profile(options: &SmokeOptions) -> &'static str {
    options.smp_formal_profile_override.unwrap_or(if options.smp_iteration {
        "smp-iteration"
    } else {
        "pr"
    })
}

pub(crate) fn kvm_smoke_command<I>(config: &Config, args: I) -> Result<()>
''',
)
# Interactive literal has no override; its existing smp_iteration flag still
# selects the bounded profile for multicore operator sessions.
replace_once(
    "tools/xtask/src/kvm/options.rs",
    '''        smp_iteration: rustos_vcpus > 1,
        smp_ring3_qualification: false,
''',
    '''        smp_iteration: rustos_vcpus > 1,
        smp_formal_profile_override: None,
        smp_ring3_qualification: false,
''',
)

# Spawn-time admission and reboot recovery must validate the same profile the
# pre-launch gate sealed. Keep runtime smp_iteration semantics independent.
replace_once(
    "tools/xtask/src/kvm/guest.rs",
    '''        options.rustos_vcpus,
        options.smp_iteration,
        false,
''',
    '''        options.rustos_vcpus,
        smp_formal_profile(options),
        false,
''',
)
replace_once(
    "tools/xtask/src/kvm/guest.rs",
    '''    rustos_vcpus: u8,
    smp_iteration: bool,
    append_logs: bool,
) -> Result<Child> {
''',
    '''    rustos_vcpus: u8,
    evidence_profile: &'static str,
    append_logs: bool,
) -> Result<Child> {
''',
)
replace_once(
    "tools/xtask/src/kvm/guest.rs",
    '''    let evidence_profile = if smp_iteration { "smp-iteration" } else { "pr" };
    let smp_evidence = (rustos_vcpus > 1)
''',
    '''    let smp_evidence = (rustos_vcpus > 1)
''',
)
replace_once(
    "tools/xtask/src/kvm/guest.rs",
    '''                self.options.rustos_vcpus,
                self.options.smp_iteration,
                false,
''',
    '''                self.options.rustos_vcpus,
                smp_formal_profile(self.options),
                false,
''',
)
