# Performance tool references

These are the primary sources used for this skill. Read only the source that
matches the current tool or measurement question. Installation state on this
machine is user-local under `/home/hongii2/.local` and is not part of the
RustOS product image.

## egglog

- [egglog README](https://github.com/egraphs-good/egglog/blob/main/README.md)
- [egglog documentation](https://egraphs-good.github.io/egglog/)

The README documents the Cargo install path, input-file/REPL modes, and `-j`
parallel execution. Use egglog as a compact semantic model; do not use it as a
runtime profiler.

## Minotaur

- [Minotaur README](https://github.com/minotaur-toolkit/minotaur/blob/dev/README.md)
- [Minotaur paper](https://arxiv.org/abs/2306.00229)

The project is an LLVM pass and C/C++ compiler wrapper. The upstream build
requires a patched LLVM, Alive2, Z3, `re2c`, hiredis, and a Redis cache. The
paper explains the LLVM-MCA/uOp cost filter and formal verification boundary.
Keep Minotaur's toolchain isolated from the system `clang`/`opt`.

## Linux perf

- [perf record manual](https://man7.org/linux/man-pages/man1/perf-record.1.html)
- [Linux perf security](https://www.kernel.org/doc/html/latest/admin-guide/perf-security.html)

Use bounded per-process captures, choose a call-graph unwinder supported by
the binary, and treat PMU/ftrace permissions as an explicit security boundary.

## Perfetto

- [Perfetto CLI reference](https://perfetto.dev/docs/reference/perfetto-cli)
- [Linux/system tracing guide](https://perfetto.dev/docs/getting-started/system-tracing)
- [Linux cookbook](https://perfetto.dev/docs/getting-started/linux-cookbook)
- [Trace Processor command-line analysis](https://perfetto.dev/docs/getting-started/command-line-analysis)

On Linux, the upstream `tracebox` download bundles the tracing services and
the `perfetto` client. `trace_processor` is the host-side SQL/conversion tool.
Ftrace and perf-based data sources may require elevated privileges.

## Coz

- [Coz official repository and usage](https://github.com/plasma-umass/coz)
- [Coz causal-profiling paper](https://arxiv.org/abs/1608.03676)

Coz profiles native C/C++/Rust programs through virtual speedup experiments.
It needs debug information and progress points; use it to prioritize a
candidate, then validate the change with the same benchmark and independent
measurements.
