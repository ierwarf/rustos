//! Ring3 syscall, scheduler, and IPC cost lane.
//!
//! - **Owner:** this lane owns only launch, parse, and report; `apps/ipcbench`
//!   owns what is measured and every probe it runs uses a published ABI.
//! - **Boundary:** guest debugcon output is untrusted text; a malformed or
//!   missing result line reports as missing rather than as a zero cost.
//! - **Lifecycle:** boot the ordinary interactive topology, wait for the
//!   harness end marker, parse the run's debugcon log, then report.
//! - **Failure:** a run that never reaches the end marker fails the lane; a
//!   probe the guest skipped is reported as skipped, never as a passing zero.
//! - **Forbidden:** no bench-only kernel path, no privileged capability grant,
//!   and no baseline rewrite without an explicit request.

use std::fs;
use std::path::Path;

use anyhow::{Context, bail};

use crate::Result;
use crate::config::Config;
use crate::kvm;

const END_MARKER: &str = "ipcbench: end";
const RESULT_PREFIX: &str = "ipcbench: result ";
const SKIP_PREFIX: &str = "ipcbench: skip ";
const TSC_PREFIX: &str = "ipcbench: tsc_khz=";

/// Milestone families the in-kernel phase profiles publish. A name outside this
/// list is some other milestone that happens to carry two arguments, and
/// folding it into the phase table would invent a cost that was never measured.
const PHASE_PREFIXES: [&str; 4] = [
    "ipc-call-phase-",
    "usermem-phase-",
    "lock-phase-",
    "syscall-phase-",
];

/// One parsed probe result. Cycle counts are the primary record: they survive
/// a host frequency change, while the nanosecond columns do not.
struct BenchResult {
    name: String,
    iters: u64,
    min: u64,
    p50: u64,
    p99: u64,
    mean: u64,
    min_ns: u64,
    p50_ns: u64,
}

/// One in-kernel phase profile family, summed over the run.
///
/// The kernel deliberately emits cycles and samples separately rather than an
/// average, because the sample count is half the finding: it says how many
/// times one operation performs the phase.
struct PhaseTotal {
    name: String,
    cycles: u128,
    samples: u128,
}

impl PhaseTotal {
    fn per_sample(&self) -> u128 {
        if self.samples == 0 {
            0
        } else {
            self.cycles / self.samples
        }
    }
}

fn field(line: &str, key: &str) -> Option<u64> {
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(key)?.parse().ok())
}

/// Reads a milestone argument, which the debugcon encoder writes as hex.
fn hex_field(line: &str, key: &str) -> Option<u64> {
    line.split_whitespace().find_map(|token| {
        let value = token.strip_prefix(key)?.strip_prefix("0x")?;
        u64::from_str_radix(value, 16).ok()
    })
}

/// Extracts one phase-profile milestone, if this line carries one.
fn parse_phase_milestone(line: &str) -> Option<(String, u64, u64)> {
    let name = line
        .split_whitespace()
        .find_map(|token| token.strip_prefix("name="))?;
    if !PHASE_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        return None;
    }
    Some((name.to_owned(), hex_field(line, "arg0=")?, hex_field(line, "arg1=")?))
}

fn parse_result(line: &str) -> Option<BenchResult> {
    let name = line
        .split_whitespace()
        .find_map(|token| token.strip_prefix("name="))?
        .to_owned();
    Some(BenchResult {
        name,
        iters: field(line, "iters=")?,
        min: field(line, "min=")?,
        p50: field(line, "p50=")?,
        p99: field(line, "p99=")?,
        mean: field(line, "mean=")?,
        min_ns: field(line, "min_ns=")?,
        p50_ns: field(line, "p50_ns=")?,
    })
}

/// The guest writes through a debugcon envelope, so a result line arrives
/// wrapped rather than bare. Recover the payload before parsing.
fn payload_lines(log: &str) -> impl Iterator<Item = &str> {
    log.lines().map(|line| {
        let body = match line.find("payload=") {
            Some(index) => &line[index + "payload=".len()..],
            None => line,
        };
        body.trim_end().trim_end_matches("\\n")
    })
}

struct ParsedRun {
    tsc_khz: u64,
    results: Vec<BenchResult>,
    skipped: Vec<String>,
    phases: Vec<PhaseTotal>,
}

