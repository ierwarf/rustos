#!/usr/bin/env bash
set -euo pipefail
cd /home/github-runner/rustos-agent-worktree
bench=tools/xtask/src/bench.rs
irq=kernel/ps/src/multitask/irq.rs
bench_backup=/tmp/bench-retry-150-$$.rs
irq_backup=/tmp/irq-retry-150-$$.rs
cp "$bench" "$bench_backup"
cp "$irq" "$irq_backup"
cleanup() {
  cp "$bench_backup" "$bench" || true
  cp "$irq_backup" "$irq" || true
}
trap cleanup EXIT
python3 - <<'PY'
from pathlib import Path
p=Path('tools/xtask/src/bench.rs')
s=p.read_text()
old='''const fn benchmark_timeout_seconds(rustos_vcpus: u8, isolated: bool) -> u64 {
    if rustos_vcpus > 1 && isolated {
        90
'''
new='''const fn benchmark_timeout_seconds(rustos_vcpus: u8, isolated: bool) -> u64 {
    if rustos_vcpus > 1 && isolated {
        150
'''
if old not in s:
    raise SystemExit('benchmark timeout anchor missing')
p.write_text(s.replace(old,new,1))
PY
git diff --check -- "$bench"
git show 7ba17f27a832e88fcb338e59522f05d16f84ec5c:agent-results/phase-job.sh > /tmp/rustos-phase-original-$$.sh
bash /tmp/rustos-phase-original-$$.sh
