//! Exact, fail-closed UI-consumption -> observed DVM-completion collection.
//! Not a physical scanout, application key-echo, Linux parity or shipping gate.

use super::{BenchResult, ParsedRun, core_paths, field, parse_log, payload_lines};
use crate::config::Config;
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeSet;

const BEGIN: &str = "uiserver: input-completion BEGIN v=2 samples=128 warmup=16 endpoint=consume-to-observed-dvm-completion";
const END: &str = "uiserver: input-completion END samples=128 failed=0";
const PREFIX: &str = "uiserver: input-completion sample ";
const COUNT: usize = 128;

#[derive(Debug)]
struct Sample {
    first: u64,
    last: u64,
    start: u64,
    copy: u64,
    ioctl: u64,
    cpu_ns: u64,
    kind: u64,
    pixels: u64,
    submit: u64,
    done: u64,
    drains: u64,
    events: u64,
}

fn parse(log: &str) -> Result<Vec<Sample>> {
    let mut begun = false;
    let mut ended = false;
    let mut samples = Vec::new();
    let mut identities = BTreeSet::new();
    let mut generation = None;
    let mut previous_epoch = 0;
    let mut previous_done = 0;
    for line in payload_lines(log) {
        if !line.starts_with("uiserver: input-completion ") {
            continue;
        }
        if line == BEGIN {
            ensure!(!begun && !ended, "duplicate GUI begin");
            begun = true;
        } else if line == END {
            ensure!(
                begun && !ended && samples.len() == COUNT,
                "incomplete/duplicate GUI end"
            );
            ended = true;
        } else if let Some(body) = line.strip_prefix(PREFIX) {
            ensure!(
                begun && !ended && samples.len() < COUNT,
                "GUI sample outside batch"
            );
            let keys = body
                .split_whitespace()
                .map(|token| token.split_once('=').map(|v| v.0))
                .collect::<Option<Vec<_>>>()
                .context("malformed GUI sample fields")?;
            let expected = [
                "id", "first", "last", "submit", "done", "gen", "epoch", "surface", "drains",
                "events", "x", "y", "start", "copy", "ioctl", "cpu_ns", "k", "px",
            ];
            ensure!(
                keys == expected,
                "duplicate, missing or unknown GUI sample field"
            );
            let get = |key: &str| field(body, key).with_context(|| format!("invalid GUI {key}"));
            let id = get("id=")?;
            let first = get("first=")?;
            let last = get("last=")?;
            let submit = get("submit=")?;
            let done = get("done=")?;
            let provider = get("gen=")?;
            let epoch = get("epoch=")?;
            let surface = get("surface=")?;
            let drains = get("drains=")?;
            let events = get("events=")?;
            ensure!(
                id == samples.len() as u64,
                "missing/duplicate/out-of-order GUI sample"
            );
            ensure!(
                first <= last && last <= submit && submit <= done && done >= previous_done,
                "reversed GUI consumption/submission/completion stamps"
            );
            ensure!(
                drains != 0 && events >= drains,
                "GUI sample has no real input drain"
            );
            ensure!(
                provider != 0 && epoch > previous_epoch && surface <= u32::MAX as u64,
                "invalid/stale GUI frame identity"
            );
            ensure!(
                get("x=")? <= u32::MAX as u64 && get("y=")? <= u32::MAX as u64,
                "invalid GUI cursor coordinates"
            );
            ensure!(
                generation.is_none_or(|old| old == provider),
                "GUI provider changed during batch"
            );
            ensure!(
                identities.insert((provider, epoch, surface)),
                "duplicate GUI completion identity"
            );
            generation = Some(provider);
            previous_epoch = epoch;
            previous_done = done;
            let start = get("start=")?;
            let copy = get("copy=")?;
            let ioctl = get("ioctl=")?;
            let cpu_ns = get("cpu_ns=")?;
            let kind = get("k=")?;
            let pixels = get("px=")?;
            ensure!(kind < 128, "unknown GUI work kind");
            ensure!(last <= start && start <= copy && copy <= ioctl && ioctl <= submit,
                "reversed GUI work stamps");
            samples.push(Sample { first, last, start, copy, ioctl, cpu_ns, kind, pixels, submit, done, drains, events });
        } else {
            bail!("failed, clipped or unknown GUI benchmark record: {line}");
        }
    }
    ensure!(
        begun && ended && samples.len() == COUNT,
        "missing complete GUI receipt batch"
    );
    Ok(samples)
}

