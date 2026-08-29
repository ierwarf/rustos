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

The size settings pay a little speed as a side effect — there is less debug info
to emit and less for the linker to move — but only a little. Measured serially
on cold builds into a scratch target directory, against the tree as it stood
before these settings:

| Build | Before | After |
| --- | --- | --- |
| `xtask` dependency closure | 10.68s, 498M | 10.20s, 253M |
| `cargo test -p kernel-ps --no-run` | 7.82s, 464M | 7.52s, 299M |

Roughly 4% off the clock and 40-50% off the disk. Do not expect more from this
change; the disk is what it was for.

Three further levers were tried and rejected. Do not re-add them without
re-testing:

- `opt-level = 1` for build scripts/proc macros: made the `xtask` dependency
  closure slower, not faster (10.96s -> 13.01s).
- `-Clink-arg=-fuse-ld=lld`: this target already links with the toolchain's
  own `rust-lld` (`linker-flavor = gnu-lld-cc`); the flag only swaps in
  whatever external `ld.lld` sits on `PATH`.
- `-Zthreads=8` (parallel front-end): miscompiled `wayclick` (dropped a
  monomorphized instance, undefined `From<WaylandError>` at link time) and
  poisoned the target tree until it was removed. `cargo xtask check` can't
  catch this because checking never links. If retried, test with
  `cargo build -p wayclick` in a fresh target directory first.

## Why `--workspace` does not build

`cargo build --workspace` fails on a duplicate `panic_impl` lang item, and no
dependency pin fixes it. The workspace holds 28 `no_std` members and 17 host
members. The `no_std` service binaries define their own panic handler; the host
members pull shared dependencies — `fatfs`, `bitflags`, and others behind them —
with the `std` feature on. Cargo unifies features across whatever one invocation
is asked to build, so the union links `std` into a freestanding binary and rustc
rejects the second lang item.

The split is not tools-versus-RustOS. `uiserver` and `netd` are std services;
`vfsd` and `syscalld` are not. Any fix that keeps both halves in one cargo
invocation would have to repartition the services themselves.

Each half builds cleanly alone, so `default-members` in the workspace manifest
pins the default to the host half. Plain `cargo build`, `cargo test`, and
`cargo clippy` work at the repo root; the freestanding half is built per
package, with its own target and link line, by `cargo xtask build`.
`cargo xtask check` remains the whole-tree gate. `cargo build --workspace`
explicitly asks for the union and still fails — that is the invocation to avoid.

## What this means for the formal gate

`verify-all.sh` runs its lanes concurrently, so the gate's wall clock is the
slowest lane, not their sum. That lane is `implementation-mutations` at 248s,
against 159s for `kani` and under 80s for everything else — speeding up any
other lane moves nothing until it passes 159s.

The mutation lane is compile-bound: it shards across four checkouts and, for
each of 484 mutants, compiles and runs one witness test under the `dev`
profile. 184 of those mutants target `kernel-ps`, whose test build is the
second row of the table above, so the profile change lands directly on that
inner loop — at about 4%, which is honest but small. Nothing here makes the
formal gate substantially faster.

Three things that look like levers are not:

- sccache is installed but unused: enabling it disables incremental
  compilation, which the shard layout is built around.
- Shard count is `min(4, cpu_count / 4, mutants, affordable)`; on 16 cores
  that's already 4, so freeing disk space buys nothing here.
- The parallel front-end would help most but miscompiles — see above.

TLC is left as it is. It runs first and alone, because its budgets are wall
clocks that only mean what they say when the lane is not competing for cores,
and it already uses `-workers auto`. Its `-coverage 1` is not diagnostic
output: `run-tlc.sh` fails the model on any operator with a zero count, so
reducing coverage reporting would weaken a gate rather than speed one up.
