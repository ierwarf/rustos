# RustOS proof infrastructure

`formal/verify-all.sh --profile pr` is the bounded, risk-weighted merge gate;
`--profile nightly` runs the complete TLC inventory plus independent bug-finding
and proof lanes. A result is
evidence only for the property and scope named below; it is never promoted
into an implementation-wide or certification claim.

| Layer | Tool | What it establishes | What it does not establish |
| --- | --- | --- | --- |
| Concurrent contract | TLC, automatic local workers and fixed fingerprint seed | Every state in the configured finite model preserves its listed invariant | Rust source equivalence, CPU memory ordering, hardware behavior, or identical state-discovery order across worker counts |
| Symbolic refinement pilot | Apalache | Typed bounded SMT exploration of the exact pilot abstraction | Equivalence to the larger TLC model or unbounded safety |
| Inductive model theorem | TLAPS | The stated mathematical theorem; currently endpoint-publication and wait-set terminal-state lemmas | Inductiveness of every model invariant or temporal liveness |
| Rust boundary | Kani `#[kani::proof]` plus mandatory `kani::cover!` witnesses | The indexed harness over every symbolic value within its explicit unwind bounds, with non-vacuous admitted paths | Whole-workspace concurrency, unmodeled I/O, compiler, or hardware correctness |
| Unbounded proof kernel | Verus | The indexed state partition for all values in the named theorem, with a source counterpart and executable test | Equivalence to RustOS source unless a mapped Kani/test gate also exists |
| Rust undefined behavior | Miri | Executed host-test paths avoid the UB classes modeled by the pinned interpreter | Untested paths, kernel target behavior, races, or hardware |
| Synchronization kernel | Loom | Every enumerated interleaving in the registered proof kernels preserves its named source-anchored protocol invariant; `concurrency-witnesses.tsv` binds and hashes each proof and production symbol | Source equivalence outside the mapped algorithms, complete C11 behavior, or unbounded thread counts |
| Controlled schedule exploration | Shuttle PCT | The bounded registry-selected protocol models survive 128 controlled schedules at depth 3; a failing schedule is retained in runner output for deterministic replay | Sound/exhaustive concurrency proof, source equivalence, target-only assembly, or hardware behavior |
| x86_64 ordering litmus | herd7 7.58 plus `x86tso-mixed.cat` | Each registered explicit bad x86_64 outcome is forbidden, while its paired publication-order mutant is reachable | Rust compiler mapping, APIC/DMA/device ordering, other CPU architectures, or complete protocol liveness |
| Parser exploration | Rust libFuzzer plus Clang libFuzzer/ASan/UBSan | Bounded coverage-guided executions do not crash the selected Rust and exact Linux-DVM C parsers | Exhaustiveness, sustained corpus quality, or target-device behavior |
| Instrumented host boundaries | `run-sanitizers.sh` | Every registered critical/high host-testable Rust target passes the pinned address/thread instrumentation profile with a rebuilt matching standard library | Untested target-only assembly, device DMA, or paths outside the registered tests |
| Dual-ABI reference comparison | `run-abi-differential.sh` | Compiled RustOS Linux/Windows constants and layouts equal native Linux and MinGW/Wine probes, except exact unexpired declared divergences | Complete syscall behavior, undocumented platform behavior, or application compatibility |
| Native syscall registry | `check-native-syscall-numbers.py` | Every literal `SYS_RUSTOS_*` declaration in the authoritative ABI modules has the native prefix and a globally unique number | Correct dispatch behavior after a unique number is selected or non-native Linux/Windows syscall tables |
| Recovery scenario matrix | `run-recovery-scenarios.sh` | Every registered checkpoint, service-restart, and storage disruption executes an exact bounded source witness and reaches its declared terminal state | Physical power-cut behavior or unregistered recovery transitions |
| Source trace replay | `run-runtime-traces.sh` | Concrete runtime-control and successful bounded KVM P0 outcomes conform to registered model actions and topology requirements | Production fleet telemetry or every model transition |
| Source decision witnesses | `run-source-conformance.sh` | The exact typed count in `docs/ai/formal-contracts.generated.md` executes mapped high-risk lifecycle, RPC, and IPC decisions; a duplicate, missing, renamed, or filtered witness fails the gate | Full transition-system equivalence, concurrency beyond the tested decision, target hardware, or the other registered models |
| Mutation sensitivity | run-spec-mutations.sh; run-herd.sh; run-implementation-mutations.sh | Each registered TLA+ property/transition mutant is killed by its named invariant and normalized counterexample trace; each herd7 order mutant reaches its exact forbidden outcome; critical/high implementation mutants are injected at a sealed exact source anchor and killed by their one resolved exact witness rather than compile or foreign-target failure | Completeness against every possible mutation, source equivalence, or a production execution of a temporary model mutant |

