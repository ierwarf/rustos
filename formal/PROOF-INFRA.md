# RustOS proof infrastructure

`formal/verify-all.sh` is the PR-sized formal gate. It runs pinned TLC models
and pinned Kani proof harnesses. A result is evidence only for the property and
scope named below; it is never promoted into an implementation-wide claim.

| Layer | Tool | What it establishes | What it does not establish |
| --- | --- | --- | --- |
| Concurrent contract | TLC, one worker and fixed seed | Every state in the configured finite model preserves its listed invariant | Rust source equivalence, CPU memory ordering, or hardware behavior |
| Inductive model theorem | TLAPS when a model carries a checked proof | The stated mathematical safety theorem | Temporal liveness not expressed by the checked theorem |
| Rust boundary | Kani `#[kani::proof]` | The named harness over every symbolic value within its explicit unwind bounds | Whole-workspace concurrency, unmodeled I/O, compiler, or hardware correctness |
| Unbounded proof kernel | Verus | The named state partition for all values in the Verus theorem | Equivalence to RustOS source unless a mapped Kani/test gate also exists |
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

- TLC is pinned in `tla2tools.lock`; run `bash formal/run-all-tlc.sh`.
- Kani is pinned in `kani.lock`; initialize it with `bash formal/setup-kani.sh`,
  then run `bash formal/run-kani.sh`.
- Verus is pinned with archive hash in `verus.lock`; initialize it with
  `bash formal/setup-verus.sh`, then run `bash formal/run-verus.sh`.
- The full gate is `bash formal/verify-all.sh`. It uses no Kani flags that
  weaken the analysis such as `--ignore-global-asm`.

The first Rust proof target is `runtime-control::response_payload_len`: a
successful response must echo the request opcode; only snapshots may carry a
bounded payload; malformed status values must fail closed without arithmetic
overflow. The proof is deliberately a narrow host boundary, not a claim that
all socket or scheduler behavior has been proved.

The DVM proof target is
`driver-domain-protocol::RustosInputFrame`, the shared no_std wire-format
implementation consumed by hostd: every accepted key or relative pointer frame
has an exact RDI1 header, nonzero epoch/sequence, and bounded Linux key/button
fields. The Kani harnesses also prove two fixed, independent wire vectors
(including CRC32) while ordinary Rust tests exercise the shared constructor.
Invalid values are rejected before they reach the guest-facing input transport.
