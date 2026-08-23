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

/// The probe that contains no RustOS code.
///
/// `vmexit_cpuid` is `CPUID` from ring 3: the hypervisor exit and the host's
/// handling of it, and nothing this repository can change. Every figure in this
/// lane is an *invariant-TSC tick*, which advances at a fixed rate while the
/// core clock does not, so a host that boosts higher completes the same work in
/// fewer ticks. Comparing two runs across that shift reads as a uniform
/// improvement in every probe at once. Anchoring on a probe with no code in it
/// is what tells the two apart.
const HARDWARE_ANCHOR: &str = "vmexit_cpuid";

/// How far the anchor may move before a comparison stops being a comparison.
///
/// Seven consecutive runs held `vmexit_cpuid` between 4,760 and 4,840 ticks --
/// under 2% -- and the eighth read 3,960, a 17% shift with no guest change,
/// alongside a 17% shift in every other probe including `null_syscall_getpid`.
/// Three percent admits ordinary run-to-run variation and rejects that.
const ANCHOR_DRIFT_TOLERANCE_PERCENT: f64 = 3.0;

/// Guest TSC granularity, measured by `tsc_overhead` as its own `min`. A probe
/// at this floor is reporting the counter, not the work.
const TSC_GRANULARITY_TICKS: u64 = 40;

/// The smallest anchor-normalized change this lane can attribute to the guest.
///
/// Measured, not assumed. Three consecutive runs of one unchanged image against
/// one baseline reported `ipc_rt_intra_process` at +1.9%, -0.5% and -0.2%, and
/// `null_syscall_getpid` -- which is the control -- at +0.1%, +5.1% and -0.2%.
/// The binary was byte-identical across all three, so every one of those
/// numbers is the instrument. `min` over twenty thousand iterations is stable;
/// what is not stable is the anchor ratio the normalization divides by, and
/// the background service traffic the probes share a CPU with.
///
/// A change smaller than this needs a phase counter, which measures one
/// operation instead of a whole round trip, or more repeats. It does not need a
/// more confident reading of one pair of runs.
const RESOLVABLE_DELTA_PERCENT: f64 = 2.0;

