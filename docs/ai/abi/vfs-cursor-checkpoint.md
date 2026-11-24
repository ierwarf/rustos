# VFS Cursor Read Checkpoint ABI Redesign (Item 2.3)

Owner design contract for removing per-read `rootd` checkpoint mutations on ordinary regular-file sequential reads while preserving exact copy-out fault semantics, `dup`/`fork` cursor coherence, and crash/restart recovery across `vfsd` restarts.

Related owners: [VFS and storage contracts](vfs-storage.md), [Contract index](../contracts-abi.md), [VfsOpenDescriptionRecovery.tla](../../../formal/vfs-open-description-recovery/VfsOpenDescriptionRecovery.tla).

## Problem and Why Naive Batching Fails

Today, every sequential `read()` chunk (at most `VFS_IPC_PAYLOAD_CAPACITY` = 64 KiB) on a regular `RemoteVfsHandleKind::File` executes a two-phase prepare/settle protocol across two Ring3 services:

1. Kernel (`kernel/compat/src/user/syscall/linux/service_ops/vfs_socket.rs::read_remote_vfs_pinned`) acquires the per-description cursor gate and sends `VFS_IPC_OP_READ` (op 4) to `vfsd`.
2. `vfsd` (`services/vfsd/src/state_storage.rs::read_remote_into`) reads from `handle.cursor`, computes `candidate.cursor = cursor + read`, and issues a synchronous `COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_MUTATE` IPC to `rootd` recording `VFSD_OPEN_MUTATION_READ` (prepared state).
3. Kernel copies the returned bytes to userspace (`usermem::write_current_user_bytes`), then sends `VFS_IPC_OP_CURSOR_SETTLE` (op 24, `COMMIT` = 1 or `CANCEL` = 2) to `vfsd`.
4. `vfsd` (`services/vfsd/src/state_files.rs::vfs_cursor_settle`) issues a second synchronous `rootd` checkpoint mutation recording `VFSD_OPEN_MUTATION_STABLE`.

That costs **4 synchronous IPCs per 64 KiB chunk** (2 kernel↔`vfsd`, 2 `vfsd`↔`rootd`) and serializes every sequential file reader behind `rootd` checkpoint mutations.

**Why N-byte / N-ms batching in `vfsd` is unsound:** If `vfsd` advances its in-memory `handle.cursor` without persisting it to `rootd`, a `vfsd` crash and replay (`services/vfsd/src/state_checkpoint.rs::restore_open_descriptions`) restores the stale `rootd` cursor. Subsequent sequential reads then silently re-read already-delivered bytes (duplicate reads), and reply-loss retries cannot distinguish whether the prior read advanced the volatile cursor.

**Key architectural insight:** When `vfsd` crashes and restarts, **Ring0 does not crash**. The kernel already owns the open-description handle table (`KernelHandle::RemoteVfs`), local reference counting, and the per-description cursor gate. Moving the live sequential file cursor into the kernel's shared open-description state makes sequential reads stateless positioned reads against `vfsd`, eliminating per-read `rootd` checkpoints completely without weakening crash recovery.

---

## 1. Ownership Split

### Scope

| Handle kind | Live sequential cursor owner | Read wire path | Checkpoint model |
| --- | --- | --- | --- |
| `RemoteVfsHandleKind::File` (FAT, TmpFs, `/proc` snapshots) | **Kernel** (`RemoteVfsOpenDescription`) | Stateless `VFS_IPC_OP_PREAD64` / `VFS_IPC_OP_BULK_READ` | Settled at explicit settle points only; **0 checkpoints per read** |
| `RemoteVfsHandleKind::Directory` | `vfsd` + `rootd` (unchanged) | `VFS_IPC_OP_GETDENTS64` + `VFS_IPC_OP_CURSOR_SETTLE` (`COMMIT`/`CANCEL`) | Unchanged: opaque directory cookies remain `vfsd`-owned |
| `RemoteVfsHandleKind::Device` / `Inotify` | Non-seekable / stream (unchanged) | Compat virtual devices, `inputd`, or `VFS_IPC_OP_READ` (inotify) | No file-offset cursor |

### Kernel Open-Description State (`RemoteVfsOpenDescription`)

`RemoteVfsHandle` in `kernel/ps/src/user/handles.rs` gains a shared open-description object behind `Arc`, matching `MemfdHandle::inner` and `ConsoleHandle::description`:

