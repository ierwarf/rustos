---
name: rustos-performance-optimization
description: Measure and optimize RustOS or native workloads with evidence-led benchmarks and profiling. Use only for performance investigations or optimization changes.
---

# RustOS Performance Optimization

Measure first. Do not run every profiler by default.

## Workflow

1. Define one workload, input, build profile/topology, warm-up, and metric.
   Preserve a control; report median and tail percentiles when available.
2. Run the existing benchmark/counters first and locate the smallest hot path.
   Use `rg` for exact markers and Serena for symbol/reference-aware navigation.
3. Add a profiler only when it can answer the next question:
   - `perf stat/record`: native Linux process counters/hot call paths.
   - Perfetto/tracebox: cross-process/CPU ordering.
   - Coz: native binary with useful progress points.
   - egglog: small semantic/state-machine rewrite model.
   - Minotaur: applicable C/C++ or LLVM-IR synthesis experiment.
4. If source changes are needed, use `rustos-code-editing`, patch the smallest
   candidate, then run `cargo xtask dev-plan` and its selected lanes.
5. Re-run the same workload. Reject improvements supported only by a minimum,
   visual trace, model estimate, or changed workload.

## RustOS boundaries

For KVM/DVM, guest counters and bounded `cargo xtask kvm-smoke` evidence are the
product evidence. Host perf/Perfetto can explain QEMU/KVM/DVM transport cost but
cannot name guest functions without guest-side symbols. Keep raw profiles/logs
outside model context and return bounded summaries.

Do not use Coz as a guest profiler or Minotaur as a generic Rust optimizer. Do
not change host security policy to make PMU/ftrace work; report the permission
blocker. Read `references/official-sources.md` only when exact flags/tool
limitations are actually needed.