/// Milestone families the in-kernel phase profiles publish. A name outside this
/// list is some other milestone that happens to carry two arguments, and
/// folding it into the phase table would invent a cost that was never measured.
const PHASE_PREFIXES: [&str; 5] = [
    "ipc-call-phase-",
    "ipc-server-phase-",
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
    if !PHASE_PREFIXES.iter().any(|prefix| name.starts_with(prefix)) {
        return None;
    }
    Some((
        name.to_owned(),
        hex_field(line, "arg0=")?,
        hex_field(line, "arg1=")?,
    ))
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
/// The phase charged exactly once per synchronous IPC round trip, used as the
/// unit that decides which other phases can be attributed to one.
const ROUND_TRIP_UNIT_PHASE: &str = "ipc-call-phase-copy-request";

/// How close a phase's sample count must be to the unit's before it can be read
/// as "once per round trip".
const ATTRIBUTABLE_RATIO: (f64, f64) = (0.95, 1.05);

/// Whether every phase row divides cleanly into one round trip: the
/// acceptance test for `--isolate-probe`. A phase absent from the run, or
/// present but charged by a probe that never reaches `ipc_call` at all
/// (`ROUND_TRIP_UNIT_PHASE` itself unsampled), does not contradict isolation.
fn isolation_holds(phases: &[PhaseTotal]) -> bool {
    let Some(unit) = phases
        .iter()
        .find(|phase| phase.name == ROUND_TRIP_UNIT_PHASE)
        .map(|phase| phase.samples)
        .filter(|samples| *samples > 0)
    else {
        return true;
    };
    phases.iter().all(|phase| {
        if phase.samples == 0 {
            return true;
        }
        let ratio = phase.samples as f64 / unit as f64;
        (ATTRIBUTABLE_RATIO.0..=ATTRIBUTABLE_RATIO.1).contains(&ratio)
    })
}

/// Semantic acceptance probes intentionally exercise scheduler transitions
/// rather than phase-profile attribution. Their kernel-stamped invariant is
/// the primary result gate; unrelated system-wide IPC housekeeping cannot be
/// divided by their iteration count and must not fabricate a failure.
fn requires_phase_attribution(probe: &str) -> bool {
    !matches!(
        probe,
        "scheduling_budget_exhaust_refill" | "ipc_nested_passive_server"
    )
}

fn isolated_primary_result_holds(run: &ParsedRun, probe: &str) -> bool {
    let primary_results = run
        .results
        .iter()
        .filter(|result| result.name == probe)
        .count();
    let primary_skips = run.skipped.iter().any(|skip| {
        skip.split_ascii_whitespace()
            .any(|field| field.strip_prefix("name=") == Some(probe))
    });
    primary_results == 1 && !primary_skips
}

fn render_phases(phases: &[PhaseTotal]) -> String {
    let mut out = String::new();
    if phases.is_empty() {
        return out;
    }
    let unit = phases
        .iter()
        .find(|phase| phase.name == ROUND_TRIP_UNIT_PHASE)
        .map(|phase| phase.samples)
        .filter(|samples| *samples > 0);

    out.push_str("\nin-kernel phase profile (system-wide, summed over the run):\n");
    out.push_str(&format!(
        "  {:<36} {:>12} {:>12} {:>8}\n",
        "phase", "samples", "cyc/sample", "per rt"
    ));
    out.push_str(&format!("  {}\n", "-".repeat(72)));
    for phase in phases {
        // The sample count is half the finding. A phase whose samples do not
        // match the unit's is charged by more probes than the round trip, and
        // its total cannot be divided into one -- which is exactly how an
        // aggregate gets multiplied by the wrong denominator and reported as a
        // per-call cost it never had.
        let per_round_trip = match unit {
            Some(unit) if phase.samples > 0 => {
                let ratio = phase.samples as f64 / unit as f64;
                if (ATTRIBUTABLE_RATIO.0..=ATTRIBUTABLE_RATIO.1).contains(&ratio) {
                    format!("{ratio:.2}")
                } else {
                    format!("({ratio:.2})")
                }
            }
            _ => String::from("-"),
        };
        out.push_str(&format!(
            "  {:<36} {:>12} {:>12} {:>8}\n",
            phase.name,
            phase.samples,
            phase.per_sample(),
            per_round_trip,
        ));
    }
    if unit.is_some() {
        out.push_str(
            "  a parenthesised ratio is shared with other probes: multiply it by\n             \x20 nothing, and do not read the phase as a per-round-trip cost\n",
        );
    }
    out
}

/// Reads the `min_cyc` column out of a previously written baseline table.
///
/// The baseline is the rendered table, so this parses what it wrote rather
/// than a second serialization that could drift from it.
fn parse_baseline_minimums(table: &str) -> Vec<(String, u64)> {
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let name = fields.next()?;
            if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || name == "probe" {
                return None;
            }
            let _iters: u64 = fields.next()?.parse().ok()?;
            let min: u64 = fields.next()?.parse().ok()?;
            Some((name.to_owned(), min))
        })
        .collect()
}

fn percent_change(before: u64, after: u64) -> f64 {
    if before == 0 {
        return 0.0;
    }
    ((after as f64 - before as f64) / before as f64) * 100.0
}

