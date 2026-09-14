# Session Handoff

Volatile resume note; live Git state and fresh tool evidence take precedence.

## Current task

Optimize scheduler/IPC structurally using external references, Serena edits,
disassembly, and repeated real `cargo xtask bench` runs. Targets for the
production-shaped `ipc_rt_intra_process_reply_recv`: 1 CPU p99 <10,000 ticks;
8 CPU p50 <10,000 and p99 <20,000. **Not achieved.** Do not weaken overflow,
SIMD isolation, timing samples, authority checks, or benchmark readiness.

## Current change

Started on clean `b44fe9e8` (receive-or-wait was already committed). Uncommitted:
- `ipc/slab.rs`: advisory hinted allocation retains slot locks, generations,
  bounded full-pool fallback; does not update the shared allocation cursor.
- `ipc/reply_publication.rs`: use kernel-stamped caller task ID for fast replies;
  ordinary replies keep round-robin allocation. No per-caller reserved capacity.
- Five regression witnesses cover hot reuse, stale/exhausted generations, full
  capacity, concurrent authority, and real fast-call lifetime; registered in
  source conformance. Benchmark documentation records evidence and limitations.

## Evidence

`build/bench-ipc-struct/` contains logs, exits, disassemblies and artifact hashes.
71 IPC tests and the selected now lane (30 commands) passed. 8 CPU isolated
control p50/p99 17,680/314,360; successful candidates 15,440/109,280 and
15,960/188,400, min anchors 3,640/3,640/3,680. One candidate boot failed
compositor readiness, and full 8 CPU control timed out. 1 CPU candidate p99
77,640; control anchor drift invalidated attribution. Do not count failed boots
or prior discarded SIMD/compiler experiments as shipping acceptance evidence.
Stable PR validation is the next gate; consult `stable-pr.exit`/`stable-pr.log`
for its actual result, not an assumption from this note.

## Next

Finish stable validation, investigate full-table/readiness failures separately,
and retain only evidence-supported improvements. Global Scheduler serialization
is still present; this is allocation-locality work, not lock-free dispatch.
Do not edit tracked files during sealed formal/KVM runs. Do not stage or commit
without a fresh user request; preserve unrelated changes. Previous `/tmp` logs
were lost across environment reset, so keep summaries in the benchmark contract.