```rust
pub struct RemoteVfsOpenDescription {
    cursor_state: Mutex<RemoteVfsCursorState>, // leaf lock, never held across IPC or user copy
}

struct RemoteVfsCursorState {
    /// Authoritative live byte offset for sequential I/O (0..=i64::MAX as u64).
    cursor: u64,
    /// Last cursor value confirmed durable in vfsd/rootd.
    durable_cursor: u64,
    /// Monotonic vfsd endpoint epoch observed when durable_cursor was last confirmed.
    durable_epoch: u64,
}
```

Invariants:
- **Initialization:** `openat` and `VFS_FCNTL_REOPEN` create a fresh `Arc<RemoteVfsOpenDescription>` with `cursor = 0`, `durable_cursor = 0`, `durable_epoch = current_vfsd_epoch`.
- **Sharing across `dup` / `fork` / `SCM_RIGHTS`:** Cloning a `RemoteVfsHandle` clones the `Arc<RemoteVfsOpenDescription>`. All descriptors referring to the same open file description—within one process (`dup`/`dup2`/`dup3`/`F_DUPFD*`), across `fork()`, or passed via `SCM_RIGHTS`—share one `RemoteVfsOpenDescription` instance. Independent `openat` / `VFS_FCNTL_REOPEN` calls allocate a new `Arc`.
- **Locking discipline:** `cursor_state` is a bounded leaf lock (no allocation, no IPC, no user copy while held). Serialization of concurrent sequential operations (`read`, `write`, `lseek`, `fsync`, `sendfile`, `copy_file_range`, cursor settle) on one open description continues to use the existing per-description cursor gate (`acquire_vfs_cursor_gate(remote_id)` in `kernel/compat/src/user/syscall/linux/service_ops/ipc_helpers.rs`).
- **Zero-round-trip copy-out fault preservation:** Under the cursor gate, sequential `read()` reads `start = desc.cursor()`, fetches bytes via stateless positioned read, copies them to user memory via `usermem::write_current_user_bytes`, and advances `desc.cursor` **only after `copy_to_user` succeeds**. If `copy_to_user` faults (`EFAULT`), `desc.cursor` is untouched (or advances only by already-copied preceding chunks when `copied > 0`, returning `copied` per Linux `read(2)`). No `prepare`, `commit`, or `cancel` IPC to `vfsd` or `rootd` is ever needed.
- **No policy moved to Ring0:** Ring0 holds only the numeric byte offset of an already-admitted open description (identical to `MemfdHandle`). Path resolution, permissions, file length, `O_APPEND` EOF placement, and storage backing remain 100% in `vfsd`.

---

## 2. Stateless Positioned Reads (`PREAD64` / `BULK_READ`)

Sequential `read(fd, buf, count)` on `RemoteVfsHandleKind::File` translates in `read_remote_vfs_pinned`:

1. Validate user buffer and acquire `acquire_vfs_cursor_gate(remote_id)`.
2. Snapshot `start = remote.cursor()` from `RemoteVfsOpenDescription`.
3. Loop while `copied < user_len`:
   - Compute `chunk_offset = start.checked_add(copied as u64)` (reject `> i64::MAX as u64` with `EOVERFLOW` if `copied == 0`, else return `copied`).
   - Select transport:
     - If `remaining >= VFS_BULK_READ_MIN_BYTES` (256 KiB) and the file is eligible for bulk read, issue `VFS_IPC_OP_BULK_READ` (op 46) for `remaining.min(VFS_BULK_READ_MAX_BYTES)` (up to 4 MiB) at `chunk_offset`. On `EOPNOTSUPP` (e.g., `/proc` snapshot handles), fall back transparently to `VFS_IPC_OP_PREAD64`.
     - Otherwise, issue `VFS_IPC_OP_PREAD64` (op 6) with `arg0 = chunk_offset`, `arg1 = remaining.min(VFS_IPC_PAYLOAD_CAPACITY)`.
   - Copy returned bytes into `user_ptr + copied` via `usermem::write_current_user_bytes`.
   - On copyout success: `copied += read`, update `remote.advance_cursor(read)` (which sets `cursor = start + copied`, leaving `durable_cursor` unchanged so `is_dirty()` becomes true).
   - On EOF (`read == 0`) or short read (`read < chunk_len`), stop the loop.