Exact TLC reuse also reserves five minutes of remaining cache lifetime before a
PR or SMP iteration begins. This prevents a pass admitted at run start from
crossing the 24-hour boundary while Kani, mutation, and concurrency lanes finish
and then failing only at final evidence sealing. The final writer independently
revalidates the exact inputs and actual age; the reserve never widens the reuse
window.

After the registry selftest and one proof-index validation, source conformance,
exact-input TLC, Kani, Verus, mutation, recovery, trace, ABI, and concurrency
lanes execute as independent fail-closed children. Their outputs are replayed
only after every child status is collected. Kani and Verus receive the sealed
proof-index precondition and do not concurrently rewrite its shared summary.
| Signed evidence | `cargo xtask formal-contracts evidence` | A GPG signature binds the current source tree, registry, exact passed/fresh proof summaries, topology runtime trace, and required boot/DVM binaries | Correctness beyond the recorded evidence or evidence after expiry |
| Integration | focused Rust tests and bounded DVM/KVM smoke | Concrete owner wiring and observable regression behavior | Exhaustive state exploration |

## Finding acceptance rule

Classify an item as an implementation bug only when at least one of these is
true:

1. TLC/Kani produces a counterexample and the mapped source transition admits
   that trace;
2. a focused test or bounded runtime reproduction fails the named contract; or
3. source inspection proves that a declared contract has a missing or inverted
   enforcement branch.

Solver timeout, unsupported language feature, coverage gap, a linter warning,
or a model-only trace without a concrete source mapping is a verification gap,
not a bug. Keep it in `CONFORMANCE.md` until it is resolved.

## Tool pins and commands

- TLC is pinned in `tla2tools.lock`; run `bash formal/run-all-tlc.sh`. The PR
  profile selects the 21 critical models declared in `contracts.toml` and has a
  hard 120-second aggregate wall budget; it preserves their model constants
  and fails rather than silently lowering the configured finite state space.
  A PR or SMP-iteration pass is reusable for 24 hours only when the exact TLA/CFG hashes, pinned
  tool, registry deadlock policy, and execution policy match; all misses run
  normally. Their reciprocal profile allow-list permits one exact completed
  result to satisfy the other for shared models, avoiding duplicate exhaustive
  exploration without touching the artifact or widening its age. Nightly
  covers the complete registry without reuse. TLC `-depth`
  controls simulation, not exhaustive graph depth, so it is not used as a
  shortcut. The default is `TLC_WORKERS=auto`; set
  `TLC_WORKERS=1` for serial reproduction.
- Kani is pinned in `kani.lock`; initialize it with `bash formal/setup-kani.sh`,
  then run `bash formal/run-kani.sh`.
- Verus is pinned with archive hash in `verus.lock`; initialize it with
  `bash formal/setup-verus.sh`, then run `bash formal/run-verus.sh`.
- `proof-index.toml` is the closed Kani/Verus retrieval graph. Run
  `bash formal/run-proof-index.sh` before either lane; it hashes every indexed
  source/proof pair and rejects unindexed proof files, stale model anchors,
  absent Kani covers, cyclic dependencies, and Verus trusted shortcuts. Its
  exact policy and non-claims are in `PROOF_INDEX_CONTRACT.md`.