fn distribution(name: &str, mut values: Vec<u64>) -> String {
    values.sort_unstable();
    let p50 = values[values.len() / 2];
    let p99 = values[(values.len() * 99 / 100).min(values.len() - 1)];
    let mean = values.iter().map(|&v| u128::from(v)).sum::<u128>() / values.len() as u128;
    format!(
        "{name}: samples={} p50_us={p50} p99_us={p99} mean_us={mean} max_us={} p99/p50={:.3} {}\n",
        values.len(),
        values[values.len() - 1],
        p99 as f64 / p50.max(1) as f64,
        if p50 > 0 && u128::from(p99) * 2 <= u128::from(p50) * 3 {
            "PASS"
        } else {
            "FAIL"
        }
    )
}

fn validate_anchors(run: &ParsedRun) -> Result<[&BenchResult; 3]> {
    let expected = core_paths::contract::ISOLATED_HARDWARE_ANCHORS;
    ensure!(
        run.skipped.is_empty() && run.results.len() == expected.len(),
        "GUI collection requires exactly {} hardware anchor results, without skips (got {} results, {} skips)",
        expected.len(),
        run.results.len(),
        run.skipped.len()
    );
    let require = |(name, iters): (&str, usize)| {
        run.results
            .iter()
            .find(|row| row.name == name && row.iters == iters as u64)
            .with_context(|| format!("missing GUI hardware anchor {name} with {iters} samples"))
    };
    let [cpuid, alu, atomic] = expected;
    Ok([require(cpuid)?, require(alu)?, require(atomic)?])
}