4. Release the cursor gate and return `copied`.

Properties:
- **Zero `rootd` checkpoints** on sequential reads.
- **Idempotent retry across `vfsd` restart or reply loss:** Because `PREAD64` and `BULK_READ` mutate neither `vfsd`'s `handle.cursor` nor `rootd`, a transport error or `vfsd` restart mid-read leaves no `prepared` record behind. Retrying at `start + copied` returns the exact next bytes with zero risk of double-advancing.
- **Read-ahead / `sendfile` / `copy_file_range`:** `kernel/compat/src/user/syscall/linux/service_ops/file_ops.rs` already uses positional `PREAD64`/`PWRITE64` under the cursor gate; it reads and advances `RemoteVfsOpenDescription` directly instead of issuing `VFS_IPC_OP_LSEEK(SEEK_CUR)` / `SEEK_SET` round trips.
- **`/proc` snapshots:** `services/vfsd/src/state_storage.rs::read_remote_into` already serves `/proc` snapshots at arbitrary `start` offsets via `read_proc_snapshot_range(id, start, dest)`, so `PREAD64` works unchanged on `/proc` regular files.

---

## 3. Durable Settle Points

The kernel's `desc.cursor` is dirty whenever `cursor != durable_cursor || current_vfsd_epoch != durable_epoch`. Every operation where `vfsd` or durable crash-recovery state must observe the exact cursor settles it under `acquire_vfs_cursor_gate(remote_id)`.

### Explicit Settle Operation (`VFS_CURSOR_SETTLE_SYNC`)

Extend `VFS_IPC_OP_CURSOR_SETTLE` (op 24, `VFS_IPC_ABI_VERSION` stays 5) with a third mode constant in `libs/rustos-user-abi/src/syscall.rs`:

- `VFS_CURSOR_SETTLE_COMMIT = 1` (existing, retained for directory `getdents64`)
- `VFS_CURSOR_SETTLE_CANCEL = 2` (existing, retained for directory `getdents64`)
- `VFS_CURSOR_SETTLE_SYNC = 3` (**new**, regular files only):
  - Request fields: `remote_id`, `arg0 = kernel_cursor` (must be `<= i64::MAX as u64`), `arg1 = 0`, `arg2 = VFS_CURSOR_SETTLE_SYNC`, `arg3 = 0`, `path_len = 0`, `payload_len = 0`.
  - `vfsd` validation: rejects non-`RemoteKind::File` with `EINVAL`, rejects `arg0 > i64::MAX as u64` with `EINVAL`, rejects `cursor_mutation_prepared(&handle)` with `EBUSY`.
  - `vfsd` action: sets `candidate.cursor = request.arg0`, `candidate.last_mutation = VFSD_OPEN_MUTATION_STABLE`, `candidate.last_start = 0`, `candidate.last_result = 0`, checkpoints to `rootd` keyed by `(operation_hi, operation_lo)`, and replies with `response.value = candidate.cursor`.
  - Replay safety: if `(operation_hi, operation_lo)` is replayed after reply loss or `vfsd` restart, `vfsd` verifies `handle.last_mutation == VFSD_OPEN_MUTATION_STABLE` and `handle.cursor == request.arg0` and returns `response.value = handle.cursor` idempotently.
  - Kernel action on success: `desc.mark_settled(cursor, observed_vfsd_epoch)`.

### Settle Point Matrix

