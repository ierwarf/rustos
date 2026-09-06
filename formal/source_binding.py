"""The one implementation of the verification-run source binding.

The binding says a sealed verification result corresponds to this exact tree.
It is *written* by `write-verification-run.py`, *reused* by
`reuse-verification-run.py`, *checked* by `check-kvm-runtime-trace.py`, and
compared against by `tools/xtask/src/formal_contracts/evidence.rs`. Four copies
of one hash is four chances for them to disagree, and a disagreement does not
degrade gracefully: every binding check fails at once. The Python copies now
share this module; the Rust one reads the same exemption list.

A file no lane reads is not an input to the result, so hashing it does not
strengthen the claim -- it only makes the seal stale for an edit that cannot
change any answer, and the seal gates `cargo xtask bench`. Exemption is not a
judgement call: `check-binding-exemptions.py` proves nothing under `formal/` or
`tools/` mentions an exempt path. The list is itself tracked and therefore
inside the hash it governs.
"""

from __future__ import annotations

import hashlib
import subprocess
from pathlib import Path

EXEMPT_LIST_RELATIVE = "formal/binding-exempt-paths.txt"


def binding_exempt_paths(root: Path) -> set[str]:
    """Paths excluded from the binding, or an empty set when the list is absent.

    An absent list means the binding covers everything, which is the
    conservative direction.
    """
    listing = root / EXEMPT_LIST_RELATIVE
    if not listing.is_file():
        return set()
    return {
        line.strip()
        for line in listing.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.startswith("#")
    }


def source_tree_sha256(root: Path) -> str:
    """Hash every tracked, non-ignored, non-exempt file, path and bytes alike."""
    exempt = binding_exempt_paths(root)
    output = subprocess.run(
        [
            "git",
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        cwd=root,
        check=True,
        capture_output=True,
    ).stdout
    digest = hashlib.sha256()
    for raw_path in sorted(path for path in output.split(b"\0") if path):
        relative = raw_path.decode("utf-8")
        if relative in exempt:
            continue
        candidate = root / relative
        # `git ls-files --cached` still reports a tracked path deleted by the
        # current change set. Its absence is part of the tree hash; attempting
        # to read it would make evidence generation unable to validate a
        # legitimate source deletion.
        if not candidate.is_file():
            continue
        digest.update(raw_path)
        digest.update(b"\0")
        digest.update(candidate.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()