- Apalache and TLAPS archives are version/hash pinned in their lock files.
  Current Apalache pilots cover exec tickets, handle transfer, and wait-set
  check-register-recheck. TLAPS covers endpoint publication and wait-set
  terminal-state lemmas.
- Kani 0.67 does not expose native SARIF. `normalize-kani-results.py` converts
  its human output into stable summary JSON and SARIF, and fails any harness
  with no satisfied cover witness. Failed runs request Kani concrete playback.
- The full merge gate is `bash formal/verify-all.sh --profile pr`. It uses no Kani flags that
  weaken the analysis such as `--ignore-global-asm`. After the source and TLC
  prerequisites pass, independent mutation, differential, recovery, proof,
  concurrency, and trace lanes run concurrently in disjoint evidence
  directories. The parent collects every exact exit status before sealing;
  concurrency changes wall time only and never skips, weakens, or converts a
  failed lane into evidence.
- `bash formal/run-implementation-mutations.sh --check` is the mandatory cheap
  registry preflight. A text that occurs once uses occurrence `N`; a deliberately
  repeated text must select `N/M`, where `M` is the exact current total. The
  runner rejects semantic duplicate rows, seals the resolved byte offset,
  context, and source hash before copying, resolves exactly one fully qualified
  libtest name, and records a kill only when that exact witness executes and
  fails. Compile-only rejection, a different failing Cargo target, timeout before
  witness execution, or source drift is invalid evidence rather than a kill.

Kani stays with bounded parsers, ABI shapes, arithmetic partitions, and narrow
unsafe-adjacent admission code. Function contracts are not credited merely
because Kani accepts assumptions: a future contract use must have an indexed
`proof_for_contract` harness and an independent non-contract caller witness.
Verus is deliberately capped to ten small mathematical kernels. Each must
state the source counterpart and executable test; `admit`, `assume`, axioms,
and external proof bodies are rejected. This prevents either tool from
manufacturing an implementation-wide correctness claim.

The first Rust proof target is `runtime-control::response_payload_len`: a
successful response must echo the request opcode; only snapshots may carry a
bounded payload; malformed status values must fail closed without arithmetic
overflow. The proof is deliberately a narrow host boundary, not a claim that
all socket or scheduler behavior has been proved.

The shared executable-image proof targets are the exact little-endian field
decoder, `admit_image`, `admit_elf64_load_segment`, the PE section admission
helper, and the bounded relocation/import validators. For every symbolic
single-region plan accepted inside the configured process window, the entry is
in bounds and in executable, non-writable memory. Arbitrary ELF load-segment
and PE section bytes preserve their window and W^X contracts. One arbitrary
relocation entry has only an allowed bounded exact effect, and one arbitrary
import thunk has a valid ordinal or bounded name identity. Unit tests, host
fuzzing, and the dual-ABI TLA+ models cover multi-region overlap and lifecycle
integration. These compositional harnesses are not a claim that arbitrary-length
multi-block/multi-descriptor parser executions are exhaustively proved.

The release-blocker models extend that plan gate to bounded byte-parser,
page-table lifecycle, DMA-domain, boot-content, Ethernet-payload, and scheduler
distribution abstractions. Their source correspondence is listed in
`CONFORMANCE.md`. None is hardware proof: in particular, the DMA model cannot
turn the current identity-only kernel backend into an IOMMU implementation,
and the scheduler model cannot replace multicore CPU-time captures.

The DVM proof target is
`driver-domain-protocol::RustosInputFrame`, the shared no_std wire-format
implementation consumed by hostd: every accepted key or relative pointer frame
has an exact RDI1 header, nonzero epoch/sequence, and bounded Linux key/button
fields. The Kani harnesses also prove two fixed, independent wire vectors
(including CRC32) while ordinary Rust tests exercise the shared constructor.
Invalid values are rejected before they reach the guest-facing input transport.
