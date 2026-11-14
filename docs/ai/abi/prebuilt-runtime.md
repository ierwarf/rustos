# Prebuilt ELF runtime libraries

Every ELF in each staged prebuilt application bundle is inspected, including
executable helpers, shared objects and bundles without desktop entries. Non-ELF
assets are skipped; ELF-magic inputs with malformed headers fail staging.
Inspection uses x86-64 program headers and `PT_DYNAMIC`, not section headers.
Each dynamic object has its own breadth-first dependency traversal and
loaded-name namespace.
`DT_RUNPATH` applies to direct dependencies; `DT_RPATH` is inherited following
glibc's search rules. `$ORIGIN` and `${ORIGIN}` use the referencing object's
image directory. Relative, empty, unsupported-token, escaping or symlink-escape
search paths and path-bearing `DT_NEEDED` names are rejected. Dependencies
must be x86-64 `ET_DYN`; missing or malformed inputs fail staging. The source
set is deliberately finite; an application requiring a library outside the
lock fails rather than importing a host library. Existing image and app-private
libraries remain inputs from the configured image overlay and prebuilt bundle,
not authenticated by the Ubuntu lock. Those inputs must be trusted separately;
loader checks establish compatibility, not their provenance or safety. This is a `DT_NEEDED` closure,
not a prediction of `dlopen`, locale, driver, or plugin assets.

Missing system libraries are copied unchanged to `/usr/lib/x86_64-linux-gnu`
from the verified package set described below. No symbol/version bytes are
rewritten and no compatibility shim is built. `libc.so.6` and the x86-64
loader are never installed into the image from these packages or overwritten; a private copy is
accepted only if byte-identical to the image baseline. The regular stage entry
point removes and recreates the whole image before copying overlays and app
artifacts, so libraries left by older staging implementations do not survive a
restage. The linker cache is regenerated later in stage. Contained file aliases from
source trees and aliases created by ldconfig are materialized as regular files:
FAT cannot represent symlinks. Escaping, dangling and directory aliases fail
rather than disappearing silently. The final inventory rejects remaining
symlinks and non-UTF8 names, so the verified tree cannot silently lose aliases
or collapse distinct names through lossy conversion during serialization.

After generating the final linker cache, stage checks that libc and the loader
still equal the intended overlay bytes captured before staging manifests, then
verifies relocation before signing the early-system image. Each actual
`PT_INTERP` must be a terminated absolute path to the same loader bytes; missing,
malformed or alternate loader bytes fail rather than being silently replaced by
the test loader. Shared objects without an interpreter use the image baseline.
The actual final-cache selections for libc and the loader are also checked
against baseline bytes, not merely accepted when their symbol versions resolve.
A compatible but different cache-preferred private libc is rejected.
Stage runs an executable-mode copy of these exact loader bytes in a bubblewrap
root containing only the finalized staged image. The environment is cleared;
the actual final cache is used. `LD_TRACE_LOADED_OBJECTS`, `LD_WARN`, and
`LD_BIND_NOW` request eager relocation diagnostics. Any missing
library, undefined symbol, version error, or loader failure fails stage. glibc's
trace relocation uses `__RTLD_NOIFUNC`: this validates symbol/version
relocations, not IFUNC execution or application behavior. Bubblewrap with user
namespaces, Python 3, GnuPG (`gpg` and `gpgv`), and network access for a cold
cache are required. No host library fallback is mounted. The intended libc and
loader bytes are not changed by this check. The explicit ignored Chrome test
requires the complete bundle (not just the desktop executable), verifies helpers
and shared objects, and rejects incompatible/missing libraries and a conflicting
final cache and a compatible non-baseline private libc. It also serializes the
verified fixture to FAT and reopens every recorded file to compare its payload,
including materialized aliases. Run it with `RUSTOS_RUNTIME_FIXTURE_OVERLAY=assets/image`,
`RUSTOS_RUNTIME_FIXTURE_CHROME=apps/chrome/bundle`, and
`RUSTOS_RUNTIME_FIXTURE_CACHE=build/runtime-libs/noble/cache` using absolute paths:
`cargo test -p xtask chrome_target_loader_relocates_without_host_fallbacks -- --ignored`.

## Provenance and updates

`tools/xtask/src/stage/noble/lock.json` pins package paths, sizes and SHA-256
hashes from the Ubuntu amd64 Noble archive snapshot at **2026-11-10T00:00:00Z**.
The provisioner pins the exact signed Noble and Noble-updates `InRelease`
hashes and Ubuntu archive 2018 signing fingerprint
`F6ECB3762474EDA9D21B7022871920D1991BC93C`. At every provision it verifies
each InRelease signature and fingerprint, verifies each Packages.xz against
its signed SHA-256, checks every locked record against the authenticated
index, and hashes the exact `.deb` bytes returned, including cached files
(no validate-then-reread cache race). Package-pool transport failures may use
Ubuntu's official archive/security mirrors with an explicit diagnostic, but
only the identical size and SHA-256 authenticated by the pinned snapshot are
accepted. Release/index authority never falls back to a mutable mirror; hash
mismatch is fatal. Bounded retries and per-invocation failed-mirror suppression
avoid repeating a dead transport for every package. It extracts only
library files into a freshly reconstructed cache root and never invokes Debian
package scripts. The cache is not a trust anchor. No host-installed package or
library directory supplies runtime libraries. The lock covers all Chrome bundle ELFs, including Qt5/Qt6 shim dependencies,
and their library-package dependencies; it is not the whole archive. Non-library
package assets, services, driver plugins, and maintainer scripts are not
installed by this provisioner. `noble_lock.py BUILD_DIR LIBRARY_PACKAGE...` can
extend the lock using only the authenticated pinned indexes. It preserves
existing records, resolves versioned Provides (including Noble's t64 names),
and checks Debian epoch/upstream/revision constraints without depending on a
host Debian package manager. It is not a replacement for apt's full package
installation policy. The subsequent complete ELF and target-loader checks
remain mandatory; missing runtime assets or `dlopen` targets are not proven by
a library package dependency list.

Snapshot pins make repeated builds reproducible, **not automatically secure**.
When updating the lock, use a snapshot no more than seven days old at update
time, verify the archive fingerprint out of band against Ubuntu, authenticate
both Release and package indexes, select package hashes from those indexes,
review additions/removals, and rerun the isolated loader checks against the
actual signed image baseline and affected applications. Keep the pinned
snapshot and all package hashes immutable between reviewed updates; periodically
refresh them for security updates rather than interpreting a reproducible old
snapshot as current security support. The signed baseline observed for this
change is Ubuntu glibc `2.39-0ubuntu8.7`; compatibility of this snapshot with a
different baseline is not presumed and is checked by that baseline's loader.

Primary references: [Ubuntu snapshot service](https://snapshot.ubuntu.com/),
[Ubuntu archive signing keys](https://archive.ubuntu.com/ubuntu/project/ubuntu-archive-keyring.gpg),
[Ubuntu archive verification and package hashes](https://wiki.ubuntu.com/SecureApt),
[glibc 2.39 loader search and token handling](https://raw.githubusercontent.com/bminor/glibc/release/2.39/master/elf/dl-load.c),
[glibc 2.39 trace relocation path](https://raw.githubusercontent.com/bminor/glibc/release/2.39/master/elf/rtld.c),
and [ELF dynamic linking specification](https://refspecs.linuxfoundation.org/elf/gabi4%2B/ch5.dynamic.html),
and [Debian version comparison policy](https://www.debian.org/doc/debian-policy/ch-controlfields.html#version).
