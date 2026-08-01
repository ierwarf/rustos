# TLA+ mutation-adequacy contract

A successful TLC run establishes only that the configured finite model
satisfies the configured formulas. It does not establish that a formula is
non-vacuous, that a critical guard is represented, or that the formula can
distinguish a nearby unsafe transition. RustOS therefore treats a baseline
TLC pass and a mutation-adequacy pass as separate merge requirements.

The executable corpus is spec-mutations.toml. It uses one exact mutation per
temporary model copy and has these mandatory fields:

| Field | Meaning |
| --- | --- |
| id, kind, severity | Stable sorted identity and a critical/high mutation operator class |
| model, flow | One registered model and the exact model-bindings.tsv flow it refines |
| find, replace, occurrence | An exact, unique source anchor and one bounded syntactic change |
| invariant | The configured invariant that must reject this exact mutant |
| min_counterexample_states | Minimum normalized TLC trace length; a transition fault cannot be accepted as a parser error |

The allowed operator taxonomy is deliberately semantic rather than random:

| Kind | Required fault shape |
| --- | --- |
| property-perturbation | Change a named property to a nearby wrong condition and require TLC to refute it |
| transition-guard-removal | Admit an action without a required ownership, readiness, sequence, or acknowledgement predicate |
| transition-order | Collapse or reverse a required publication/barrier order |
| transition-effect | Omit or corrupt a required state update |
| transition-revocation | Retain authority that an exit/revoke action must remove |

The command bash formal/run-spec-mutations.sh first model-checks each unchanged
model. For every listed mutant it copies the TLA+ model and configuration to a
temporary directory, applies the one registered alteration, and runs the
pinned TLC wrapper. The mutant is **killed** only when all of the following
hold:

1. the baseline passed;
2. TLC failed as an invariant violation, not a parse/type error, timeout, or
   expression-coverage failure;
3. TLC named the registry's invariant; and
4. normalize-tlc-trace.py emitted a counterexample with at least the
   registered state count.

A mutant that passes is a **survivor** and fails the gate. The runner writes
the deterministic corpus hash, source/mutant hashes, the invariant, and the
normalized counterexample trace hash to
build/formal/spec-mutations/summary.json. The named model-to-flow binding is
checked before TLC runs, so reviewers can compare the action-labelled model
trace with the existing source/runtime witness for that flow. A temporary TLA+
mutant is never executed as production RustOS code; it must not be described
as a runtime counterexample.

The corpus is a risk-scoped adequacy floor for SMP ownership, online, IPI,
timer, scheduler, shootdown, cross-CPU lifetime, wait-set, activation, and
interactive boot. It is not a claim that finite mutation testing proves every
property or all Rust/hardware behaviors. Any new critical/high model in one
of these flows must add a corpus entry and, where it introduces a new
transition class, a corresponding semantic mutation before merge.

Use the fast static gate while editing:

    python3 formal/run-spec-mutations.py --check

Use the full evidence gate once the related source set is stable:

    bash formal/run-spec-mutations.sh

After a model or mutation failure, use the scoped repair gate instead:

    bash formal/run-spec-mutations.sh --id <mutation-id>

It runs only that mutation's unchanged baseline and temporary mutant, writes a
scoped result beside that mutation, and never replaces the full-corpus
summary. It is repair evidence, not a substitute for the full PR gate, so
already-passing unrelated mutations are not needlessly re-explored.