| Operation | Behavior under `acquire_vfs_cursor_gate(remote_id)` | Extra IPCs? |
| --- | --- | --- |
| **`lseek(fd, offset, whence)`** | Carries `arg2 = desc.cursor()` and `flags |= VFS_LSEEK_FLAG_SYNC_CURSOR` in `VFS_IPC_OP_LSEEK`. `vfsd` computes `SEEK_CUR` from `arg2` (when the flag is set) and `SEEK_SET`/`SEEK_END` as usual, checkpoints `VFSD_OPEN_MUTATION_LSEEK` to `rootd`, and returns `new_cursor` in `response.value`. Kernel calls `desc.set_synced_cursor(new_cursor, epoch)`. | **0 extra IPCs** (piggybacked on `LSEEK`) |
| **`write(fd, buf, len)`** | Carries `arg0 = desc.cursor()` and `flags |= VFS_WRITE_FLAG_CURSOR_SYNC` in `VFS_IPC_OP_WRITE`. For non-`O_APPEND`, `vfsd` writes at `arg0`; for `O_APPEND`, `vfsd` writes at live EOF atomically. `vfsd` checkpoints `candidate.cursor = start + written` (`VFSD_OPEN_MUTATION_STABLE`), returns `written` in `response.value` and `candidate.cursor` in `response.aux`. Kernel calls `desc.set_synced_cursor(response.aux, epoch)`. | **0 extra IPCs** (piggybacked on `WRITE`) |
| **`fsync(fd)` / `fdatasync(fd)`** | If `desc.is_dirty()`, issues `VFS_IPC_OP_CURSOR_SETTLE(VFS_CURSOR_SETTLE_SYNC, desc.cursor())` before `VFS_IPC_OP_FSYNC` so the durable checkpoint reflects the post-read cursor before `fsync` returns. If clean, issues only `VFS_IPC_OP_FSYNC`. | **1 IPC only if dirty**; 0 if clean |
| **Final `close(fd)`** | Retires the open description via `VFS_IPC_OP_CLOSE` → tombstone → `VFS_IPC_OP_CHECKPOINT_ACK` compaction. No pre-close cursor flush is required because no reader can ever observe the cursor after final close, and regular files never enter `prepared` state (eliminating `EBUSY` close rejections on regular files). Non-final `close(fd)` only drops the local fd reference; surviving `dup`/`fork` peers keep the shared `Arc<RemoteVfsOpenDescription>`. | **0 extra IPCs** |
| **`vfsd` restart / reattach** | `vfsd` replays `rootd` checkpoints (`restore_open_descriptions`), restoring handles at `durable_cursor`. Because Ring0 retained `desc.cursor`: (1) subsequent sequential `read()` immediately uses `PREAD64`/`BULK_READ` at `desc.cursor` without waiting for a resync; (2) the first `lseek`, `write`, `fsync`, or explicit reconcile observes `current_vfsd_epoch != durable_epoch` and syncs `desc.cursor` to the new `vfsd` instance via `VFS_CURSOR_SETTLE_SYNC` (or the inline `LSEEK`/`WRITE` cursor-sync flag). No stop-the-world fd-table sweep is needed on restart. | **0 on restart**; lazy on next sync/mutating op |
| **`fork` / `dup` / `SCM_RIGHTS`** | All duplicates share the same `Arc<RemoteVfsOpenDescription>`. No `vfsd` IPC is required. Concurrent operations across parent/child/threads serialize on `acquire_vfs_cursor_gate(remote_id)`. | **0 IPCs** |

---

## 4. Crash / Restart Recovery Invariants and TLA+ Diff

### Why Neither Duplicate Reads Nor Skipped Bytes Can Occur

Let $C_k$ be the kernel's `RemoteVfsOpenDescription.cursor`, $C_d$ be `rootd`'s durable checkpointed cursor, and $U$ be the byte stream delivered to userspace via successful `copy_to_user`.

1. **Sequential read success:** Under the per-description cursor gate, the kernel requests bytes at $[C_k, C_k + n)$ via stateless `PREAD64`/`BULK_READ`, copies $[C_k, C_k + m)$ ($m \le n$) to userspace, and atomically sets $C_k' = C_k + m$. Thus $U$ receives $[C_k, C_k + m)$ and the next read starts at $C_k + m$.
2. **Copy-out fault (`EFAULT`):** `write_current_user_bytes` fails before $C_k$ advances. Neither $C_k$ nor $C_d$ changes. No bytes were skipped and no `vfsd` state was dirtied.
3. **Reply loss on `PREAD64`/`BULK_READ`:** The kernel never received the reply, never copied bytes to userspace, and never advanced $C_k$. Retry requests the exact same offset $C_k$. Neither duplicate reads nor skipped bytes occur.
4. **`vfsd` crash and restart mid-read or between reads:** `vfsd` loses volatile state and replays $C_d$ from `rootd` ($C_d$ may lag $C_k$). Ring0 preserves $C_k$ intact.
   - If the next syscall is `read()`, the kernel issues `PREAD64`/`BULK_READ` at $C_k$ — `vfsd` ignores its lagging `handle.cursor` and serves $[C_k, C_k + n)$ directly. Zero duplicates, zero skips.
   - If the next syscall is `lseek(SEEK_CUR)` or `write()`, the kernel supplies $C_k$ via `VFS_LSEEK_FLAG_SYNC_CURSOR` / `VFS_WRITE_FLAG_CURSOR_SYNC`, so `vfsd` never operates on the lagging $C_d$.
   - If the next syscall is `fsync()`, the kernel settles $C_d \leftarrow C_k$ via `VFS_CURSOR_SETTLE_SYNC` before `FSYNC`.