/// Compares this run's `min` column against a baseline, with the hardware
/// anchor in front of the result rather than in a footnote.
///
/// When the anchor held, the raw deltas are the answer. When it moved, the raw
/// deltas are contaminated by exactly that much, so the report says so and
/// prints the anchor-normalized column instead of a conclusion. Normalization
/// is an estimate and is labelled as one; the honest reading of a drifted run
/// is to rerun it, and the report says that too.
fn render_comparison(baseline_path: &Path, baseline: &str, results: &[BenchResult]) -> String {
    let previous = parse_baseline_minimums(baseline);
    let find_previous = |name: &str| {
        previous
            .iter()
            .find(|(probe, _)| probe == name)
            .map(|(_, min)| *min)
    };
    let current = |name: &str| {
        results
            .iter()
            .find(|result| result.name == name)
            .map(|result| result.min)
    };

    let mut out = format!("\ncomparison vs {}\n", baseline_path.display());
    let anchor = find_previous(HARDWARE_ANCHOR).zip(current(HARDWARE_ANCHOR));
    let Some((anchor_before, anchor_after)) = anchor else {
        out.push_str(&format!(
            "  no comparison: {HARDWARE_ANCHOR} is missing from one of the two runs, so\n               nothing separates a guest change from a host clock change\n"
        ));
        return out;
    };
    let anchor_drift = percent_change(anchor_before, anchor_after);
    let anchor_held = anchor_drift.abs() <= ANCHOR_DRIFT_TOLERANCE_PERCENT;
    out.push_str(&format!(
        "  anchor {HARDWARE_ANCHOR}: {anchor_before} -> {anchor_after} ({anchor_drift:+.1}%){}\n",
        if anchor_held {
            " -- held"
        } else {
            " -- MOVED, so every raw delta below is contaminated by this much"
        }
    ));
    let scale = if anchor_before == 0 {
        1.0
    } else {
        anchor_after as f64 / anchor_before as f64
    };

    out.push_str(&format!(
        "\n  {:<40} {:>10} {:>10} {:>9} {:>11}\n",
        "probe", "before", "after", "raw", "normalized"
    ));
    for result in results {
        let Some(before) = find_previous(&result.name) else {
            continue;
        };
        let raw = format!("{:.1}%", percent_change(before, result.min));
        // A probe already at the counter's granularity cannot scale with the
        // anchor: it reads the same handful of ticks at any core clock. Scaling
        // it produces a number that looks like a regression and means nothing.
        let normalized = if before <= TSC_GRANULARITY_TICKS || result.min <= TSC_GRANULARITY_TICKS {
            "floor".to_owned()
        } else {
            let expected = (before as f64 * scale).max(1.0);
            let delta = ((result.min as f64 - expected) / expected) * 100.0;
            // Below the instrument's own spread there is nothing to read. The
            // number is still printed, because hiding it invites the same
            // reader to go find it; the label is what stops it being reported
            // as a result.
            if delta.abs() < RESOLVABLE_DELTA_PERCENT {
                format!("{delta:.1}% noise")
            } else {
                format!("{delta:.1}%")
            }
        };
        out.push_str(&format!(
            "  {:<40} {:>10} {:>10} {:>9} {:>11}\n",
            result.name, before, result.min, raw, normalized,
        ));
    }
    if !anchor_held {
        out.push_str(
            "\n  The normalized column assumes every probe scales with the anchor, which is\n               an estimate, not a measurement. Rerun both sides in one session before\n               attributing a change to the guest.\n",
        );
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
    baseline: Option<&Path>,
    compare: Option<&Path>,
    rustos_vcpus: u8,
    isolate_probe: Option<&str>,
) -> Result<()> {
    // Unconditional, and this is the second measurement bug this lane has had.
    // Building was opt-in, so a run that forgot the flag booted whatever image
    // was last built and reported it without complaint: two runs across a
    // kernel change produced the same binary's numbers twice and read as "the
    // change did nothing". Nothing in the output could have shown that. The
    // build is incremental, so paying for it always is cheaper than one wrong
    // conclusion.
    crate::build::build(config, false)?;

    // The harness is a session-startup program, so it needs the same
    // interactive topology an ordinary desktop launch brings up. Requiring the
    // end marker is what bounds the run: a guest that never finished must not
    // fall through to parsing a stale log.
    let mut kvm_args = vec![
        "--gui-dvm-surfaces".to_owned(),
        "--dvm-network-shmem".to_owned(),
        "--dvm-block-shmem".to_owned(),
        "--timeout".to_owned(),
        // The guest normally reaches the harness terminal in under twenty
        // seconds even with the isolated-probe settle. Keep a broken boot
        // bounded by the repository's KVM acceptance ceiling.
        "30".to_owned(),
        // Lock contention is invisible on one CPU: `lock-phase-spin` only
        // moves when two CPUs actually want the same word. Comparing a
        // one-vCPU run against a multi-vCPU one is how a sharding or
        // lock-free change earns its risk.
        "--rustos-vcpus".to_owned(),
        rustos_vcpus.to_string(),
        "--expect".to_owned(),
        END_MARKER.to_owned(),
    ];
    if let Some(probe) = isolate_probe {
        // Every `ipc-call-phase-*`/`usermem-phase-*` counter is process-wide
        // for the whole boot, so running more than one probe in it makes a
        // phase's total undivideable by any one probe's round-trip count.
        // Restricting the boot to one probe is what makes the ratio below
        // meaningful instead of parenthesised.
        kvm_args.push("--ipcbench-probe".to_owned());
        kvm_args.push(probe.to_owned());
    }
    kvm::kvm_smoke_command(config, kvm_args.into_iter())
        .context("boot the interactive topology and wait for the ipcbench end marker")?;

    let log_path = config.build_dir.join("kvm/rustos-debugcon.log");
    let log = fs::read_to_string(&log_path)
        .with_context(|| format!("read debugcon log {}", log_path.display()))?;
    let run = parse_log(&log)?;

    if let Some(probe) = isolate_probe {
        if !isolated_primary_result_holds(&run, probe) {
            bail!(
                "isolated ipcbench probe {probe} did not produce exactly one non-skipped primary result"
            );
        }
        let phases = render_phases(&run.phases);
        if requires_phase_attribution(probe) && !isolation_holds(&run.phases) {
            bail!("isolated ipcbench phase attribution failed for probe {probe}");
        }
        let check = if requires_phase_attribution(probe) {
            format!(
                "phase attribution; every ipc-call-phase-*/usermem-phase-* row is inside {:.2}..={:.2} per round trip, or absent",
                ATTRIBUTABLE_RATIO.0, ATTRIBUTABLE_RATIO.1,
            )
        } else {
            "kernel-stamped semantic proof; the primary result is emitted only after every probe invariant passes".to_owned()
        };
        println!(
            "\n=== ipcbench (isolated: {probe}) ==={phases}\nisolation check: PASS ({check})\n",
        );
        return Ok(());
    }

    let table = render(run.tsc_khz, &run.results, &run.skipped);
    let derived = render_derived(&run.results);
    let phases = render_phases(&run.phases);
    println!("\n=== ipcbench ===\n{table}{derived}{phases}");

    if let Some(path) = compare {
        let previous = fs::read_to_string(path)
            .with_context(|| format!("read comparison baseline {}", path.display()))?;
        println!("{}", render_comparison(path, &previous, &run.results));
    }

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

    /// Two real runs of this lane, four minutes apart, with a guest change
    /// between them that touches neither `vmexit_cpuid` nor `null_syscall_getpid`.
    const DRIFTED_BASELINE: &str = "\
tsc_overhead                                50000         40         80         80         68        10        20
null_syscall_getpid                         50000       3840       3880       9720       7504       962       972
vmexit_cpuid                                50000       4760       4800       5120       6919      1192      1202
ipc_rt_intra_process                        20000     118160     121720     394400     198472     29604     30496
";

    /// The lane reports ticks of an invariant TSC, and the core clock is not
    /// the TSC. A host that boosts higher finishes the same work in fewer
    /// ticks, and every probe improves at once -- including one with no code of
    /// ours in it. Reading that as a win is the failure this guards.
    #[test]
    fn a_host_clock_shift_is_reported_as_drift_rather_than_as_an_improvement() {
        let results = vec![
            BenchResult {
                name: "tsc_overhead".to_owned(),
                iters: 50_000,
                min: 40,
                p50: 40,
                p99: 80,
                mean: 58,
                min_ns: 10,
                p50_ns: 10,
            },
            BenchResult {
                name: "vmexit_cpuid".to_owned(),
                iters: 50_000,
                min: 3_960,
                p50: 4_040,
                p99: 6_840,
                mean: 9_072,
                min_ns: 992,
                p50_ns: 1_012,
            },
            BenchResult {
                name: "ipc_rt_intra_process".to_owned(),
                iters: 20_000,
                min: 97_680,
                p50: 100_000,
                p99: 200_000,
                mean: 150_000,
                min_ns: 24_474,
                p50_ns: 25_000,
            },
        ];
        let report = render_comparison(Path::new("baseline.txt"), DRIFTED_BASELINE, &results);

        assert!(report.contains("MOVED"), "drift must be stated: {report}");
        // The raw column still says -17%, which is exactly why it must not be
        // the only column: the anchor moved by the same proportion.
        assert!(
            report.contains("-17.3%"),
            "raw delta belongs in the report: {report}"
        );
        // Normalized against the anchor, the round trip did not move.
        assert!(
            report.contains("-0.6%") || report.contains("-0.7%"),
            "normalized delta must show the change was not the guest: {report}"
        );
        // Both runs read 40 ticks for `tsc_overhead`; scaling that would print
        // a 20% regression in the measurement counter itself.
        assert!(
            report.contains("floor"),
            "a probe at the counter granularity must not be normalized: {report}"
        );
        assert!(report.contains("Rerun both sides"), "{report}");
    }

    /// The instrument's own spread was measured at about two percent across
    /// three runs of one unchanged image. A delta inside that is a reading of
    /// the harness, and saying so is the difference between a measurement and
    /// a story about one.
    #[test]
    fn a_delta_inside_the_instrument_spread_is_labelled_noise() {
        let results = vec![
            BenchResult {
                name: "vmexit_cpuid".to_owned(),
                iters: 50_000,
                min: 4_760,
                p50: 4_800,
                p99: 5_120,
                mean: 6_919,
                min_ns: 1_192,
                p50_ns: 1_202,
            },
            // One percent above where the anchor says it should have landed.
            BenchResult {
                name: "ipc_rt_intra_process".to_owned(),
                iters: 20_000,
                min: 119_340,
                p50: 122_000,
                p99: 400_000,
                mean: 200_000,
                min_ns: 29_900,
                p50_ns: 30_500,
            },
        ];
        let baseline = concat!(
            "probe                                       iters    min_cyc\n",
            "vmexit_cpuid                                50000       4760\n",
            "ipc_rt_intra_process                        20000     118160\n",
        );
        let report = render_comparison(Path::new("baseline.txt"), baseline, &results);

        assert!(
            report.contains("1.0% noise"),
            "a sub-spread delta must carry its label: {report}"
        );
    }

    /// A comparison with no anchor is not a comparison. Reporting deltas anyway
    /// would be the same error with the evidence removed.
    #[test]
    fn a_comparison_without_the_anchor_refuses_to_report_deltas() {
        let baseline = "ipc_rt_intra_process 20000 118160 121720 394400 198472 29604 30496\n";
        let results = vec![BenchResult {
            name: "ipc_rt_intra_process".to_owned(),
            iters: 20_000,
            min: 97_680,
            p50: 100_000,
            p99: 200_000,
            mean: 150_000,
            min_ns: 24_474,
            p50_ns: 25_000,
        }];
        let report = render_comparison(Path::new("baseline.txt"), baseline, &results);
        assert!(report.contains("no comparison"), "{report}");
        assert!(
            !report.contains("-17"),
            "no delta may be reported: {report}"
        );
    }

    /// A stable anchor is the case the lane is for, and it must not warn.
    #[test]
    fn a_held_anchor_reports_the_raw_delta_without_a_drift_warning() {
        let results = vec![
            BenchResult {
                name: "vmexit_cpuid".to_owned(),
                iters: 50_000,
                min: 4_800,
                p50: 4_840,
                p99: 5_120,
                mean: 6_919,
                min_ns: 1_202,
                p50_ns: 1_212,
            },
            BenchResult {
                name: "ipc_rt_intra_process".to_owned(),
                iters: 20_000,
                min: 106_344,
                p50: 110_000,
                p99: 200_000,
                mean: 150_000,
                min_ns: 26_646,
                p50_ns: 27_000,
            },
        ];
        let report = render_comparison(Path::new("baseline.txt"), DRIFTED_BASELINE, &results);
        assert!(report.contains("held"), "{report}");
        assert!(!report.contains("MOVED"), "{report}");
        assert!(!report.contains("Rerun both sides"), "{report}");
    }

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
    fn isolated_probe_requires_its_exact_primary_result_without_a_skip() {
        let run = parse_log(SAMPLE).expect("sample parses");
        assert!(isolated_primary_result_holds(&run, "null_syscall_getpid"));
        assert!(!isolated_primary_result_holds(&run, "other"));
        assert!(!isolated_primary_result_holds(&run, "missing"));
    }

    #[test]
    fn semantic_probes_do_not_misattribute_system_wide_ipc_housekeeping() {
        assert!(!requires_phase_attribution(
            "scheduling_budget_exhaust_refill"
        ));
        assert!(!requires_phase_attribution("ipc_nested_passive_server"));
        assert!(requires_phase_attribution("ipc_rt_intra_process"));
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
    fn a_phase_charged_by_more_probes_than_the_round_trip_is_marked_unattributable() {
        // This is the trap the marker exists for. `usermem-phase-bind-visible`
        // is charged by every user copy in the run, so dividing its total by the
        // round-trip count invents a per-call cost it never had. A published
        // figure was wrong by five times before this was rendered.
        let phases = vec![
            PhaseTotal {
                name: String::from("ipc-call-phase-copy-request"),
                cycles: 1_762 * 22_987,
                samples: 22_987,
            },
            PhaseTotal {
                name: String::from("ipc-call-phase-write-response"),
                cycles: 1_618 * 22_958,
                samples: 22_958,
            },
            PhaseTotal {
                name: String::from("usermem-phase-bind-visible"),
                cycles: 677 * 335_989,
                samples: 335_989,
            },
        ];
        let rendered = render_phases(&phases);
        assert!(rendered.contains("ipc-call-phase-write-response"));
        // 22,958 / 22,987 is one per round trip; 335,989 / 22,987 is not.
        assert!(rendered.contains("1.00"), "{rendered}");
        assert!(rendered.contains("(14.62)"), "{rendered}");
        assert!(rendered.contains("shared with other probes"), "{rendered}");
    }

    #[test]
    fn isolation_holds_when_every_phase_divides_cleanly() {
        let phases = vec![
            PhaseTotal {
                name: String::from("ipc-call-phase-copy-request"),
                cycles: 1_762 * 22_987,
                samples: 22_987,
            },
            PhaseTotal {
                name: String::from("ipc-call-phase-write-response"),
                cycles: 1_618 * 22_958,
                samples: 22_958,
            },
        ];
        assert!(isolation_holds(&phases));
    }

    #[test]
    fn isolation_fails_when_a_phase_is_charged_by_more_than_the_round_trip() {
        let phases = vec![
            PhaseTotal {
                name: String::from("ipc-call-phase-copy-request"),
                cycles: 1_762 * 22_987,
                samples: 22_987,
            },
            PhaseTotal {
                name: String::from("usermem-phase-bind-visible"),
                cycles: 677 * 335_989,
                samples: 335_989,
            },
        ];
        assert!(!isolation_holds(&phases));
    }

    #[test]
    fn isolation_holds_vacuously_when_the_probe_charges_no_ipc_call_phase() {
        // Isolating `vmexit_cpuid` charges nothing in either family, so there
        // is no round trip to divide and nothing to contradict isolation.
        assert!(isolation_holds(&[]));
        let phases = vec![PhaseTotal {
            name: String::from("usermem-phase-bind-visible"),
            cycles: 677 * 12,
            samples: 12,
        }];
        assert!(isolation_holds(&phases));
    }

    #[test]
    fn a_run_without_the_unit_phase_claims_no_attribution_at_all() {
        let phases = vec![PhaseTotal {
            name: String::from("usermem-phase-bind-visible"),
            cycles: 677 * 335_989,
            samples: 335_989,
        }];
        let rendered = render_phases(&phases);
        assert!(rendered.contains(" -"), "{rendered}");
        assert!(!rendered.contains("shared with other probes"), "{rendered}");
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
