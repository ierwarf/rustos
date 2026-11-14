# Custody and revoke patterns

Three subsystems revoke authority from a holder that may still be running.
They look alike but are deliberately separate. Evidence for each claim is the
named source file; formal state lives in `formal/mm-request-lease` and
`formal/app-buffer-custody`.

| Pattern | Authority token | Revoke step | Drain wait condition | Waiting style |
| --- | --- | --- | --- | --- |
| MM request lease (`kernel/compat/src/user/syscall/linux/mm_request_lease.rs`) | Lock-free ticket slot (`SLOTS[256]`); ticket echoed by `syscalld`, validated against pid | CAS ticket -> `BUSY_SENTINEL` (SeqCst), `retire_and_drain` | `appliers` counter reaches 0 | Blocking: owner `Drop` yields (`yield_now`) until drained; `reclaim_dead` spins for dead owners |
| DMA-BUF custody (`kernel/ps/src/user/handles/device_buffer.rs`, `kernel/compat/src/user/syscall/linux/app_buffer_broker_ops.rs`, `kernel/io-manager/src/io/dvm_display/app_buffer.rs`) | `BufferCapability` holding `Arc<Allocation>`; `mappings` word | `fetch_or(REVOKED)` closes new mappings/exports; low bits are live mapping holds | Mapping holds drop to 0, provider acknowledges retirement (`provider_retired`), `Arc` refs released, then scrub and `reclaim_after_scrub` | Polled, never blocking: broker returns `EAGAIN` until the transport `take` completes |
| Library objects (`kernel/compat/src/user/syscall/linux/library_object_ops.rs`, `kernel/compat/src/library_cache.rs`) | Generation-exact handle (slot + generation; slots never reuse a generation) | `registry.retire` under the lock | Lock exclusion: the single `REGISTRY` mutex serializes every op, so retire is synchronous | None; frame-source reclaim runs after the lock is released |

## Lock order

```
SERVICE_ENDPOINT_REGISTRY_MUTATION  ->  { REGISTRY
                                        | MAILBOX -> APP_BUFFER_POOL }
```

- `with_owned_service_lease` (`ipc_ops.rs`) holds `SERVICE_ENDPOINT_REGISTRY_MUTATION`
  for the closure; the library path takes `REGISTRY` inside it. The closure is
  bounded: no IPC, no user copy, no process-state lock.
- The app-buffer broker `STATE` lock is taken *outside* the service lease
  (`with_devmgrd_publication`), never inside it. `transport::begin` takes
  `MAILBOX` and then the pool lock (`app_buffer_pool()`) while still holding
  `MAILBOX`.
- MM request lease slots take none of these locks. Forbidden: any IPC wait or
  process-state lock while holding a slot claim (`ApplyGuard`).
- Reclaim and scrub happen with no lock held (`reclaim_retired`, library
  `reclaim_frame_sources`).

## Decision: no generic `Lease<T>`

These are deliberately NOT unified behind a generic `Lease<T>`:

1. The drain wait condition is the essential part and differs: a counter spin
   (`appliers == 0`), lock exclusion (`REGISTRY` mutex), or provider ack plus
   `Arc`/mapping-hold refcount. A shared trait would only wrap the one line
   that varies and hide the part that matters for safety.
2. A blocking drain under the DMA-BUF or library locks is unsafe: the party
   being waited on (provider, pager) may need those locks or the service lease
   to finish. Only the MM lease drains by blocking, and only because its
   appliers are bounded kernel mutations that take no such lock.
3. The two TLA+ specs (`MmRequestLease`, `AppBufferCustody`) have disjoint
   state; a common abstraction would couple models that prove different
   invariants and enlarge both.

Within the MM lease the retire-CAS + drain + free sequence is one helper
(`retire_and_drain`), shared by owner `Drop` and `reclaim_dead`.

## When to reconsider

Revisit if a fourth subsystem needs the same counter-drain (retire a token,
then wait for an in-flight applier counter to reach zero). Two copies of
that shape would justify extracting the counter-drain slot type, not a full
generic lease. Until then keep per-subsystem code and per-subsystem specs.