fn parse_log(log: &str) -> Result<ParsedRun> {
    let mut tsc_khz = 0;
    let mut results = Vec::new();
    let mut skipped = Vec::new();
    let mut phases: Vec<PhaseTotal> = Vec::new();
    let mut saw_end = false;
    // The phase counters are global and drain on a wall-clock window, so a
    // window that closed before the harness started describes boot, not the
    // benchmark. Accumulate only inside the run.
    let mut inside_run = false;

    for line in payload_lines(log) {
        if let Some(rest) = line.strip_prefix(TSC_PREFIX) {
            tsc_khz = rest.trim().parse().unwrap_or(0);
            inside_run = true;
        } else if let Some(rest) = line.strip_prefix(RESULT_PREFIX) {
            match parse_result(rest) {
                Some(result) => results.push(result),
                None => bail!("malformed ipcbench result line: {rest}"),
            }
        } else if let Some(rest) = line.strip_prefix(SKIP_PREFIX) {
            skipped.push(rest.trim().to_owned());
        } else if line.starts_with(END_MARKER) {
            saw_end = true;
            inside_run = false;
        } else if inside_run && let Some((name, cycles, samples)) = parse_phase_milestone(line) {
            match phases.iter_mut().find(|phase| phase.name == name) {
                Some(phase) => {
                    phase.cycles += u128::from(cycles);
                    phase.samples += u128::from(samples);
                }
                None => phases.push(PhaseTotal {
                    name,
                    cycles: u128::from(cycles),
                    samples: u128::from(samples),
                }),
            }
        }
    }

    if !saw_end {
        bail!("ipcbench never reached its end marker; the harness did not finish");
    }
    if results.is_empty() {
        bail!("ipcbench finished but reported no results");
    }
    phases.sort_by(|left, right| right.per_sample().cmp(&left.per_sample()));
    Ok(ParsedRun {
        tsc_khz,
        results,
        skipped,
        phases,
    })
}

/// Renders the in-kernel phase profiles the run collected.
///
/// These counters are system-wide, not bench-private: any task that ran during
/// the window contributes. That is stated in the header rather than hidden,
/// because a reader who assumes otherwise will over-attribute a phase to the
/// probe it sits next to.
fn render_phases(phases: &[PhaseTotal]) -> String {
    let mut out = String::new();
    if phases.is_empty() {
        return out;
    }
    out.push_str("\nin-kernel phase profile (system-wide, summed over the run):\n");
    out.push_str(&format!(
        "  {:<36} {:>12} {:>14}\n",
        "phase", "samples", "cyc/sample"
    ));
    out.push_str(&format!("  {}\n", "-".repeat(64)));
    for phase in phases {
        out.push_str(&format!(
            "  {:<36} {:>12} {:>14}\n",
            phase.name,
            phase.samples,
            phase.per_sample()
        ));
    }
    out
}

fn render(tsc_khz: u64, results: &[BenchResult], skipped: &[String]) -> String {
    let mut out = String::new();
    out.push_str(&format!("tsc_khz={tsc_khz}\n\n"));
    out.push_str(&format!(
        "{:<40} {:>8} {:>10} {:>10} {:>10} {:>10} {:>9} {:>9}\n",
        "probe", "iters", "min_cyc", "p50_cyc", "p99_cyc", "mean_cyc", "min_ns", "p50_ns"
    ));
    out.push_str(&"-".repeat(112));
    out.push('\n');
    for result in results {
        out.push_str(&format!(
            "{:<40} {:>8} {:>10} {:>10} {:>10} {:>10} {:>9} {:>9}\n",
            result.name,
            result.iters,
            result.min,
            result.p50,
            result.p99,
            result.mean,
            result.min_ns,
            result.p50_ns,
        ));
    }
    for entry in skipped {
        out.push_str(&format!("SKIPPED {entry}\n"));
    }
    out
}

/// Derived costs the raw table cannot show: a round trip is only meaningful
/// against the local-syscall floor it is built on.
fn render_derived(results: &[BenchResult]) -> String {
    let find = |name: &str| results.iter().find(|result| result.name == name);
    let mut out = String::new();
    let (Some(local), Some(offload)) = (
        find("null_syscall_getpid"),
        find("ipc_rt_cross_process_syscalld_getuid"),
    ) else {
        return out;
    };
    let rt_min = offload.min.saturating_sub(local.min);
    let rt_p50 = offload.p50.saturating_sub(local.p50);
    out.push_str("\nderived:\n");
    out.push_str(&format!(
        "  cross-process IPC round trip (getuid - getpid): min={rt_min} cyc  p50={rt_p50} cyc\n"
    ));
    if local.min > 0 {
        out.push_str(&format!(
            "  round trip / local syscall ratio: min={:.1}x  p50={:.1}x\n",
            rt_min as f64 / local.min as f64,
            rt_p50 as f64 / local.p50 as f64,
        ));
    }
    out
}

