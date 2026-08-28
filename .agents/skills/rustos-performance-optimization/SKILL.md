---
name: rustos-performance-optimization
description: "Measure and optimize RustOS and native workloads with evidence-led use of perf record, Perfetto, Coz, egglog, and Minotaur. Use for performance investigations and optimization changes; do not use for ordinary functional debugging without a performance question."
---

# RustOS Performance Optimization

Use this skill whenever the user asks to make RustOS, a service, a driver relay,
or a native benchmark faster, or asks for a performance investigation. It is a
measurement and decision workflow, not permission to change source code.

## Tool roles

Select tools by the workload; record the applicability decision in the result.
Do not run every tool blindly and do not treat a tool's output as proof by
itself.

| Tool | Use it for | Do not claim from it |
| --- | --- | --- |
| `perf record` / `perf script` | Sample a Linux host process or native binary and identify hot call paths. Use `-g`/`--call-graph` when unwind data is available. | It cannot resolve code executing inside a non-Linux RustOS guest; `[unknown]` KVM samples are host-side evidence only. |
| `perf stat` | Paired control/variant counters and runtime, instructions, branches, and cache events. | A single counter or one run is not an optimization result. |
| `perfetto` / `tracebox` + `trace_processor` | Multi-process timelines: scheduling, ftrace, host QEMU/DVM activity, and causal ordering. Use bounded traces and query them with PerfettoSQL. | A visual timeline or a provider-reported FPS is not end-to-end RustOS evidence. |
| `coz` | Causal profiling of a native C/C++/Rust binary that has debug information and explicit progress points. Use `coz run ... --- ...`, then `coz plot --text`. | Coz needs progress points and enough samples; it is not a generic flamegraph or a guest profiler. |
| `egglog` | Equality-saturation/Datalog models for algebraic rewrites, fast-path state transitions, or before/after semantic equivalence. Keep the model small and executable. | A proof/model does not measure latency, contention, or real hardware behavior. |
| `minotaur` | LLVM IR or C/C++ compiler experiments where SIMD/code-generation synthesis is applicable. Compare its result against the original and verify the resulting workload. | It is not a direct Rust optimizer and is not applicable to arbitrary RustOS Rust code or guest binaries. |

## Required workflow

1. Define one workload, input, build profile, machine topology, warm-up, and
   metric. Preserve a control and report median plus p95/p99/max where the
   metric supports it.
2. Establish a baseline before changing source. For a native Linux process,
   use a bounded `perf record` capture, for example:

   ```sh
   mkdir -p build/perf
   timeout --foreground 30s perf record -g --call-graph dwarf \
     -o build/perf/control.data -- ./path/to/workload
   perf script -i build/perf/control.data > build/perf/control.script
   ```

   Use `perf report --stdio` only for small captures; `perf script` is the
   fallback when report aggregation is slow or blocked. Never leave a raw
   `perf.data` in the repository root.
3. Add Perfetto when the question is about ordering or interaction across
   processes/CPUs. Prefer a bounded host trace such as:

   ```sh
   tracebox -t 10s -o build/perfetto/control.pftrace sched/sched_switch
   trace_processor query build/perfetto/control.pftrace \
     "SELECT ts, dur, name FROM slice LIMIT 20"
   ```

   If ftrace or perf sampling is permission-restricted, report the exact
   permission failure. Do not silently change `perf_event_paranoid`, tracing
   ownership, or other system security policy.
4. Use Coz only after confirming the binary has debug information and a useful
   throughput or latency progress point. Run the control and candidate with
   the same command and enough duration for a stable causal profile. Preserve
   the text profile and distinguish predicted virtual speedup from measured
   end-to-end runtime.
5. Use egglog when a proposed rewrite or fast path changes a semantic state
   machine or algebraic expression. Encode the old and new result relation,
   run `egglog` on the model, and retain the model/output beside the
   measurement. Follow with differential tests and the real benchmark.
6. Use Minotaur only when the hot region is C/C++ or LLVM IR and the installed
   patched LLVM/Alive2/Z3 toolchain is healthy. Prefer an offline cut-extraction
   pass for broad workloads, then run synthesis intentionally; keep Redis
   local to the experiment. A Minotaur rewrite must still pass semantic tests,
   `perf`/Perfetto measurement, and the normal build gate.
7. Compare control and candidate under the same workload. Reject changes that
   improve only a minimum, a visual trace, a model estimate, or an unbounded
   host-side artifact. State whether the bottleneck moved, stayed, or was not
   measured.

## RustOS-specific boundaries

- For RustOS KVM/DVM work, use guest counters, bounded `cargo xtask kvm-smoke`
  evidence, and the focused debugcon/serial markers as the product evidence.
  Host `perf record` and Perfetto traces are complementary evidence about
  QEMU, KVM, DVM, scheduling, and transport overhead.
- Do not attribute `[unknown]` host KVM samples to a RustOS function without a
  guest-side symbolized profile. Do not call a visual UI result or a model
  result a physical-performance measurement.
- `coz` is useful for a Linux-host service or a native relay only when progress
  points are present. It is not a replacement for RustOS guest counters.
- `minotaur` is appropriate for a C/C++ relay or LLVM IR toolchain experiment,
  not for moving RustOS policy into ring0. Preserve the named user-service
  owner and its ABI contract while optimizing implementation cost.
- If measurement leads to a source edit, load `rustos-code-editing` first and
  pass the Serena, ast-grep, and CodeGraph preflight before editing. After
  source edits run `cargo xtask dev-plan` and execute its selected lanes.

## Failure and reporting

Check availability with `command -v perf perfetto tracebox trace_processor coz
egglog minotaur` before choosing a lane. A missing binary, denied PMU/ftrace
permission, absent debug info, missing Minotaur dependency, or empty progress
profile is a blocker for that lane, not a reason to fabricate success or add
an unrelated fallback. Report the command, exit status, and the next bounded
probe.

Read [references/official-sources.md](references/official-sources.md) when
choosing flags, installation assumptions, or interpreting tool limitations.
