//! Owner: host benchmark consumer; optional, versioned raw-tail evidence.
//! Boundary: reject malformed, duplicate, cross-probe or clipped distributions.
//! Lifecycle: parse only within the run; join to the complete primary result.
//! Failure: legacy logs may omit tails; present evidence must validate exactly.
//! Paired witnesses locate a slow leg, not host descheduling or kernel causality.
use super::BenchResult;
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;

pub(super) const PREFIX: &str = "ipcbench: tail ";
pub(super) const SAMPLE_PREFIX: &str = "ipcbench: tail-sample ";

#[derive(Default)]
pub(super) struct Data {
    summaries: Vec<Summary>,
    samples: Vec<Sample>,
}
struct Summary {
    name: String,
    iters: u64,
    p999: u64,
    p9999: u64,
    over80k: u64,
    over1m: u64,
    over10m: u64,
    top1pct_ppm: u64,
}
struct Sample {
    name: String,
    rank: u64,
    seq: u64,
    cycles: u64,
    paired: bool,
    parts: [u64; 3],
    start: u64,
    end: u64,
}
fn fields(text: &str, count: usize) -> Result<BTreeMap<&str, &str>> {
    let mut out = BTreeMap::new();
    for token in text.split_whitespace() {
        let (key, value) = token.split_once('=').context("malformed tail field")?;
        ensure!(
            !value.is_empty() && out.insert(key, value).is_none(),
            "duplicate or empty tail field"
        );
    }
    ensure!(
        out.len() == count && number(&out, "v")? == 1,
        "unsupported tail record"
    );
    Ok(out)
}
fn number(fields: &BTreeMap<&str, &str>, key: &str) -> Result<u64> {
    fields
        .get(key)
        .context("missing tail field")?
        .parse()
        .context("invalid tail number")
}
fn name(fields: &BTreeMap<&str, &str>) -> Result<String> {
    Ok(fields
        .get("name")
        .context("missing tail probe")?
        .to_string())
}
impl Data {
    pub(super) fn record(&mut self, line: &str) -> Result<()> {
        if let Some(text) = line.strip_prefix(PREFIX) {
            let f = fields(text, 9)?;
            let summary = Summary {
                name: name(&f)?,
                iters: number(&f, "iters")?,
                p999: number(&f, "p999")?,
                p9999: number(&f, "p9999")?,
                over80k: number(&f, "over80k")?,
                over1m: number(&f, "over1m")?,
                over10m: number(&f, "over10m")?,
                top1pct_ppm: number(&f, "top1pct_ppm")?,
            };
            ensure!(
                !self.summaries.iter().any(|old| old.name == summary.name),
                "duplicate tail summary"
            );
            self.summaries.push(summary);
        } else {
            let text = line
                .strip_prefix(SAMPLE_PREFIX)
                .context("unknown tail record")?;
            let f = fields(text, 11)?;
            let paired = number(&f, "paired")?;
            ensure!(paired <= 1, "invalid paired tail flag");
            self.samples.push(Sample {
                name: name(&f)?,
                rank: number(&f, "rank")?,
                seq: number(&f, "seq")?,
                cycles: number(&f, "cycles")?,
                paired: paired == 1,
                parts: [
                    number(&f, "call_to_recv")?,
                    number(&f, "server_body")?,
                    number(&f, "reply_to_return")?,
                ],
                start: number(&f, "start_tsc")?,
                end: number(&f, "end_tsc")?,
            });
        }
        Ok(())
    }
    pub(super) fn validate(&self, results: &[BenchResult]) -> Result<()> {
        for tail in &self.summaries {
            let result = results
                .iter()
                .find(|result| result.name == tail.name)
                .context("tail without primary result")?;
            ensure!(
                tail.iters == result.iters
                    && result.p99 <= tail.p999
                    && tail.p999 <= tail.p9999
                    && tail.p9999 <= result.max,
                "tail quantiles/sample count disagree with primary"
            );
            for (limit, count) in [
                (80_000, tail.over80k),
                (1_000_000, tail.over1m),
                (10_000_000, tail.over10m),
            ] {
                ensure!(
                    (result.max > limit) == (count != 0)
                        && (result.min > limit) == (count == tail.iters),
                    "tail threshold count contradicts primary bounds"
                );
            }
            ensure!(
                (result.max == 0 && tail.top1pct_ppm == 0)
                    || (result.max != 0 && tail.top1pct_ppm >= 10_000),
                "tail time share contradicts nonnegative samples"
            );
            ensure!(
                tail.over10m <= tail.over1m
                    && tail.over1m <= tail.over80k
                    && tail.over80k <= tail.iters
                    && tail.top1pct_ppm <= 1_000_000,
                "invalid raw tail counts/share"
            );
        }
        for tail in &self.summaries {
            let result = results
                .iter()
                .find(|result| result.name == tail.name)
                .unwrap();
            let samples: Vec<_> = self
                .samples
                .iter()
                .filter(|sample| sample.name == tail.name)
                .collect();
            ensure!(
                samples.len() <= 8 && samples.len() as u64 <= tail.iters,
                "unbounded tail witness set"
            );
            for (index, sample) in samples.iter().enumerate() {
                ensure!(
                    sample.rank == index as u64 + 1
                        && sample.end.wrapping_sub(sample.start) == sample.cycles,
                    "invalid tail rank or timestamp span"
                );
                ensure!(
                    sample.cycles <= result.max && (index != 0 || sample.cycles == result.max),
                    "tail witness dropped the true maximum"
                );
                ensure!(
                    index == 0 || sample.cycles <= samples[index - 1].cycles,
                    "unordered tail witnesses"
                );
                ensure!(
                    !samples[..index].iter().any(|old| old.seq == sample.seq),
                    "duplicate tail sample sequence"
                );
                if sample.paired {
                    let sum = sample
                        .parts
                        .iter()
                        .try_fold(0_u64, |sum, value| sum.checked_add(*value));
                    ensure!(
                        sum == Some(sample.cycles),
                        "paired tail parts do not sum to exact sample"
                    );
                } else {
                    ensure!(
                        sample.parts == [0; 3],
                        "unpaired tail cannot claim leg attribution"
                    );
                }
            }
        }
        ensure!(
            self.samples
                .iter()
                .all(|sample| self.summaries.iter().any(|tail| tail.name == sample.name)),
            "tail witness without summary"
        );
        Ok(())
    }
    pub(super) fn render(&self) -> String {
        if self.summaries.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "\nraw tail evidence (cycles; all samples retained; top1%=share of total time):\n",
        );
        out.push_str(&format!(
            "{:<40} {:>10} {:>10} {:>8} {:>8} {:>8} {:>9}\n",
            "probe", "p99.9", "p99.99", ">80k", ">1M", ">10M", "top1%"
        ));
        for tail in &self.summaries {
            out.push_str(&format!(
                "{:<40} {:>10} {:>10} {:>8} {:>8} {:>8} {:>8.2}%\n",
                format!("tail/{}", tail.name),
                tail.p999,
                tail.p9999,
                tail.over80k,
                tail.over1m,
                tail.over10m,
                tail.top1pct_ppm as f64 / 10_000.0
            ));
        }
        if !self.samples.is_empty() {
            out.push_str(
                "\npaired worst samples (same request; not independent leg percentiles):\n",
            );
            out.push_str(&format!(
                "{:<40} {:>4} {:>8} {:>10} {:>10} {:>10} {:>10}\n",
                "probe", "rank", "seq", "total", "to_recv", "body", "to_return"
            ));
            for sample in &self.samples {
                out.push_str(&format!(
                    "{:<40} {:>4} {:>8} {:>10} {:>10} {:>10} {:>10}{}\n",
                    format!("worst/{}", sample.name),
                    sample.rank,
                    sample.seq,
                    sample.cycles,
                    sample.parts[0],
                    sample.parts[1],
                    sample.parts[2],
                    if sample.paired {
                        ""
                    } else {
                        " UNPAIRED (total retained)"
                    }
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SUMMARY: &str = "ipcbench: tail v=1 name=ipc_rt_test iters=20000 p999=14000 p9999=15000 over80k=0 over1m=0 over10m=0 top1pct_ppm=40000";
    const SAMPLE: &str = "ipcbench: tail-sample v=1 name=ipc_rt_test rank=1 seq=42 cycles=15000 paired=1 call_to_recv=8000 server_body=100 reply_to_return=6900 start_tsc=100 end_tsc=15100";
    fn primary() -> BenchResult {
        BenchResult {
            name: "ipc_rt_test".into(),
            iters: 20000,
            min: 100,
            p50: 1000,
            p99: 12000,
            max: 15000,
            mean: 2000,
            min_ns: 25,
            p50_ns: 250,
        }
    }
    #[test]
    fn complete_tail_evidence_is_bound_to_the_exact_primary_and_request() {
        let mut data = Data::default();
        data.record(SUMMARY).unwrap();
        data.record(SAMPLE).unwrap();
        data.validate(&[primary()]).unwrap();
        assert!(data.render().contains("p99.9"));
        let baseline = format!(
            "{}{}",
            super::super::render(4_000_000, &[primary()], &[]),
            data.render()
        );
        assert_eq!(
            super::super::parse_baseline_minimums(&baseline),
            vec![("ipc_rt_test".into(), 100)]
        );
        Data::default().validate(&[primary()]).unwrap(); // old logs remain readable
    }
    #[test]
    fn tail_records_are_accepted_only_inside_the_complete_run() {
        let start = "ipcbench: begin\nipcbench: tsc_khz=4000000\n";
        let result = "ipcbench: result name=ipc_rt_test iters=20000 min=100 p50=1000 p99=12000 max=15000 mean=2000 min_ns=25 p50_ns=250\n";
        let log = format!("{start}{result}{SUMMARY}\n{SAMPLE}\nipcbench: end\n");
        assert!(super::super::parse_log(&log).is_ok());
        assert!(
            super::super::parse_log(&format!("{SUMMARY}\n{start}{result}ipcbench: end\n")).is_err()
        );
        assert!(
            super::super::parse_log(&format!("{start}{result}ipcbench: end\n{SUMMARY}\n")).is_err()
        );
    }

    #[test]
    fn bad_quantiles_counts_versions_or_duplicate_summaries_are_rejected() {
        for (from, to) in [
            ("p999=14000", "p999=11000"),
            ("p9999=15000", "p9999=15001"),
            ("iters=20000", "iters=19999"),
            ("over1m=0", "over1m=1"),
            ("top1pct_ppm=40000", "top1pct_ppm=1000001"),
            ("v=1", "v=2"),
        ] {
            let mut data = Data::default();
            assert!(
                data.record(&SUMMARY.replace(from, to))
                    .and_then(|_| data.validate(&[primary()]))
                    .is_err()
            );
        }
        let mut data = Data::default();
        data.record(SUMMARY).unwrap();
        assert!(data.record(SUMMARY).is_err());
        assert!(data.record(&format!("{SAMPLE} rank=2")).is_err());
    }
    #[test]
    fn clipped_maximum_wrong_leg_sum_foreign_probe_or_duplicate_request_are_rejected() {
        for (from, to) in [
            ("cycles=15000", "cycles=14999"),
            ("server_body=100", "server_body=99"),
            ("name=ipc_rt_test", "name=foreign"),
            ("end_tsc=15100", "end_tsc=15101"),
            ("rank=1", "rank=2"),
            ("paired=1", "paired=0"),
        ] {
            let mut data = Data::default();
            data.record(SUMMARY).unwrap();
            assert!(
                data.record(&SAMPLE.replace(from, to))
                    .and_then(|_| data.validate(&[primary()]))
                    .is_err()
            );
        }
        let mut data = Data::default();
        data.record(SUMMARY).unwrap();
        data.record(SAMPLE).unwrap();
        data.record(&SAMPLE.replace("rank=1", "rank=2")).unwrap();
        assert!(data.validate(&[primary()]).is_err());
    }
}
