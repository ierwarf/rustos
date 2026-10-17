#!/usr/bin/env python3
"""Each missing native Identity namespace guard must fail source refinement.

Faults are injected into reads only. Never edit or race the live source tree or
relax actual Identity/resource admission. Concurrent actors inside each native
owner witness remain unchanged.
"""
import contextlib
import io
from pathlib import Path
import runpy
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
CHECKER = ROOT / "formal/check-dispatch-source-contracts.py"
GUARD = "let _namespace = crate::multitask::scheduler::execution_test_serial_guard();"
CASES = {
    "kernel/ps/src/multitask/current_identity.rs": [
        "published_identity_round_trips_and_clears",
        "dispatch_lifecycle_is_committed_with_identity",
        "kernel_task_publishes_no_user_binding",
        "incomplete_user_or_odd_publication_requires_locked_log_lookup",
    ],
    "kernel/ps/src/multitask/current_identity/owner_tests.rs": [
        "metadata_republication_cannot_erase_newer_lifecycle_intent",
        "absent_partial_metadata_preserves_pending_start_and_stop_intent",
        "stale_live_command_cannot_stop_or_resume_a_recycled_identity",
        "concurrent_metadata_and_gate_writers_do_not_lose_each_others_fields",
    ],
}
original_read = Path.read_text
for source, names in CASES.items():
    source_path = ROOT / source
    pristine = original_read(source_path)
    for name in names:
        start = pristine.index("fn " + name + "() {")
        at = pristine.index(GUARD, start)
        corrupted = pristine[:at] + pristine[at:].replace(GUARD, "", 1)

        def read_with_fault(path, *args, **kwargs):
            if path.resolve() == source_path:
                return corrupted
            return original_read(path, *args, **kwargs)

        with patch.object(Path, "read_text", read_with_fault), contextlib.redirect_stdout(io.StringIO()):
            try:
                runpy.run_path(str(CHECKER), run_name="__main__")
            except SystemExit as error:
                expected = "standalone original Identity fixture namespace: " + name
                if expected not in str(error):
                    raise AssertionError(f"{name}: unrelated refusal: {error}") from error
            else:
                raise AssertionError(f"{name}: unisolated identity publisher survived")
print("dispatch native namespace guard regressions passed count=8")
