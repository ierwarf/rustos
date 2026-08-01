# Proof-index contract

`proof-index.toml` is the closed, machine-checked retrieval graph for the
small set of Rust proof kernels that complement the TLA+ and concurrency
triangle gates. It solves a traceability problem: before a Kani or Verus result
can be used at all, the repository must identify the production source symbol,
the registered formal model, the exact harness or theorem, an executable
counterpart where applicable, its dependency edges, its tool lock, and its
bounded scope. It is not a proof generator, an LLM retrieval system, or a
claim that RustOS or an entire subsystem is verified.

## Closed-world rules

Every `proof` record is sorted by a unique ID and has one production `source`,
`symbol`, `formal_model`, and scope. The model must be registered in
`models.tsv`. Dependencies must name records in the same file, be acyclic, and
stay at most eight edges deep.

For Kani, the index package set must equal `run-kani.sh`'s package set. Every
named harness must be a `#[kani::proof]` function in the named source and must
contain its own `kani::cover!`; a passing assertion with no admitted path is
not evidence. Kani remains for finite parser/ABI/arithmetic and narrow
state-machine partitions. It does not model general SMP scheduling or prove
unbounded progress. If a Kani function contract is later added, it needs both
an indexed `proof_for_contract` harness and an independent ordinary-caller
test; contract assumptions alone are never credited.

For Verus, every `formal/verus-proof-kernel/*.rs` file must be indexed, and the
index cannot exceed ten files. Each listed lemma must exist, name a production
source counterpart and focused executable test, and run under the pinned
Verus release with a 60-second wall limit and solver `--rlimit 150`. The
checker rejects `admit`, `assume`, axioms, and Verus external bodies in all
registered proof files. A Verus result proves only its explicitly written
mathematical theorem; it does not prove the Rust implementation, compiler
lowering, device ordering, APIC delivery, or code that lacks an index entry.

`run-proof-index.sh` first executes the checker, then emits
`build/formal/proof-index/summary.json`. That record hashes the index plus
every selected source and Verus file and is mandatory PR/nightly evidence. The
Kani and Verus summaries each also carry the same index hash, preventing a
mixed proof/index result from being sealed.

## Adding a proof

1. Start with a demonstrated high-risk source invariant and its registered
   TLA+ model; do not add a theorem only because it is easy to prove.
2. Write or identify a focused executable source test that can refute the
   production rule. For Kani, add a local cover witness. For Verus, write the
   smallest theorem that carries the unbounded part Kani cannot cover.
3. Add the complete sorted index record and any explicit dependency. State the
   boundary in `scope`; never overstate it as source equivalence.
4. Run `bash formal/run-proof-index.sh`, the relevant Kani or Verus runner,
   then the PR seal. A counterexample is triaged against the production source;
   an unsupported feature, solver timeout, or unindexed route is a coverage
   gap rather than a fabricated pass.

The workflow is inspired by dependency-aware proof retrieval research, but it
never imports generated proofs or accepts automated repair without the same
closed-world checks and executable evidence.
