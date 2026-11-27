//! Owner: ipcbench; raw tail evidence, never a scheduling policy or filter.
//! Lifecycle: summarize all validated samples; defer output until measurement ends.
//! Boundary: paired stamps share one invariant-TSC domain, not host CPU time.
//! Concurrency: the client owns each bounded top-eight set; deferred text is locked.
//! Failure: an unpaired outlier remains in the total distribution and witness set.
//! Evidence: quantile, overflow, cutoff and paired-outlier native regressions.
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

static DEFERRED: Mutex<Vec<String>> = Mutex::new(Vec::new());
const WITNESSES: usize = 8;
const SEQ_CAPACITY: usize = 20_000;
const SEQ_OUTLIER_LIMIT: usize = 16;

const fn option_str_eq_true(value: Option<&str>) -> bool {
    match value {
        Some(s) => matches!(s.as_bytes(), b"true"),
        None => false,
    }
}

pub(crate) const PHASE_PROFILE_ENABLED: bool =
    option_str_eq_true(option_env!("RUSTOS_IPC_PHASE_PROFILE"));

fn rdtscp_supported() -> bool {
    static CACHED: AtomicU8 = AtomicU8::new(0);
    match CACHED.load(Ordering::Relaxed) {
        1 => return false,
        2 => return true,
        _ => {}
    }
    #[cfg(target_arch = "x86_64")]
    let supported = {
        let max_ext = core::arch::x86_64::__cpuid(0x8000_0000).eax;
        max_ext >= 0x8000_0001
            && (core::arch::x86_64::__cpuid(0x8000_0001).edx & (1 << 27)) != 0
    };
    #[cfg(not(target_arch = "x86_64"))]
    let supported = false;
    CACHED.store(if supported { 2 } else { 1 }, Ordering::Relaxed);
    supported
}

/// Fast guest CPU helper using `__rdtscp(&mut aux)` when CPUID `0x80000001`
/// EDX bit 27 confirms RDTSCP support, returning 0 otherwise.
#[inline(always)]
pub(crate) fn current_guest_cpu() -> u32 {
    #[cfg(target_arch = "x86_64")]
    if rdtscp_supported() {
        let mut aux = 0_u32;
        let _ = unsafe { core::arch::x86_64::__rdtscp(&mut aux) };
        return aux;
    }
    0
}

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

    pub(crate) fn merge_from(&mut self, other: Worst) {
        for sample in other.samples.into_iter().flatten() {
            self.observe(sample);
        }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SeqSample {
    pub(crate) run: u32,
    pub(crate) seq: u64,
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) to_recv: u64,
    pub(crate) body: u64,
    pub(crate) to_return: u64,
    pub(crate) guest_cpu: u32,
    pub(crate) paired: bool,
}

impl SeqSample {
    pub(crate) fn from_parts(
        run: u32,
        seq: u64,
        start: u64,
        end: u64,
        parts: Option<[u64; 3]>,
        guest_cpu: u32,
    ) -> Self {
        let (paired, [to_recv, body, to_return]) = match parts {
            Some(parts) => (true, parts),
            None => (false, [0; 3]),
        };
        Self {
            run,
            seq,
            start,
            end,
            to_recv,
            body,
            to_return,
            guest_cpu,
            paired,
        }
    }

    fn cycles(self) -> u64 {
        self.end.wrapping_sub(self.start)
    }
}

/// Bounded pre-allocated sequential tail capture ring. Disabled instances
/// perform no allocation and no recording work inside measured loops.
pub(crate) struct SeqRing {
    enabled: bool,
    capacity: usize,
    cursor: usize,
    paired_count: usize,
    unpaired_count: usize,
    overflow_count: usize,
    samples: Vec<SeqSample>,
}

impl SeqRing {
    pub(crate) fn new(explicit_enable: bool) -> Self {
        Self::with_capacity(PHASE_PROFILE_ENABLED || explicit_enable, SEQ_CAPACITY)
    }

    pub(crate) fn with_capacity(enabled: bool, capacity: usize) -> Self {
        if enabled {
            // Warm the CPUID check before the timed loop so sampling never
            // triggers a hypervisor CPUID exit.
            let _ = rdtscp_supported();
            Self {
                enabled: true,
                capacity,
                cursor: 0,
                paired_count: 0,
                unpaired_count: 0,
                overflow_count: 0,
                samples: Vec::with_capacity(capacity),
            }
        } else {
            Self {
                enabled: false,
                capacity: 0,
                cursor: 0,
                paired_count: 0,
                unpaired_count: 0,
                overflow_count: 0,
                samples: Vec::new(),
            }
        }
    }