pub(crate) fn bench(
    config: &Config,
    build_image: bool,
    baseline: Option<&Path>,
    rustos_vcpus: u8,
) -> Result<()> {
    if build_image {
        crate::build::build(config, false)?;
    }

    // The harness is a session-startup program, so it needs the same
    // interactive topology an ordinary desktop launch brings up. Requiring the
    // end marker is what bounds the run: a guest that never finished must not
    // fall through to parsing a stale log.
    kvm::kvm_smoke_command(
        config,
        [
            "--gui-dvm-surfaces".to_owned(),
            "--dvm-network-shmem".to_owned(),
            "--dvm-block-shmem".to_owned(),
            "--timeout".to_owned(),
            "120".to_owned(),
            // Lock contention is invisible on one CPU: `lock-phase-spin` only
            // moves when two CPUs actually want the same word. Comparing a
            // one-vCPU run against a multi-vCPU one is how a sharding or
            // lock-free change earns its risk.
            "--rustos-vcpus".to_owned(),
            rustos_vcpus.to_string(),
            "--expect".to_owned(),
            END_MARKER.to_owned(),
        ]
        .into_iter(),
    )
    .context("boot the interactive topology and wait for the ipcbench end marker")?;

    let log_path = config.build_dir.join("kvm/rustos-debugcon.log");
    let log = fs::read_to_string(&log_path)
        .with_context(|| format!("read debugcon log {}", log_path.display()))?;
    let run = parse_log(&log)?;

    let table = render(run.tsc_khz, &run.results, &run.skipped);
    let derived = render_derived(&run.results);
    let phases = render_phases(&run.phases);
    println!("\n=== ipcbench ===\n{table}{derived}{phases}");

    if let Some(path) = baseline {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, format!("{table}{derived}{phases}"))
            .with_context(|| format!("write baseline {}", path.display()))?;
        println!("baseline written to {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
user-debug payload=ipcbench: tsc_khz=3990809\\n
user-debug payload=ipcbench: result name=null_syscall_getpid iters=50000 min=3360 p50=3400 p90=3400 p99=5960 max=71972000 mean=6751 min_ns=841 p50_ns=851 mean_ns=1691\\n
user-debug payload=ipcbench: skip name=other reason=unavailable\\n
user-debug payload=ipcbench: end\\n";

    /// One real milestone line, verbatim, including the debugcon envelope the
    /// guest wraps it in.
    const MILESTONE: &str = "seq=316 ts_us=3238281 tick=3316 lvl=info cat=compat mod=nucleus_core::debug line=0 pid=- tid=- msg=\"milestone-begin v=1 output_seq=316 seq=281 ts_us=3238281 tick=3316 cat=compat name=ipc-call-phase-wait-take arg0=0x100 arg1=0x4 pid=- tid=- dropped=0 discarded_bytes=0 checksum=be4fc44b6e85f301 milestone-end\"";

    #[test]
    fn parses_wrapped_debugcon_payload_lines() {
        let run = parse_log(SAMPLE).expect("sample parses");
        assert_eq!(run.tsc_khz, 3_990_809);
        assert_eq!(run.results.len(), 1);
        assert_eq!(run.results[0].name, "null_syscall_getpid");
        assert_eq!(run.results[0].min, 3360);
        assert_eq!(run.results[0].p50_ns, 851);
        assert_eq!(run.skipped.len(), 1);
    }

    #[test]
    fn a_run_without_the_end_marker_fails_instead_of_reporting_partial_costs() {
        let truncated = SAMPLE.replace("ipcbench: end", "ipcbench: still-running");
        assert!(parse_log(&truncated).is_err());
    }

    #[test]
    fn a_finished_run_with_no_results_is_a_failure_not_an_empty_table() {
        let empty = "user-debug payload=ipcbench: end\\n";
        assert!(parse_log(empty).is_err());
    }

    #[test]
    fn phase_milestones_inside_the_run_are_summed_per_name() {
        let log = format!(
            "user-debug payload=ipcbench: tsc_khz=3990809\n{MILESTONE}\n{MILESTONE}\n\
             user-debug payload=ipcbench: result name=null_syscall_getpid iters=1 min=1 p50=1 p99=1 mean=1 min_ns=1 p50_ns=1\n\
             user-debug payload=ipcbench: end\n"
        );
        let run = parse_log(&log).expect("run parses");
        assert_eq!(run.phases.len(), 1);
        assert_eq!(run.phases[0].name, "ipc-call-phase-wait-take");
        // Two identical windows: 0x100 cycles over 4 samples, twice.
        assert_eq!(run.phases[0].samples, 8);
        assert_eq!(run.phases[0].per_sample(), 64);
    }

    #[test]
    fn a_phase_milestone_before_the_run_is_not_attributed_to_it() {
        // A window that closed during boot describes boot. Folding it in would
        // report a cost the benchmark never provoked.
        let log = format!(
            "{MILESTONE}\nuser-debug payload=ipcbench: tsc_khz=3990809\n\
             user-debug payload=ipcbench: result name=null_syscall_getpid iters=1 min=1 p50=1 p99=1 mean=1 min_ns=1 p50_ns=1\n\
             user-debug payload=ipcbench: end\n"
        );
        let run = parse_log(&log).expect("run parses");
        assert!(run.phases.is_empty(), "boot window leaked into the run");
    }

    #[test]
    fn a_milestone_outside_the_profile_families_is_ignored() {
        let unrelated = MILESTONE.replace("ipc-call-phase-wait-take", "kernel-scheduler-hold-max");
        let log = format!(
            "user-debug payload=ipcbench: tsc_khz=3990809\n{unrelated}\n\
             user-debug payload=ipcbench: result name=null_syscall_getpid iters=1 min=1 p50=1 p99=1 mean=1 min_ns=1 p50_ns=1\n\
             user-debug payload=ipcbench: end\n"
        );
        let run = parse_log(&log).expect("run parses");
        assert!(run.phases.is_empty());
    }
}
