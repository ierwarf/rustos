set -euo pipefail
cd /home/github-runner/rustos-agent-worktree

src_dvm=/home/github-runner/_work/rustos/rustos/driver-domains/linux/out/artifacts
test -r "$src_dvm/rustos-linux-dvm-x86_64.manifest"
mkdir -p driver-domains/linux/out
if [ ! -L driver-domains/linux/out/artifacts ]; then
  if [ -e driver-domains/linux/out/artifacts ]; then
    mv driver-domains/linux/out/artifacts "/tmp/rustos-agent-artifacts-prephase-$$"
  fi
  ln -s "$src_dvm" driver-domains/linux/out/artifacts
fi

cp kernel/ps/src/multitask/irq.rs "/tmp/irq-control-$$.rs"
control="/tmp/irq-control-$$.rs"

python3 - <<'PY'
from pathlib import Path
p=Path('kernel/ps/src/multitask/irq.rs')
s=p.read_text()

old="""static PERIODIC_FAIRNESS_TICKS: [core::sync::atomic::AtomicU8;
    nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { core::sync::atomic::AtomicU8::new(0) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
// Explicit wake/block/handoff requests schedule immediately. A runnable peer
"""
new="""static PERIODIC_FAIRNESS_TICKS: [core::sync::atomic::AtomicU8;
    nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { core::sync::atomic::AtomicU8::new(0) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
// CPU-local epoch ownership for the fairness counter. A new contended epoch is
// phase-staggered once; after its first dispatch every CPU resumes the exact
// PERIODIC_FAIRNESS_INTERVAL_TICKS cadence. Only that CPU's interrupt-disabled
// clockevent path touches its element, so relaxed accesses are sufficient.
static PERIODIC_FAIRNESS_ACTIVE: [AtomicBool; nucleus_core::util::lockdep::MAX_TRACKED_CPUS] =
    [const { AtomicBool::new(false) }; nucleus_core::util::lockdep::MAX_TRACKED_CPUS];
// Explicit wake/block/handoff requests schedule immediately. A runnable peer
"""
if old not in s:
    raise SystemExit('fairness statics anchor missing')
s=s.replace(old,new,1)

anchor="""const PERIODIC_FAIRNESS_INTERVAL_TICKS: u8 = 8;

pub fn timer_interrupt_handler_addr() -> u64 {
"""
insert="""const PERIODIC_FAIRNESS_INTERVAL_TICKS: u8 = 8;

/// Advance one CPU's periodic-fairness epoch. The CPU index only chooses the
/// first deadline inside a newly contended epoch. Once active, every successful
/// fairness dispatch is followed by the same eight-tick budget as before.
const fn advance_periodic_fairness_state(
    logical_index: usize,
    active: bool,
    ticks: u8,
    eligible: bool,
) -> (bool, u8, bool) {
    if !eligible {
        return (false, 0, false);
    }
    let start = if active {
        ticks
    } else {
        (logical_index % PERIODIC_FAIRNESS_INTERVAL_TICKS as usize) as u8
    };
    let elapsed = start.saturating_add(1);
    if elapsed >= PERIODIC_FAIRNESS_INTERVAL_TICKS {
        (true, 0, true)
    } else {
        (false, elapsed, true)
    }
}

#[cfg(test)]
mod periodic_fairness_tests {
    use super::{PERIODIC_FAIRNESS_INTERVAL_TICKS, advance_periodic_fairness_state};

    fn ticks_until_due(cpu: usize, active: &mut bool, ticks: &mut u8) -> usize {
        for turn in 1..=usize::from(PERIODIC_FAIRNESS_INTERVAL_TICKS) {
            let (due, next_ticks, next_active) =
                advance_periodic_fairness_state(cpu, *active, *ticks, true);
            *ticks = next_ticks;
            *active = next_active;
            if due {
                return turn;
            }
        }
        panic!("fairness deadline exceeded the admitted interval");
    }

    #[test]
    fn periodic_fairness_staggers_only_the_first_turn_of_each_contended_epoch() {
        for cpu in 0..usize::from(PERIODIC_FAIRNESS_INTERVAL_TICKS) {
            let mut active = false;
            let mut ticks = 0;
            assert_eq!(
                ticks_until_due(cpu, &mut active, &mut ticks),
                usize::from(PERIODIC_FAIRNESS_INTERVAL_TICKS) - cpu
            );
            assert_eq!(
                ticks_until_due(cpu, &mut active, &mut ticks),
                usize::from(PERIODIC_FAIRNESS_INTERVAL_TICKS),
                "steady cadence changed on cpu {cpu}"
            );

            let (due, next_ticks, next_active) =
                advance_periodic_fairness_state(cpu, active, ticks, false);
            assert!(!due);
            assert_eq!(next_ticks, 0);
            assert!(!next_active);
            active = next_active;
            ticks = next_ticks;
            assert_eq!(
                ticks_until_due(cpu, &mut active, &mut ticks),
                usize::from(PERIODIC_FAIRNESS_INTERVAL_TICKS) - cpu,
                "new epoch did not restore cpu phase {cpu}"
            );
        }
    }
}

pub fn timer_interrupt_handler_addr() -> u64 {
"""
if anchor not in s:
    raise SystemExit('interval anchor missing')
