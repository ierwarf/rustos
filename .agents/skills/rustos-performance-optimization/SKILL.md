---
name: rustos-performance-optimization
description: Measure and optimize RustOS or native workloads with evidence-led benchmarks and profiling. Use only for performance investigations or optimization changes.
---

# RustOS Performance Optimization

Measure first. Prefer existing benchmarks and counters over adding tools.

## Workflow

1. Fix one workload, topology/profile, warm-up, and metric; preserve a control.
2. Run the existing benchmark/counters and locate the smallest hot path. Use
   `rg` for exact markers and Serena for symbol/reference-aware navigation.
3. Add profiling only when it answers a specific unresolved question. For a
   native host process, `perf stat/record` is the default. Use a system trace
   such as Perfetto only when cross-process/CPU ordering is the missing fact.
4. If source changes are needed, use `rustos-code-editing`, patch the smallest
   candidate, then run `cargo xtask dev-plan` and its selected lanes.
5. Re-run the same workload. Reject improvements supported only by a minimum,
   visual trace, model estimate, or changed workload.

For KVM/DVM, guest counters and bounded RustOS benchmark/KVM evidence are the
product evidence; host profiling explains host/QEMU transport only unless guest
symbols are explicitly available. Keep raw profiles/logs outside model context
and return bounded summaries. Do not install extra profilers or change host
security policy merely because a tool might be useful; report the missing
capability when it blocks the next measurement.