pub(super) fn bench(config: &Config, vcpus: u8) -> Result<()> {
    crate::build::build(config, false)?;
    let capture = core_paths::Capture::begin(config, core_paths::contract::GUI, vcpus)?;
    let args = [
        // Boot the exact signed image whose linked/staged ELFs were just captured.
        "--no-build",
        "--gui-dvm-surfaces",
        "--dvm-network-shmem",
        "--dvm-block-shmem",
        "--exercise-input",
        "--timeout",
        "120",
        "--rustos-vcpus",
        &vcpus.to_string(),
        "--ipcbench-probe",
        core_paths::contract::GUI,
        "--expect",
        super::END_MARKER,
        "--expect",
        END,
    ]
    .map(str::to_owned);
    let launch = crate::kvm::kvm_benchmark_command(config, args.into_iter());
    let log_path = config.build_dir.join("kvm/rustos-debugcon.log");
    let _ = crate::kvm::archive_run_log(&log_path);
    capture.finish(config, &log_path, launch.is_ok())?;
    launch.context("collect bounded GUI completion samples and the independent ipcbench anchor")?;
    let log = std::fs::read_to_string(&log_path)?;
    let run = parse_log(&log)?;
    let anchors = validate_anchors(&run)?;
    let samples = parse(&log)?;
    println!(
        "GUI endpoint=cursor-moving-UI-drain-to-observed-validated-DVM-completion (poll delay included; not physical scanout)"
    );
    for anchor in anchors {
        println!(
            "hardware anchor name={} p50_ticks={} p99_ticks={} (collected separately after isolate settle)",
            anchor.name, anchor.p50, anchor.p99
        );
    }
    print!(
        "{}{}{}",
        distribution(
            "consume_to_completion",
            samples.iter().map(|s| s.done - s.first).collect()
        ),
        distribution(
            "consume_to_submit",
            samples.iter().map(|s| s.submit - s.first).collect()
        ),
        distribution(
            "submit_to_completion",
            samples.iter().map(|s| s.done - s.submit).collect()
        )
    );
    for (name, pick) in [
        ("consume_to_work_start", (|s: &Sample| s.start - s.first) as fn(&Sample) -> u64),
        ("latest_drain_to_work_start", |s| s.start - s.last),
        ("accepted_submit_work_wall", |s| s.submit - s.start),
        ("accepted_submit_thread_cpu", |s| s.cpu_ns / 1000),
        ("scene_prep_wall", |s| s.copy - s.start),
        ("copy_wall", |s| s.ioctl - s.copy),
        ("submit_call_wall", |s| s.submit - s.ioctl),
    ] {
        print!("{}", distribution(name, samples.iter().map(pick).collect()));
    }
    let mut cpu_ns: Vec<_> = samples.iter().map(|sample| sample.cpu_ns).collect();
    cpu_ns.sort_unstable();
    let p99_ns = cpu_ns[COUNT * 99 / 100];
    let slowest = samples.iter().max_by_key(|s| s.cpu_ns).expect("validated sample batch");
    const TARGET_NS: u64 = 1_000_000;
    println!(
        "UI CPU 1ms target: p99_ns={p99_ns} max_ns={} over_1ms={}/{} {}",
        slowest.cpu_ns,
        cpu_ns.iter().filter(|&&ns| ns > TARGET_NS).count(),
        COUNT,
        if p99_ns <= TARGET_NS && slowest.cpu_ns <= TARGET_NS { "PASS" } else { "FAIL" }
    );
    println!("slowest UI CPU frame: kind={} upload_pixels={} prep_us={} copy_us={} ioctl_us={}",
        slowest.kind, slowest.pixels, slowest.copy - slowest.start,
        slowest.ioctl - slowest.copy, slowest.submit - slowest.ioctl);
    println!(
        "input accounting: moving_drains={} pointer_events={} accepted_input_frames={COUNT}; drains may be coalesced into the latest cursor scene",
        samples.iter().map(|s| s.drains).sum::<u64>(),
        samples.iter().map(|s| s.events).sum::<u64>()
    );
    println!(
        "GUI core-path collection COMPLETE (not Linux parity, shipping or tail-target acceptance)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> String {
        let mut log = format!("{BEGIN}\n");
        for id in 0..COUNT {
            log.push_str(&format!("{PREFIX}id={id} first=10 last=11 submit=20 done=30 gen=1 epoch={} surface=7 drains=2 events=3 x=4 y=5 start=15 copy=16 ioctl=18 cpu_ns=4900 k=0 px=0\n", id+1));
        }
        log.push_str(END);
        log
    }
    #[test]
    fn only_the_complete_hardware_anchor_triplet_is_admitted() {
        let result = |name, iters| {
            format!("ipcbench: result name={name} iters={iters} min=100 p50=200 p99=300 max=400 mean=210 min_ns=25 p50_ns=50\n")
        };
        let cpuid = result("vmexit_cpuid", 50_000);
        let alu = result("anchor_alu_chain", 10_000);
        let atomic = result("anchor_atomic_chain", 10_000);
        let check = |rows: String| {
            let log = format!("ipcbench: begin\nipcbench: tsc_khz=4000000\n{rows}ipcbench: end\n");
            parse_log(&log).and_then(|run| validate_anchors(&run).map(|_| ()))
        };
        let exact = format!("{cpuid}{alu}{atomic}");
        assert!(check(exact.clone()).is_ok());
        assert!(check(cpuid.clone()).is_err());
        assert!(check(exact.replace("iters=10000", "iters=9999")).is_err());
        assert!(check(exact.replace("anchor_atomic_chain", "unrelated_probe")).is_err());
        assert!(check(format!("{cpuid}{alu}{alu}")).is_err());
        assert!(check(exact.clone() + &result("unrelated_probe", 1)).is_err());
        assert!(check(exact + "ipcbench: skip name=unrelated_probe reason=failed\n").is_err());
    }

    #[test]
    fn complete_exact_receipts_are_required_not_only_a_success_sentinel() {
        let log = fixture();
        assert_eq!(parse(&log).unwrap().len(), COUNT);
        assert!(parse(&log.replace("id=1 first=", "id=0 first=")).is_err());
        assert!(parse(&log.replace("done=30", "done=19")).is_err());
        assert!(parse(&log.replace("copy=16", "copy=14")).is_err());
        assert!(parse(&log.replace(" cpu_ns=4900", "")).is_err());
        assert!(parse(&log.replace("cpu_ns=4900", "cpu_ns=invalid")).is_err());
        assert!(parse(&log.replace("epoch=2 ", "epoch=1 ")).is_err());
        assert!(parse(&log.replace("gen=1 epoch=2 ", "gen=2 epoch=2 ")).is_err());
        assert!(parse(&log.replace("drains=2", "drains=0")).is_err());
        assert!(parse(&log.replace("events=3", "events=1")).is_err());
        assert!(parse(&log.replace(" x=4", " x=4 x=4")).is_err());
        assert!(parse(&log.replace("id=1 first=10 last=11 submit=20 done=30 gen=1 epoch=2 surface=7 drains=2 events=3 x=4 y=5 start=15 copy=16 ioctl=18 cpu_ns=4900 k=0 px=0\n", "")).is_err());
        assert!(parse(&log.replace(END, "")).is_err());
        assert!(
            parse(&(log + "\nuiserver: input-completion FAIL completed=128 failed=1")).is_err()
        );
    }
}
