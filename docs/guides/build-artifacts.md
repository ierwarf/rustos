# Build Artifacts and Disk

A RustOS checkout's source is small — under 40M tracked, with a `.git` around
110M. Everything else on the device is regenerable output, and left alone it
grows without bound: this tree reached 87G, of which 86G was build product.

None of it was ever a source-control problem. `.gitignore` already covered
every generated path and `git status` was clean throughout. Rewriting history
would have reclaimed nothing.

## Where the space goes

| Path | What it is | Rebuild cost |
| --- | --- | --- |
| `target/` | Workspace compilation. `debug/incremental` alone held 29G across 1733 session directories. | Minutes |
| `driver-domains/linux/out/buildroot-output/` | The Buildroot appliance tree, Mesa included. | Hours |
| `driver-domains/linux/out/{dl,ccache}/` | Download cache and compiler cache. | Re-download |
| `build/formal/` | Formal lane output. 14G, of which the sealed evidence is under 10M. | Per lane |

The asymmetry in that last row is the important one. Every log, summary,
detached signature, and proof index a seal depends on fits in ten megabytes;
the rest is mutation shards' cargo trees and the ABI differential's Wine
prefix. Reclaiming the scratch never touches the evidence.

## Reclaiming

```bash
cargo xtask clean --dry-run --stale 7 --scratch   # report only
cargo xtask clean --stale 7                       # aged compilation residue
cargo xtask clean --scratch                       # formal lanes' regenerable trees
```

`--stale <DAYS>` is the one to reach for. Cargo keys an incremental session
directory by crate and fingerprint and never revisits a session whose
fingerprint has moved on, so an aged session is unreachable by any future
build — deleting it costs nothing and 1372 of the 1733 sessions here held 24G.
The warm cache is left intact.

Bare `cargo xtask clean` is still the full wipe. AGENTS.md asks an interrupted
build to resume rather than clean, and that guidance stands: the tiers exist so
that reclaiming space does not mean discarding a working cache.

`.cargo/config.toml` also sets `gc = true`, which lets Cargo age out unused
registry sources under `CARGO_HOME` on its own. That governs the Cargo home
cache, not `target/`.

## Keeping it small

`[profile.dev]` in the workspace manifest carries the settings that stop the
regrowth: line tables instead of full DWARF, no debug info for dependencies or
build scripts, and debug info left unpacked in object files rather than copied
into every binary. Backtraces keep their file and line numbers; what is dropped
is the variable-inspection data a source debugger would want from a dependency.

`release` is deliberately excluded. It is the profile the nucleus, the image,
and every `cargo xtask bench` baseline are built under, and a codegen change
there silently reprices every recorded cycle count.

## Build speed

`.cargo/config.toml` adds two host-target flags: LLD for linking and the
parallel rustc front-end at 8 threads. Both reach ordinary `cargo
build`/`test`/`check` only. Kernel and nucleus builds go through
`apply_kernel_cargo_env`, which sets the `RUSTFLAGS` environment variable
outright, and Cargo discards `target.*.rustflags` whenever that variable is
present — so the kernel's `-static-pie` / `-nostartfiles` link line is
untouched. Express host tuning as `rustflags`, never as a `linker` key:
`target.*.linker` is *not* suppressed by the environment variable and would
follow the kernel into its link step.

Measured on cold builds into a scratch target directory:

| Build | Before | After |
| --- | --- | --- |
| `xtask` dependency closure | 10.96s, 498M | 8.97s, 165M |
| `cargo test -p kernel-ps --no-run` | 7.60s, 464M | 4.63s, 299M |

Optimizing build scripts and proc macros was measured too and rejected: at
`opt-level = 1` the `xtask` closure went from 10.96s to 13.01s, so only their
debug info is dropped.

## What this means for the formal gate

`verify-all.sh` runs its lanes concurrently, so the gate's wall clock is the
slowest lane, not their sum. That lane is `implementation-mutations` at 248s,
against 159s for `kani` and under 80s for everything else — speeding up any
other lane moves nothing until it passes 159s.

The mutation lane is compile-bound: it shards across four checkouts and, for
each of 484 mutants, compiles and runs one witness test under the `dev`
profile. 184 of those mutants target `kernel-ps`, whose test build is the
second row of the table above. The profile and linker changes land directly on
that inner loop.

Two things that look like levers are not:

- **sccache.** It is installed but unused, and the four shards do compile
  overlapping trees. Enabling it disables incremental compilation, and the
  shard layout deliberately keeps every mutation of one source file adjacent
  so cargo rebuilds one crate incrementally. Trading that for a cache is not
  obviously a win.
- **Shard count.** It is `min(4, cpu_count / 4, mutants, affordable)`. On 16
  cores that is 4, and the disk term only binds on a nearly full device.
  Reclaiming space does not buy more shards here.

TLC is left as it is. It runs first and alone, because its budgets are wall
clocks that only mean what they say when the lane is not competing for cores,
and it already uses `-workers auto`. Its `-coverage 1` is not diagnostic
output: `run-tlc.sh` fails the model on any operator with a zero count, so
reducing coverage reporting would weaken a gate rather than speed one up.
