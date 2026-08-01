# Pre-QEMU concurrency triangle

concurrency-triangle.toml is the closed registry for bounded concurrent
protocols that must be checked before QEMU is used as a diagnostic oracle. It
does not treat a passing test as a generic SMP proof. Every row instead binds
one critical/high system flow, one TLA+ model, one concrete Rust source symbol,
the exact ordering anchors in that symbol, one Loom proof kernel, and one
Shuttle schedule test. A row whose decision is an ISA-visible lock-free
publication also binds an x86_64 herd7 litmus and one order-reversal mutant.

## Admission and bounded execution

formal/check-concurrency-triangle.py fails before any runner if a source
symbol, ordering anchor, system-flow/model/source tuple, test function,
architecture, model, pin, or required non-applicability rationale is missing.
Rows are sorted and unique. Thus renaming an atomic ordering, moving a
protocol to another owner, or deleting the litmus cannot silently preserve
old evidence.

The default PR budget is deliberately finite:

| Lane | Default | Bound | Failure evidence |
| --- | --- | --- | --- |
| Loom | 200 branch bound | 1..=10,000 | smallest enumerated interleaving that violates a model assertion |
| Shuttle PCT | 128 schedules, depth 3 | 16..=2,048 schedules; depth 1..=4; 30 s/scenario | controlled failing schedule, reproducible through Shuttle replay |
| herd7 | x86_64 x86tso-mixed.cat, 10 s/litmus | 1..=60 s/litmus | exact forbidden-state report and the corresponding mutant result |

The bounds are guardrails, not proof-strength labels. An override outside
those ranges fails the runner rather than creating an unreviewable long test
or a nearly empty smoke test.

## Mutant sensitivity

TLC alone can accept a vacuous invariant. formal/run-spec-mutations.sh
therefore applies each registered TLA+ property/transition mutant in an
isolated copy and requires its named invariant and normalized counterexample
trace to fail. This follows the specification-mutation distinction used by
model-checking test generation and the recent TLA-Prover evaluation: a checker
pass is insufficient when a small property alteration also passes.

The herd7 lane applies the same rule at the memory-order boundary. The
baseline uses an explicit bad outcome and must report Never, with zero positive
witnesses. Its paired mutant reverses the protocol's publication order and
must report Sometimes, with a positive witness. A baseline that passes but a
mutant that is still Never is rejected as a vacuous or mis-specified litmus.
This is not a claim that the temporary assembly mutant is production Rust
code; it proves only that the exact stated bad outcome is sensitive to the
ordering edge being modeled.

## Scope and non-claims

Loom exhaustively explores the small Rust synchronization abstractions in
loom-proof-kernel; it is not a complete C11 implementation. Shuttle scales the
same source-anchored protocols through controlled PCT schedules but is
probabilistic, so a pass is bug-finding evidence rather than a sound proof.
herd7 enumerates the explicit x86_64 assembly litmus executions under the
pinned x86 TSO cat model; it does not prove Rust-to-assembly compilation,
device DMA ordering, APIC delivery, or a non-x86 target. The source anchors,
TLA+ flow contracts, and later target KVM/QEMU evidence close different gaps;
none may be substituted for another.

An ISA litmus is intentionally not required for mutex/identity protocols such
as endpoint revocation, IPC terminal ownership, and generic wait-set arming.
Their registry rows name why an ISA-only reduction would erase the actual
invariant. This prevents a green but irrelevant herd7 test from being counted
as coverage.

## Tool pin and operation

herdtools.lock pins official herdtools7 release 7.58, its source archive, and
the matching Ubuntu 7.58-1 amd64 package SHA-256. setup-herdtools.sh verifies
and extracts that package below build/formal/tools/; it never elevates
privileges or performs a global install. run-herd.sh accepts only the pinned
version, uses the tool-reported library directory, and accepts only
x86tso-mixed.cat.

Run the complete pre-QEMU gate with the following two commands after the
documented OCaml prerequisites are present:

    bash formal/setup-herdtools.sh
    bash formal/run-concurrency-triangle.sh

formal/verify-all.sh --profile pr invokes the same triangle before runtime
trace/KVM evidence and seals build/formal/concurrency-triangle/summary.json.

## References

- TLA-Prover evaluation levels and mutation-sensitive Diamond criterion:
  <https://arxiv.org/abs/2606.06133>
- NIST, model checking with specification mutation:
  <https://www.nist.gov/publications/test-generation-using-model-checking-and-specification-mutation>
- Loom's stated scheduler/model scope:
  <https://github.com/tokio-rs/loom>
- Shuttle schedulers, PCT depth, bounds, and replay:
  <https://docs.rs/shuttle/0.9.1/shuttle/>
- herdtools7 installation/pinning guidance:
  <https://diy.inria.fr/sources/index.html>
- herd/cat execution semantics:
  <https://diy.inria.fr/tuto/mem/index.html>
- Linux litmus-test discipline, especially adapting a close existing pattern:
  <https://docs.kernel.org/dev-tools/lkmm/docs/litmus-tests.html>
