# RustOS source provenance

- Upstream: https://github.com/rafalh/rust-fatfs
- Exact baseline: `c4b88477b22ca7e5131fbd8891f62a5deaa88e6e` (fatfs 0.4.0).
- License: unmodified MIT notice in `LICENSE.txt`.
- This directory contains the dependency's library sources, manifest and README;
  upstream integration images/examples are not a claim of tests run here.
- Root Cargo uses a source patch rather than modifying Cargo's checkout cache.

Local patch: `FileSystem::valid_data_cluster` checks the volume's actual data
cluster interval before file extent/read/write offset arithmetic and seek /
truncate traversal. Reserved cluster 1 previously reached `cluster - 2` and
could panic with overflow checking, or wrap an offset without it. Invalid file
cluster numbers now return `CorruptedFileSystem`. The small MSRV increase to
1.70 reflects `Option::is_some_and`; RustOS's pinned toolchain exceeds it.

The adapter also uses a read-only `File::directory_entry_offset` accessor
(backed by `DirEntryEditor::position`) so repeated/case-alias plans identify the
same entry instead of minting an object per request. The adapter binds this
reusable slot to volume and mutation generation; it is not a persistent inode.
`root_dir` explicitly annotates its already-elided return lifetime to avoid a
new-toolchain warning when built as a local path dependency.

The adapter additionally bounds extent iteration by the admitted file length,
checks physical range overlap (cyclic chains), geometry, epochs and mapping
metadata budgets. This is not a complete hostile-filesystem or writable-FAT
crash-consistency audit. RustOS integration tests live in `storage-fat`.