5. **Reply loss on `VFS_CURSOR_SETTLE_SYNC` / `LSEEK` / `WRITE`:** Every mutating op carries a unique `(operation_hi, operation_lo)` identity. If `vfsd` committed the checkpoint before reply loss or restart, replaying the exact `(operation_hi, operation_lo)` matches `checkpoint_operations` in `vfsd` and returns the committed `handle.cursor` without re-applying the mutation.

### Exact `VfsOpenDescriptionRecovery.tla` State-Machine Diff

The existing model (`formal/vfs-open-description-recovery/VfsOpenDescriptionRecovery.tla`) models every read as `PrepareRead -> CommitRead | CancelRead` using `durable = "prepared"`. Under this redesign:
- Regular files (`kind = "file"`) use stateless reads (`StatelessFileReadSuccess`, `StatelessFileReadFaultOrLoss`) that advance `kernelCursor` only on visible copyout, plus `SettleFileCursor` (`durableCursor' = kernelCursor`), and **never** enter `durable = "prepared"`.
- Directories (`kind = "dir"`) retain `PrepareDirRead` / `CommitDirRead` / `CancelDirRead` (`durable = "prepared"`).

```diff
--- a/formal/vfs-open-description-recovery/VfsOpenDescriptionRecovery.tla
+++ b/formal/vfs-open-description-recovery/VfsOpenDescriptionRecovery.tla
@@ -4,10 +4,13 @@ EXTENDS Integers, TLC
 (***************************************************************************
 Finite refinement of the rootd-retained vfsd open-description protocol.
 An OPEN is published only after every path chunk is durable.
-Sequential I/O first prepares a cursor advance and is then committed or
-cancelled after the kernel user-copy result. CLOSE retains a tombstone
-through response loss and only an explicit visibility ACK permits compaction.
+For regular files (kind = "file"), the kernel owns the live sequential cursor
+(kernelCursor), issues stateless positioned reads, advances kernelCursor only
+after user-copy succeeds, and flushes durableCursor at explicit settle points
+(lseek/write/fsync/CURSOR_SETTLE_SYNC). For directories (kind = "dir"),
+getdents64 retains prepared/commit/cancel. CLOSE retains a tombstone through
+response loss and only an explicit visibility ACK permits compaction.
 ***************************************************************************)
 
 CONSTANT MaxCursor
 ASSUME MaxCursor \in Nat \ {0}
 
+Kinds == {"file", "dir"}
 States == {"absent", "staging", "stable", "prepared", "tombstone"}
 
-VARIABLES serviceLive, endpointPublished, durable, local, pathChunks,
-          cursor, startCursor, copyVisible, closeReplySeen, compacted
+VARIABLES kind, serviceLive, endpointPublished, durable, local, pathChunks,
+          durableCursor, kernelCursor, startCursor, deliveredOffset,
+          copyVisible, closeReplySeen, compacted
 
-vars == <<serviceLive, endpointPublished, durable, local, pathChunks,
-          cursor, startCursor, copyVisible, closeReplySeen, compacted>>
+vars == <<kind, serviceLive, endpointPublished, durable, local, pathChunks,
+          durableCursor, kernelCursor, startCursor, deliveredOffset,
+          copyVisible, closeReplySeen, compacted>>
 
 Init ==
+    /\ kind \in Kinds
     /\ serviceLive = TRUE
     /\ endpointPublished = FALSE
     /\ durable = "absent"
     /\ local = "absent"
     /\ pathChunks = 0
-    /\ cursor = 0
+    /\ durableCursor = 0
+    /\ kernelCursor = 0
     /\ startCursor = 0
+    /\ deliveredOffset = 0
     /\ copyVisible = FALSE
     /\ closeReplySeen = FALSE
     /\ compacted = FALSE
@@ -76,26 +83,61 @@ PublishEndpoint ==
     /\ UNCHANGED <<serviceLive, durable, local, pathChunks, cursor,
                    startCursor, copyVisible, closeReplySeen, compacted>>
 
-PrepareRead ==
+\* Regular-file stateless positioned read: succeeds only after user copy.
+StatelessFileReadSuccess ==
+    /\ kind = "file"
+    /\ serviceLive
+    /\ endpointPublished
+    /\ durable = "stable"
+    /\ kernelCursor < MaxCursor
+    /\ deliveredOffset' = kernelCursor
+    /\ kernelCursor' = kernelCursor + 1
+    /\ copyVisible' = TRUE
+    /\ UNCHANGED <<kind, serviceLive, endpointPublished, durable, local,
+                   pathChunks, durableCursor, startCursor, closeReplySeen, compacted>>
+
+\* Regular-file positioned read where copy_to_user faults or reply is lost.
+StatelessFileReadFaultOrLoss ==
+    /\ kind = "file"
+    /\ serviceLive
+    /\ endpointPublished
+    /\ durable = "stable"
+    /\ kernelCursor < MaxCursor
+    /\ copyVisible' = FALSE
+    /\ UNCHANGED <<kind, serviceLive, endpointPublished, durable, local,
+                   pathChunks, durableCursor, kernelCursor, startCursor,
+                   deliveredOffset, closeReplySeen, compacted>>
+
+\* Explicit durable settle (CURSOR_SETTLE_SYNC, lseek, write, fsync).
+SettleFileCursor ==
+    /\ kind = "file"
+    /\ serviceLive
+    /\ endpointPublished
+    /\ durable = "stable"
+    /\ durableCursor # kernelCursor
+    /\ durableCursor' = kernelCursor
+    /\ UNCHANGED <<kind, serviceLive, endpointPublished, durable, local,
+                   pathChunks, kernelCursor, startCursor, deliveredOffset,
+                   copyVisible, closeReplySeen, compacted>>
+
+\* Directory getdents64 retains prepare/commit/cancel.
+PrepareDirRead ==
+    /\ kind = "dir"
     /\ serviceLive
     /\ endpointPublished
     /\ durable = "stable"
-    /\ cursor < MaxCursor
-    /\ startCursor' = cursor
-    /\ cursor' = cursor + 1
+    /\ durableCursor < MaxCursor
+    /\ startCursor' = durableCursor
+    /\ durableCursor' = durableCursor + 1
     /\ durable' = "prepared"
     /\ local' = "prepared"
     /\ copyVisible' = FALSE
-    /\ UNCHANGED <<serviceLive, endpointPublished, pathChunks,
-                   closeReplySeen, compacted>>
+    /\ UNCHANGED <<kind, serviceLive, endpointPublished, pathChunks,
+                   kernelCursor, deliveredOffset, closeReplySeen, compacted>>
 
-CommitRead ==
+CommitDirRead ==
+    /\ kind = "dir"
     /\ durable = "prepared"
     /\ copyVisible' = TRUE
+    /\ deliveredOffset' = startCursor
+    /\ kernelCursor' = durableCursor
     /\ durable' = "stable"
     /\ local' = "stable"
-    /\ UNCHANGED <<serviceLive, endpointPublished, pathChunks, cursor,
-                   startCursor, closeReplySeen, compacted>>
+    /\ UNCHANGED <<kind, serviceLive, endpointPublished, pathChunks,
+                   durableCursor, startCursor, closeReplySeen, compacted>>
 
-CancelRead ==
+CancelDirRead ==
+    /\ kind = "dir"
     /\ durable = "prepared"
-    /\ cursor' = startCursor
+    /\ durableCursor' = startCursor
     /\ durable' = "stable"
     /\ local' = "stable"
     /\ copyVisible' = FALSE
-    /\ UNCHANGED <<serviceLive, endpointPublished, pathChunks, startCursor,
-                   closeReplySeen, compacted>>
+    /\ UNCHANGED <<kind, serviceLive, endpointPublished, pathChunks,
+                   kernelCursor, deliveredOffset, startCursor,
+                   closeReplySeen, compacted>>
```