    #[inline(always)]
    pub(crate) fn observe(&mut self, run: u32, sample: Sample, guest_cpu: u32) {
        if !self.enabled {
            return;
        }
        let entry = SeqSample::from_parts(
            run,
            sample.seq,
            sample.start,
            sample.end,
            sample.parts,
            guest_cpu,
        );
        self.push_entry(entry, true);
    }

    #[inline(always)]
    fn push_entry(&mut self, entry: SeqSample, count_pairing: bool) {
        if count_pairing {
            if entry.paired {
                self.paired_count += 1;
            } else {
                self.unpaired_count += 1;
            }
        }
        if self.samples.len() < self.capacity {
            self.samples.push(entry);
        } else if self.capacity > 0 {
            let idx = self.cursor;
            self.samples[idx] = entry;
            self.cursor = if idx + 1 == self.capacity { 0 } else { idx + 1 };
            self.overflow_count += 1;
        } else {
            self.overflow_count += 1;
        }
    }

    pub(crate) fn merge_from(&mut self, mut other: SeqRing) {
        if !self.enabled || !other.enabled {
            return;
        }
        other.normalize_order();
        self.paired_count += other.paired_count;
        self.unpaired_count += other.unpaired_count;
        self.overflow_count += other.overflow_count;
        for entry in other.samples {
            self.push_entry(entry, false);
        }
    }

    fn normalize_order(&mut self) {
        if self.overflow_count > 0 && self.cursor > 0 && self.samples.len() == self.capacity {
            self.samples.rotate_left(self.cursor);
            self.cursor = 0;
        }
    }

    fn cpu_switches(&self) -> usize {
        self.samples
            .windows(2)
            .filter(|pair| pair[0].run == pair[1].run && pair[0].guest_cpu != pair[1].guest_cpu)
            .count()
    }

    fn select_outlier_neighborhoods(&self) -> Vec<SeqSample> {
        let len = self.samples.len();
        if len == 0 {
            return Vec::new();
        }
        let mut order: Vec<usize> = (0..len).collect();
        order.sort_unstable_by(|&a, &b| {
            self.samples[b]
                .cycles()
                .cmp(&self.samples[a].cycles())
                .then_with(|| a.cmp(&b))
        });
        let mut covered = vec![false; len];
        let mut selected = Vec::with_capacity(SEQ_OUTLIER_LIMIT.min(len));
        for &peak in &order {
            if selected.len() >= SEQ_OUTLIER_LIMIT {
                break;
            }
            if covered[peak] {
                continue;
            }
            let run = self.samples[peak].run;
            let left = if peak > 0 && self.samples[peak - 1].run == run {
                peak - 1
            } else {
                peak
            };
            let right = if peak + 1 < len && self.samples[peak + 1].run == run {
                peak + 1
            } else {
                peak
            };
            let remaining = SEQ_OUTLIER_LIMIT - selected.len();
            let mut cluster = [0usize; 3];
            let mut count = 0usize;
            if remaining >= 3 {
                for idx in left..=right {
                    if !covered[idx] {
                        cluster[count] = idx;
                        count += 1;
                    }
                }
            } else if remaining == 2 {
                if left != peak && !covered[left] {
                    cluster[count] = left;
                    count += 1;
                }
                cluster[count] = peak;
                count += 1;
                if count < 2 && right != peak && !covered[right] {
                    cluster[count] = right;
                    count += 1;
                }
            } else {
                cluster[count] = peak;
                count += 1;
            }
            for &idx in &cluster[..count] {
                covered[idx] = true;
                selected.push(self.samples[idx]);
            }
        }
        selected
    }