s=s.replace(anchor,insert,1)

old="""    if interrupted_user_frame
        && kernel_mm::api::frame_capability::pager_fault_frame_refill_requested()
    {
        PERIODIC_FAIRNESS_TICKS[logical_index].store(0, Ordering::Relaxed);
        return false;
    }
    let fair_dispatch_due = if local_work_pending
        && !deferred_reschedule_pending
        && !user_return_reschedule_pending
        && interrupted_user_frame
    {
        let elapsed = PERIODIC_FAIRNESS_TICKS[logical_index]
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if elapsed >= PERIODIC_FAIRNESS_INTERVAL_TICKS {
            PERIODIC_FAIRNESS_TICKS[logical_index].store(0, Ordering::Relaxed);
            true
        } else {
            false
        }
    } else {
        PERIODIC_FAIRNESS_TICKS[logical_index].store(0, Ordering::Relaxed);
        false
    };
"""
new="""    if interrupted_user_frame
        && kernel_mm::api::frame_capability::pager_fault_frame_refill_requested()
    {
        PERIODIC_FAIRNESS_TICKS[logical_index].store(0, Ordering::Relaxed);
        PERIODIC_FAIRNESS_ACTIVE[logical_index].store(false, Ordering::Relaxed);
        return false;
    }
    let eligible_for_periodic_fairness = local_work_pending
        && !deferred_reschedule_pending
        && !user_return_reschedule_pending
        && interrupted_user_frame;
    let active = PERIODIC_FAIRNESS_ACTIVE[logical_index].load(Ordering::Relaxed);
    let ticks = PERIODIC_FAIRNESS_TICKS[logical_index].load(Ordering::Relaxed);
    let (fair_dispatch_due, next_ticks, next_active) = advance_periodic_fairness_state(
        logical_index,
        active,
        ticks,
        eligible_for_periodic_fairness,
    );
    PERIODIC_FAIRNESS_TICKS[logical_index].store(next_ticks, Ordering::Relaxed);
    PERIODIC_FAIRNESS_ACTIVE[logical_index].store(next_active, Ordering::Relaxed);
"""
if old not in s:
    raise SystemExit('fairness body anchor missing')
s=s.replace(old,new,1)
p.write_text(s)
PY

rustfmt --edition 2024 kernel/ps/src/multitask/irq.rs
cargo test -q -p kernel-ps periodic_fairness_staggers_only_the_first_turn_of_each_contended_epoch -- --nocapture
git diff --check

cp kernel/ps/src/multitask/irq.rs "/tmp/irq-phase-candidate-$$.rs"
candidate="/tmp/irq-phase-candidate-$$.rs"
probe=ipc_rt_intra_process_reply_recv
out="/tmp/rustos-phase-ab-$$"
mkdir -p "$out"

bench_retry() {
  name=$1
  for n in 1 2; do
    echo "RUN $name attempt=$n"
    if cargo xtask bench --rustos-vcpus 8 --isolate-probe "$probe" >"$out/$name.log" 2>&1; then
      grep -E 'vmexit_cpuid|ipc_rt_intra_process_reply_recv|isolation check' "$out/$name.log" | tail -6
      return 0
    fi
    tail -n 8 "$out/$name.log"
  done
  return 1
}

cp "$control" kernel/ps/src/multitask/irq.rs
bench_retry control1
cp "$candidate" kernel/ps/src/multitask/irq.rs
bench_retry candidate1
bench_retry candidate2
cp "$control" kernel/ps/src/multitask/irq.rs
bench_retry control2
cp "$candidate" kernel/ps/src/multitask/irq.rs

echo '=== phase A/B summary ==='
for f in control1 candidate1 candidate2 control2; do
  printf '%s: ' "$f"
  grep 'ipc_rt_intra_process_reply_recv' "$out/$f.log" | tail -1
  printf '%s anchor: ' "$f"
  grep 'vmexit_cpuid' "$out/$f.log" | tail -1
done
echo '=== diff stat ==='
git diff --stat
echo '=== status ==='
git status --short