New and preserved safety invariants in `VfsOpenDescriptionRecovery.tla`:
- `CompletePathBeforeLive`: `durable \in {"stable", "prepared"} => pathChunks = 2` (preserved).
- `FileNeverEntersPrepared`: `kind = "file" => durable # "prepared"` (**new** — proves regular-file reads never block `close`/`lseek` with `EBUSY`).
- `FileCursorMonotonicAndExact`: `kind = "file" => durableCursor <= kernelCursor /\ (copyVisible => deliveredOffset + 1 = kernelCursor)` (**new** — proves no duplicate or skipped byte delivery across `Crash`/`Recover` and `StatelessFileReadFaultOrLoss`).
- `PreparedAdvanceIsReversible`: `durable = "prepared" => kind = "dir" /\ durableCursor = startCursor + 1` (scoped to directories).
- `EndpointPublishesOnlyReplayedState`, `CompactionRequiresObservedReply`, `TombstoneSurvivesUntilAck` (all preserved verbatim).

---

## 5. Migration and Validation Plan

### ABI Operations: Keep, Extend, Retire

| Constant / Op | Disposition | Notes |
| --- | --- | --- |
| `VFS_IPC_OP_PREAD64` (6) | **Keep** | Primary sequential regular-file read path (`< 256 KiB` or fallback). |
| `VFS_IPC_OP_BULK_READ` (46) | **Keep** | Primary sequential regular-file read path for `>= VFS_BULK_READ_MIN_BYTES` (256 KiB..4 MiB). |
| `VFS_IPC_OP_CURSOR_SETTLE` (24) | **Extend** | Keep `COMMIT` (1) & `CANCEL` (2) for `getdents64`; add `VFS_CURSOR_SETTLE_SYNC` (3) for regular-file cursor flush. |
| `VFS_IPC_OP_LSEEK` (7) | **Extend** | Add `VFS_LSEEK_FLAG_SYNC_CURSOR` (`arg2 = kernel_cursor`) so `SEEK_CUR` uses live kernel cursor and settles in 1 IPC. |
| `VFS_IPC_OP_WRITE` (5) | **Extend** | Add `VFS_WRITE_FLAG_CURSOR_SYNC` (`arg0 = kernel_cursor`) and return post-write cursor in `response.aux`. |
| `VFS_IPC_OP_READ` (4) | **Retire for regular files** | Kept only for inotify (`read_inotify_vfs`) and non-virtual device reads; regular-file callers never emit `VFS_IPC_OP_READ` or `VFSD_OPEN_MUTATION_READ`. |
| `VFSD_OPEN_MUTATION_READ` (2) | **Legacy decode-only** | Retained in `OpenDescriptionCheckpointWire::valid` for backward-compatible checkpoint scan; never emitted for new regular-file reads. |