    pub(crate) fn defer(mut self, name: &str) {
        if !self.enabled {
            return;
        }
        self.normalize_order();
        let recorded = self.samples.len();
        let cpu_switches = self.cpu_switches();
        let selected = self.select_outlier_neighborhoods();
        let mut output = DEFERRED.lock().unwrap();
        output.push(format!(
            "ipcbench: tail-seq-summary v=1 name={name} recorded={recorded} paired={} unpaired={} overflow={} cpu_switches={cpu_switches}",
            self.paired_count, self.unpaired_count, self.overflow_count,
        ));
        for (index, sample) in selected.into_iter().enumerate() {
            output.push(format!(
                "ipcbench: tail-seq v=1 name={name} rank={} seq={} cycles={} paired={} call_to_recv={} server_body={} reply_to_return={} cpu={} start_tsc={} end_tsc={}",
                index + 1,
                sample.seq,
                sample.cycles(),
                u8::from(sample.paired),
                sample.to_recv,
                sample.body,
                sample.to_return,
                sample.guest_cpu,
                sample.start,
                sample.end,
            ));
        }
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

    #[test]
    fn seq_ring_disabled_captures_nothing_and_emits_no_records() {
        let mut ring = SeqRing::with_capacity(false, 64);
        ring.observe(
            0,
            Sample {
                seq: 1,
                start: 100,
                end: 200,
                parts: Some([40, 20, 40]),
            },
            3,
        );
        assert_eq!(ring.samples.len(), 0);
        assert_eq!(ring.paired_count, 0);
        assert_eq!(ring.unpaired_count, 0);
        assert_eq!(ring.overflow_count, 0);
    }

    #[test]
    fn seq_ring_counts_unpaired_overflow_cpu_switches_and_outlier_clusters() {
        let mut ring = SeqRing::with_capacity(true, 6);
        for i in 1..=8u64 {
            let cycles = match i {
                5 => 50_000,
                6 => 40_000,
                _ => 100,
            };
            let parts = if i == 6 {
                None
            } else {
                Some([cycles / 2, 0, cycles - cycles / 2])
            };
            ring.observe(
                0,
                Sample {
                    seq: i,
                    start: i * 1_000,
                    end: i * 1_000 + cycles,
                    parts,
                },
                if i >= 5 { 2 } else { 0 },
            );
        }
        ring.normalize_order();
        assert_eq!(ring.samples.len(), 6);
        assert_eq!(ring.paired_count, 7);
        assert_eq!(ring.unpaired_count, 1);
        assert_eq!(ring.overflow_count, 2);
        // Retained samples are seq 3..=8: cpu switches once between seq 4 (cpu 0) and seq 5 (cpu 2).
        assert_eq!(ring.cpu_switches(), 1);
        let kept: Vec<_> = ring
            .samples
            .iter()
            .map(|s| s.seq)
            .collect();
        assert_eq!(kept, vec![3, 4, 5, 6, 7, 8]);
        let selected = ring.select_outlier_neighborhoods();
        assert!(selected.len() <= SEQ_OUTLIER_LIMIT);
        assert_eq!(selected[0].seq, 4);
        assert_eq!(selected[1].seq, 5);
        assert_eq!(selected[2].seq, 6);
    }

    #[test]
    fn seq_ring_respects_run_boundaries_for_cpu_switches_and_clusters() {
        let mut a = SeqRing::with_capacity(true, 4);
        let mut b = SeqRing::with_capacity(true, 4);
        for i in 1..=3u64 {
            a.observe(
                0,
                Sample {
                    seq: i,
                    start: 100,
                    end: if i == 3 { 10_100 } else { 200 },
                    parts: Some([50, 0, if i == 3 { 9_950 } else { 50 }]),
                },
                0,
            );
            b.observe(
                1,
                Sample {
                    seq: (1 << 48) | i,
                    start: 100,
                    end: 200,
                    parts: Some([50, 0, 50]),
                },
                3,
            );
        }
        let mut merged = SeqRing::with_capacity(true, 8);
        merged.merge_from(a);
        merged.merge_from(b);
        // Different runs across the merge boundary must not be counted as a CPU switch.
        assert_eq!(merged.cpu_switches(), 0);
        assert_eq!(merged.paired_count, 6);
        let selected = merged.select_outlier_neighborhoods();
        // Peak is seq=3 (run=0); its neighborhood must not spill into seq=(1<<48)|1 (run=1).
        assert_eq!(selected[0].seq, 2);
        assert_eq!(selected[1].seq, 3);
        assert_eq!(selected[0].run, 0);
        assert_eq!(selected[1].run, 0);
    }
}
