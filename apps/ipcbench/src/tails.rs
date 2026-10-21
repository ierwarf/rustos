//! Owner: ipcbench; raw tail evidence, never a scheduling policy or filter.
//! Lifecycle: summarize all validated samples; defer output until measurement ends.
//! Boundary: paired stamps share one invariant-TSC domain, not host CPU time.
//! Concurrency: the client owns each bounded top-eight set; deferred text is locked.
//! Failure: an unpaired outlier remains in the total distribution and witness set.
//! Evidence: quantile, overflow, cutoff and paired-outlier native regressions.
use std::sync::Mutex;

static DEFERRED: Mutex<Vec<String>> = Mutex::new(Vec::new());
const WITNESSES: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Summary {
    p999: u64,
    p9999: u64,
    over80k: usize,
    over1m: usize,
    over10m: usize,
    top1pct_ppm: u64,
}

impl Summary {
    pub(crate) fn from_sorted(samples: &[u64]) -> Self {
        assert!(!samples.is_empty());
        debug_assert!(samples.windows(2).all(|pair| pair[0] <= pair[1]));
        let len = samples.len();
        let pick = |n: usize, d: usize| samples[(len - 1).min(len / d * n + len % d * n / d)];
        let over = |limit| len - samples.partition_point(|value| *value <= limit);
        let sum: u128 = samples.iter().map(|value| u128::from(*value)).sum();
        let top: u128 = samples[len - len.div_ceil(100)..]
            .iter()
            .map(|value| u128::from(*value))
            .sum();
        Self {
            p999: pick(999, 1_000),
            p9999: pick(9_999, 10_000),
            over80k: over(80_000),
            over1m: over(1_000_000),
            over10m: over(10_000_000),
            top1pct_ppm: if sum == 0 {
                0
            } else {
                (top * 1_000_000 / sum) as u64
            },
        }
    }

    pub(crate) fn defer(self, name: &str, iters: usize) {
        DEFERRED.lock().unwrap().push(format!(
            "ipcbench: tail v=1 name={name} iters={iters} p999={} p9999={} over80k={} over1m={} over10m={} top1pct_ppm={}",
            self.p999, self.p9999, self.over80k, self.over1m, self.over10m, self.top1pct_ppm,
        ));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Sample {
    pub(crate) seq: u64,
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) parts: Option<[u64; 3]>,
}
impl Sample {
    fn cycles(self) -> u64 {
        self.end.wrapping_sub(self.start)
    }
}

pub(crate) struct Worst {
    samples: [Option<Sample>; WITNESSES],
}
impl Worst {
    pub(crate) const fn new() -> Self {
        Self {
            samples: [None; WITNESSES],
        }
    }

    // Outside the timed call: O(1) rejection, bounded insertion, no allocation,
    // additional clock read, syscall, console output, or removal from the sample set.
    pub(crate) fn observe(&mut self, sample: Sample) {
        if self.samples[WITNESSES - 1].is_some_and(|last| sample.cycles() <= last.cycles()) {
            return;
        }
        let index = self
            .samples
            .iter()
            .position(|old| old.is_none_or(|old| sample.cycles() > old.cycles()))
            .unwrap_or(WITNESSES - 1);
        self.samples.copy_within(index..WITNESSES - 1, index + 1);
        self.samples[index] = Some(sample);
    }

    pub(crate) fn defer(self, name: &str) {
        let mut output = DEFERRED.lock().unwrap();
        for (index, sample) in self.samples.into_iter().flatten().enumerate() {
            let [to_recv, body, to_return] = sample.parts.unwrap_or([0; 3]);
            output.push(format!(
                "ipcbench: tail-sample v=1 name={name} rank={} seq={} cycles={} paired={} call_to_recv={to_recv} server_body={body} reply_to_return={to_return} start_tsc={} end_tsc={}",
                index + 1, sample.seq, sample.cycles(), u8::from(sample.parts.is_some()), sample.start, sample.end,
            ));
        }
    }
}

pub(crate) fn flush() {
    // Never print while a measured probe or the deferred-output mutex is held.
    let lines = std::mem::take(&mut *DEFERRED.lock().unwrap());
    for line in lines {
        crate::debug_line(&line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_keep_the_existing_order_statistic_convention() {
        let values: Vec<u64> = (0..20_000).collect();
        let summary = Summary::from_sorted(&values);
        assert_eq!((summary.p999, summary.p9999), (19_980, 19_998));
        assert_eq!(summary.over80k, 0);
        assert_eq!(Summary::from_sorted(&[7]).p9999, 7);
    }

    #[test]
    fn strict_raw_cycle_cutoffs_and_top_share_keep_all_outliers() {
        let summary =
            Summary::from_sorted(&[80_000, 80_001, 1_000_000, 1_000_001, 10_000_000, 10_000_001]);
        assert_eq!(
            (summary.over80k, summary.over1m, summary.over10m),
            (5, 3, 1)
        );
        assert_eq!(summary.p9999, 10_000_001);
        let wide = Summary::from_sorted(&[u64::MAX; 20_000]);
        assert_eq!(wide.top1pct_ppm, 10_000);
        assert_eq!(Summary::from_sorted(&[0; 200]).top1pct_ppm, 0);
    }

    #[test]
    fn worst_set_keeps_exact_correlated_parts_and_unpaired_outliers() {
        let mut worst = Worst::new();
        for cycles in 1..=20 {
            worst.observe(Sample {
                seq: cycles,
                start: 100,
                end: 100 + cycles,
                parts: Some([1, 0, cycles - 1]),
            });
        }
        worst.observe(Sample {
            seq: 21,
            start: 100,
            end: 1_000,
            parts: None,
        });
        assert_eq!(worst.samples[0].unwrap().parts, None);
        let kept: Vec<_> = worst.samples.into_iter().flatten().collect();
        assert_eq!(
            kept.iter().map(|s| s.seq).collect::<Vec<_>>(),
            vec![21, 20, 19, 18, 17, 16, 15, 14]
        );
        for sample in &kept[1..] {
            assert_eq!(sample.parts.unwrap().iter().sum::<u64>(), sample.cycles());
        }
    }
}
