//! Exact, fail-closed UI-consumption -> observed DVM-completion collection.
//! Not a physical scanout, application key-echo, Linux parity or shipping gate.

use super::{HARDWARE_ANCHOR, core_paths, field, parse_log, payload_lines};
use crate::config::Config;
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeSet;

const BEGIN: &str = "uiserver: input-completion BEGIN v=1 samples=128 warmup=16 endpoint=consume-to-observed-dvm-completion";
const END: &str = "uiserver: input-completion END samples=128 failed=0";
const PREFIX: &str = "uiserver: input-completion sample ";
const COUNT: usize = 128;

#[derive(Debug)]
struct Sample {
    first: u64,
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
                "events", "x", "y",
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
            samples.push(Sample {
                first,
                submit,
                done,
                drains,
                events,
            });
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

pub(super) fn bench(config: &Config, vcpus: u8) -> Result<()> {
    crate::build::build(config, false)?;
    let capture = core_paths::Capture::begin(config, core_paths::contract::GUI, vcpus)?;
    let args = [
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
    let anchor = parse_log(&log)?;
    ensure!(
        anchor.skipped.is_empty()
            && anchor.results.len() == 1
            && anchor.results[0].name == HARDWARE_ANCHOR
            && anchor.results[0].iters == 50_000,
        "GUI collection requires exactly the genuine CPUID anchor, without skipped probes"
    );
    let samples = parse(&log)?;
    println!(
        "GUI endpoint=cursor-moving-UI-drain-to-observed-validated-DVM-completion (poll delay included; not physical scanout)"
    );
    println!(
        "hardware anchor p50_ticks={} p99_ticks={} (collected separately after isolate settle)",
        anchor.results[0].p50, anchor.results[0].p99
    );
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
            log.push_str(&format!("{PREFIX}id={id} first=10 last=11 submit=20 done=30 gen=1 epoch={} surface=7 drains=2 events=3 x=4 y=5\n", id+1));
        }
        log.push_str(END);
        log
    }
    #[test]
    fn complete_exact_receipts_are_required_not_only_a_success_sentinel() {
        let log = fixture();
        assert_eq!(parse(&log).unwrap().len(), COUNT);
        assert!(parse(&log.replace("id=1 first=", "id=0 first=")).is_err());
        assert!(parse(&log.replace("done=30", "done=19")).is_err());
        assert!(parse(&log.replace("epoch=2 ", "epoch=1 ")).is_err());
        assert!(parse(&log.replace("gen=1 epoch=2 ", "gen=2 epoch=2 ")).is_err());
        assert!(parse(&log.replace("drains=2", "drains=0")).is_err());
        assert!(parse(&log.replace("events=3", "events=1")).is_err());
        assert!(parse(&log.replace(" x=4", " x=4 x=4")).is_err());
        assert!(parse(&log.replace("id=1 first=10 last=11 submit=20 done=30 gen=1 epoch=2 surface=7 drains=2 events=3 x=4 y=5\n", "")).is_err());
        assert!(parse(&log.replace(END, "")).is_err());
        assert!(
            parse(&(log + "\nuiserver: input-completion FAIL completed=128 failed=1")).is_err()
        );
    }
}
