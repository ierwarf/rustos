# Verus proof kernel

These are small, unbounded mathematical theorems for state partitions that are
too large for a useful finite TLC horizon. They are not compiled into RustOS.
Each theorem must name an executable counterpart, which remains covered by a
Kani proof or focused Rust test.

`runtime_response.rs` matches `libs/runtime-control::response_payload_len`:
successful response identity is exact, snapshots alone have bounded payload,
and malformed statuses cannot become success. Run it through the pinned wrapper:

    bash formal/setup-verus.sh   # once per pinned release
    bash formal/run-verus.sh