### Exact Files to Modify During Implementation

1. **ABI (`libs/rustos-user-abi/`)**:
   - `libs/rustos-user-abi/src/syscall.rs`: add `VFS_CURSOR_SETTLE_SYNC = 3`, `VFS_LSEEK_FLAG_SYNC_CURSOR`, `VFS_WRITE_FLAG_CURSOR_SYNC`; update `VFS_IPC_OP_READ` doc comment.
   - `libs/rustos-user-abi/src/syscall/syscall_tests.rs`: pin new flag/mode constants.
2. **Kernel handle substrate (`kernel/ps/`)**:
   - `kernel/ps/src/user/handles.rs`: add `RemoteVfsOpenDescription` (`Arc<RemoteVfsOpenDescription>` inside `RemoteVfsHandle`) with `cursor()`, `advance_cursor()`, `set_synced_cursor()`, `is_dirty()`, `mark_settled()`.
   - `kernel/ps/src/user/handles/table/tests.rs`: verify `dup`/`fork`/`SCM_RIGHTS` share one `RemoteVfsOpenDescription` cursor while independent `RemoteVfsHandle::new` calls do not.
3. **Kernel Linux compat (`kernel/compat/`)**:
   - `kernel/compat/src/user/syscall/linux/service_ops/vfs_socket.rs`:
     - Update `read_remote_vfs_pinned` to translate regular-file sequential `read()` to `PREAD64`/`BULK_READ` at `remote.cursor()` and advance only after `write_current_user_bytes` succeeds.
     - Update `write_remote_vfs` to pass `VFS_WRITE_FLAG_CURSOR_SYNC` + `arg0 = remote.cursor()` and apply `remote.set_synced_cursor(response.aux, epoch)`.
     - Update `syscall_linux_vfs_fsync` to settle dirty regular-file cursors via `VFS_CURSOR_SETTLE_SYNC` under the cursor gate before `VFS_IPC_OP_FSYNC`.
   - `kernel/compat/src/user/syscall/linux/service_ops/vfs_meta.rs`: update `syscall_linux_vfs_lseek` to pass `VFS_LSEEK_FLAG_SYNC_CURSOR` + `arg2 = remote.cursor()` and update `remote.set_synced_cursor(response.value, epoch)`.
   - `kernel/compat/src/user/syscall/linux/service_ops/file_ops.rs`: update `sendfile`/`copy_file_range` to read/advance `RemoteVfsOpenDescription` directly under the cursor gate.
   - `kernel/compat/src/user/syscall/linux/service_ops/ipc_helpers.rs`: add `settle_vfs_cursor_sync`; remove `VFS_IPC_OP_READ` from regular-file prepare/commit replay assumptions; preserve `VFS_IPC_OP_CURSOR_SETTLE` deferred reconciliation.
