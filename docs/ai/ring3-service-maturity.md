# Ring3 service maturity audit — 2026-10-22

Baseline: `d252dc5e`. This is a bounded critical-path source audit, not proof
that every line, feature, or runtime interleaving of every service is complete.
The current iteration prioritizes correctness and failed-path completion over
benchmark tuning. No kernel source or public wire ABI is changed.

## Reviewed ownership paths

| Services | Paths examined | Current correction |
| --- | --- | --- |
| rootd | bootstrap, readiness/restarts, lifecycle drain, lease admission/revocation | supervise core replacements after bootstrap on the original readiness cadence |
| initd | startup/activation, endpoint barriers, lease reconciliation, reaping | preserve the per-executable failure budget across successful replacements |
| runtimed | launch worker, recording/activation, reaping, socket/console custody | isolate each launch ACK; retain the created PID on timeout |
| procd | ingress, exec ticket cancellation, fork/wait/thread plans, signal/exit cleanup | no change; binary tests are disabled, so a normal Cargo test is not handler coverage |
| sessiond | launch/watch/retry/dependency flow | no change |
| vfsd | namespace ingress, checkpoint/cursor settlement, snapshot workers/cache lifetime | request-owned descriptor lease and captured generation; cache-hit byte limits and duplicate-publication handling |
| syscalld | ingress/lifecycle, credentials, mmap/mprotect/munmap policy | persist a completed fixed-range removal before replacement can fail |
| netd | ingress/deadlines, deferred queues, fd replay, Unix ancillary/listener custody | terminal error reply for empty dequeued requests; production receive-turn regression seam |
| loaderd | wire authorization, snapshot cleanup, map/commit/abort, reply cleanup | no change in this iteration |
| storaged | readiness generations, geometry, read-ahead, completion/cancel, writes/flush | declared block count must match payload before any write/cache side effect |
| devmgrd | publication, bounded ioctl workers, authority/refusal, handle cleanup | no change |
| pagerd | admission/range edits, restart/replay, cache revocation/reclaim | no change; anonymous resolver/cache machinery is not live backed-fault dispatch |
| inputd | pressure, provider/session reset, batch retries, grants, malformed replies | apply events and reset/revoke barriers in decoded order; retain exact unacknowledged suffix |
| uiserver | surface generations/cleanup, GPU recovery/completion, input handoff, Wayland cleanup | no change; detailed rendering/font/terminal behavior not exhaustively reviewed |

## Regression evidence and limits

Integrated native suites for the eight modified services passed **254 tests**:
initd 18, inputd 32, netd 39, runtimed 62, storaged 18, rootd 31 (host-test),
syscalld 19 and vfsd 35 (library targets). These are actual nonignored tests.
Initd's exhaustion test invokes the production fail-closed path in a subprocess
and checks exit status 111. Runtime ACK tests exercise the real worker channel;
input tests use the real decoder, queue and reset implementation. Netd's receive
turn uses injected clock/dispatch/reply operations; storaged's write admission
uses a side-effect spy. Vfsd/syscalld witnesses live in executable libraries,
not their test-disabled binaries.

Additional native targets passed loaderd 5, pagerd 30, sessiond 2 and uiserver 77:
**368 passing tests total** across these runs. Devmgrd's native target compiled
but ran zero tests; that is not handler coverage. Procd has no enabled native
handler test target. These two gaps remain explicit.

Source review of all eight fixes found no introduced correctness defect. The
strengthened netd/storaged regression seams also passed a separate follow-up review.
Executed validation: the selected now lane passed 456 checks; the stable
`verify-all --profile pr` gate passed; bounded 8-vCPU KVM smoke passed with
SMP iteration, GUI DVM surfaces and input exercise. Evidence is in
`/tmp/ring3-now.log`, `/tmp/ring3-stable-1.log` and `/tmp/ring3-smoke.log`.
These execution results are separate from source-review verdicts. Do not infer
a pass from a plan, test enumeration, or a zero-test Cargo result.

Open boundaries:

- Native spies do not validate live provider restart, cancellation, transport
  loss, saturation, remount races, or physical hot-unplug end to end.
- Vfsd cache-accounting and optional cache-admission refusal lack a direct
  native orchestration witness; descriptor ownership itself is covered.
- No comprehensive procd handler test suite was added. No new pipe/eventfd,
  writable filesystem, or backed-fault subsystem was implemented.
- Deliberate unsupported operations are not represented as completed features.
- No benchmark improvement, commercial recovery readiness, Linux parity, or
  complete Ring3 maturity is claimed by this audit.