4. **`vfsd` (`services/vfsd/`)**:
   - `services/vfsd/src/state_storage.rs`: remove `VFSD_OPEN_MUTATION_READ` checkpointing for regular-file reads (reject regular-file `VFS_IPC_OP_READ` or handle only inotify/devices).
   - `services/vfsd/src/state_files.rs`: implement `VFS_CURSOR_SETTLE_SYNC` in `vfs_cursor_settle`; honor `VFS_LSEEK_FLAG_SYNC_CURSOR` in `vfs_lseek`; honor `VFS_WRITE_FLAG_CURSOR_SYNC` and populate `response.aux = candidate.cursor` in `vfs_write`.
   - `services/vfsd/src/state_checkpoint.rs` & `services/vfsd/src/lib.rs`: preserve legacy checkpoint decoding and update unit tests.
5. **Formal & Contracts**:
   - `formal/vfs-open-description-recovery/VfsOpenDescriptionRecovery.tla` and `.cfg`.
   - `formal/CONFORMANCE.md`, `formal/system-flows.tsv`, `formal/run-source-conformance.sh`.
   - `docs/ai/abi/vfs-storage.md` (update cursor-read / `CURSOR_SETTLE` contract text).

### Acceptance Tests

- **Host unit tests** (`cargo test -p rustos-user-abi -p kernel-ps -p kernel-compat -p vfsd`):
  1. **Zero checkpoints on sequential read:** mock `rootd` checkpoint transport in `vfsd` asserts `0` checkpoint mutations across N sequential `PREAD64`/`BULK_READ` calls.
  2. **Copy-out fault non-advancement:** failed `copy_to_user` (`EFAULT`) leaves `RemoteVfsOpenDescription.cursor` unchanged and sends `0` `CURSOR_SETTLE` IPCs; partial multi-chunk fault advances by exact `copied`.
  3. **Shared description coherence:** `dup`, `dup2`, `fork`, and `SCM_RIGHTS` transfer observe each other's sequential reads, writes, and `lseek` updates atomically.
  4. **Interleaved read → write / `lseek(SEEK_CUR)` / `fsync`:** `write` writes at post-read kernel cursor; `O_APPEND` writes at EOF and updates kernel cursor from `response.aux`; `lseek(SEEK_CUR, 0)` returns post-read cursor and settles `rootd`; `fsync` flushes dirty cursor once and elides flush when clean.
  5. **`VFS_CURSOR_SETTLE_SYNC` replay idempotency:** replaying the same `(operation_hi, operation_lo)` after simulated `vfsd` restart returns the settled cursor without advancing or failing.
- **Formal verification**:
  - TLC exhaustive check of updated `VfsOpenDescriptionRecovery.tla` (both `kind = "file"` and `kind = "dir"`).
  - `formal/verify-all.sh --profile pr` passes with updated conformance witnesses and system-flow entries.
- **KVM acceptance**:
  - 1-vCPU `kvm-smoke` passes cleanly.
  - Sequential large-file read benchmark confirms elimination of `rootd` checkpoint IPCs on the read hot path.
  - `vfsd` restart fault-injection during sequential file reading verifies zero duplicate bytes, zero skipped bytes, and zero `EBUSY` errors on subsequent `lseek`/`close`.
