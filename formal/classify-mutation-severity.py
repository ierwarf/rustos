#!/usr/bin/env python3
"""Propose severities for formal/implementation-mutations.tsv.

Read-only by default: the TSV is modified only by the explicit --apply mode. The proposal is printed as TSV
(id, current, proposed, reason) to stdout, or to the file given as the
argument, and a distribution summary goes to stderr.

The rubric is docs/ai/formal-scope-policy.md ("Mutation severity rubric"):

  critical  a surviving mutant lets input an unprivileged principal controls
            (a Ring3 process, a file or network byte, a device) cause memory
            unsafety, privilege or authority escalation, isolation breakage,
            data loss or corruption, or a kernel panic/unrecoverable hang
  high      the same class of failure but only from a trusted-but-fallible
            source (firmware, signed boot image, an authenticated service), or
            a kernel-internal invariant whose violation stalls or mis-schedules
            the system without a direct unprivileged trigger
  medium    wrong accounting, ordering, budgets or restart policy with bounded
            blast radius; the system degrades but stays safe and recoverable
  low       diagnostics, tracing, readiness evidence, benchmarks and harness
            behaviour: wrong output, no safety consequence

The classification is mechanical: a surface tier from the mutated source path
and a safety tier from words in the mutation id, combined by RULES below. It
is a first proposal for human review; `reason` names the rule that fired.
Standard library only.
"""
from __future__ import annotations

import argparse
import re
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TSV = ROOT / "formal/implementation-mutations.tsv"
ORDER = ["low", "medium", "high", "critical"]

# Surface tier: how directly unprivileged input reaches the mutated code.
SURFACES = [
    # (path prefix regex, surface, description)
    (r"^(kernel/compat|kernel/mm|kernel/object|kernel/ipc-runtime|libs/rustos-user-abi|libs/rustos-image-admission)/",
     "boundary", "syscall, memory, capability, IPC or ABI boundary"),
    (r"^services/(loaderd|vfsd|netd|syscalld|pagerd)/", "boundary", "unprivileged-facing service"),
    (r"^(kernel/ps|kernel/hal|kernel/io-manager|kernel/nucleus-core|kernel/executive|libs/rustos-scheduler-frame-gate)/",
     "kernel", "kernel-internal mechanism"),
    (r"^(services/(rootd|initd|runtimed|uiserver|inputd)|libs/(runtime-control|driver-domain-host|rustos-svc-runtime))/",
     "service", "privileged service policy"),
    (r"^(tools|apps)/", "harness", "host tooling or benchmark"),
]

# Words in the mutation id that mark a safety property (case-insensitive,
# matched against hyphen separated tokens).
SAFETY = re.compile(
    r"(authority|capability|cap|generation|stale|forged|uaf|use-after|double|overflow|unchecked|bound|bounds|"
    r"wx|aliasing|alias|isolation|revoke|revoked|leak|panic|torn|owner|ownership|privilege|escalat|"
    r"permission|uid|gid|signature|digest|sealed|seal|zero-fill|zeroing|copy|user-copy|uaccess|"
    r"truncat|corrupt|lost|dangling|unmapped|frame|refcount|retire|retired|teardown|exit|kill|"
    r"deadlock|unlock|lock|publish|publication|claim|reply|donat|inherit|abi|decode|"
    r"wake|notif|ipi|stall|hang|starv|spin|timeout|deadline|never|ignored|refus|drop|grant|skip|missing|release)"
)
# Words that mark output-only behaviour.
DIAGNOSTIC = re.compile(
    r"(diag|trace|log|stats|metric|counter|milestone|bench|evidence|readiness|report|print|banner|summary)"
)
# Sources that carry only evidence or harness logic regardless of the id.
EVIDENCE_PATHS = re.compile(r"^(tools/xtask/src/kvm|apps/)")


def rows(tsv: Path = TSV):
    with tsv.open(encoding="utf-8") as handle:
        for raw in handle:
            raw = raw.rstrip("\n")
            if not raw or raw.startswith("#"):
                continue
            fields = raw.split("\t")
            if len(fields) != 11:
                raise SystemExit(f"{tsv}: malformed row {fields[0]!r} ({len(fields)} fields)")
            yield fields


def surface_of(source: str):
    for pattern, name, description in SURFACES:
        if re.match(pattern, source):
            return name, description
    return "kernel", "unclassified source (treated as kernel-internal)"


def classify(ident: str, source: str):
    surface, description = surface_of(source)
    safety = bool(SAFETY.search(ident))
    diagnostic = bool(DIAGNOSTIC.search(ident)) and not safety
    if EVIDENCE_PATHS.match(source) and not safety:
        return "low", f"harness/evidence source {source} with no safety token in the id"
    if surface == "harness":
        return ("medium", "host tooling with a safety token (staging, admission or seal logic)") if safety else (
            "low", "host tooling or benchmark without a safety token")
    if diagnostic:
        return "low", f"{description}; id names output-only behaviour"
    if surface == "boundary":
        if safety:
            return "critical", f"{description}; id names a safety property"
        return "high", f"{description}; no explicit safety token"
    if surface == "kernel":
        if safety:
            return "high", f"{description}; id names a safety property without a direct unprivileged trigger"
        return "medium", f"{description}; no explicit safety token"
    if safety:
        return "high", f"{description}; id names a safety property"
    return "medium", f"{description}; no explicit safety token"


def read_proposal(path: Path):
    """Parse a proposal TSV (id, current, proposed, reason) into id -> (current, proposed)."""
    proposal = {}
    for number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not raw or raw.startswith("#"):
            continue
        fields = raw.split("\t")
        if len(fields) != 4:
            raise SystemExit(f"{path}:{number}: expected 4 columns, got {len(fields)}")
        ident, current, proposed, _reason = fields
        if current not in ORDER or proposed not in ORDER:
            raise SystemExit(f"{path}:{number}: unknown severity in {current!r} -> {proposed!r}")
        if ident in proposal:
            raise SystemExit(f"{path}:{number}: duplicate id {ident}")
        proposal[ident] = (current, proposed)
    return proposal


def apply_proposal(tsv: Path, proposal) -> tuple[int, int]:
    """Rewrite ONLY the severity column of `tsv` in place; return (changed, untouched).

    Every other byte, the row order and the line endings are preserved: the file
    is processed as bytes split on b"\\n" only. The whole proposal is checked
    against the file before anything is written (unknown id, or a `current`
    that no longer matches, aborts with the file untouched), and the result is
    verified column by column before it replaces the original.
    """
    original = tsv.read_bytes()
    lines = original.split(b"\n")
    seen = set()
    changed = untouched = 0
    result = []
    for number, line in enumerate(lines, 1):
        if not line or line.startswith(b"#"):
            result.append(line)
            continue
        head = line.split(b"\t", 2)
        if len(head) != 3:
            raise SystemExit(f"{tsv}:{number}: malformed row")
        ident = head[0].decode("utf-8")
        entry = proposal.get(ident)
        if entry is None:
            untouched += 1
            result.append(line)
            continue
        current, proposed = entry
        if head[1].decode("utf-8") != current:
            raise SystemExit(
                f"{tsv}:{number}: {ident} is {head[1].decode()!r}, proposal expected {current!r}; "
                "regenerate the proposal against the current file"
            )
        seen.add(ident)
        if proposed != current:
            changed += 1
        result.append(b"\t".join([head[0], proposed.encode("utf-8"), head[2]]))
    missing = sorted(set(proposal) - seen)
    if missing:
        raise SystemExit(
            f"proposal names {len(missing)} ids absent from {tsv}: " + ", ".join(missing[:5])
        )
    rewritten = b"\n".join(result)
    # Verify: identical except for the second column of rows with a proposal.
    old_lines, new_lines = original.split(b"\n"), rewritten.split(b"\n")
    assert len(old_lines) == len(new_lines)
    for old, new in zip(old_lines, new_lines):
        if old == new:
            continue
        old_head, new_head = old.split(b"\t", 2), new.split(b"\t", 2)
        assert old_head[0] == new_head[0] and old_head[2] == new_head[2], "non-severity byte changed"
    temporary = tsv.with_name(tsv.name + ".severity.tmp")
    temporary.write_bytes(rewritten)
    temporary.chmod(tsv.stat().st_mode & 0o7777)
    temporary.replace(tsv)
    return changed, untouched


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("output", nargs="?", help="write the proposal TSV here (default stdout)")
    parser.add_argument("--tsv", type=Path, default=TSV, help="registry to read or apply to")
    parser.add_argument(
        "--apply",
        action="store_true",
        help="rewrite ONLY the severity column of --tsv from the proposal (computed now, or --proposal)",
    )
    parser.add_argument("--proposal", type=Path, help="with --apply: use this earlier proposal TSV")
    args = parser.parse_args()
    if args.proposal and not args.apply:
        parser.error("--proposal requires --apply")
    if args.apply and args.output:
        parser.error("--apply writes the registry, not an output file")

    current, proposed, moves = Counter(), Counter(), Counter()
    lines = ["# id\tcurrent\tproposed\treason"]
    computed = {}
    for fields in rows(args.tsv):
        ident, severity, source = fields[0], fields[1], fields[2]
        new, reason = classify(ident, source)
        current[severity] += 1
        proposed[new] += 1
        moves[(severity, new)] += 1
        computed[ident] = (severity, new)
        lines.append(f"{ident}\t{severity}\t{new}\t{reason}")
    total = sum(current.values())
    print(f"rows={total}", file=sys.stderr)
    print("current :", ", ".join(f"{k}={current[k]}" for k in reversed(ORDER) if current[k]), file=sys.stderr)
    print("proposed:", ", ".join(f"{k}={proposed[k]}" for k in reversed(ORDER) if proposed[k]), file=sys.stderr)
    for (old, new), count in sorted(moves.items()):
        print(f"  {old} -> {new}: {count}", file=sys.stderr)
    if args.apply:
        proposal = read_proposal(args.proposal) if args.proposal else computed
        changed, untouched = apply_proposal(args.tsv, proposal)
        print(f"applied: severity changed on {changed} rows, {untouched} rows without a proposal left as is", file=sys.stderr)
        return 0
    text = "\n".join(lines) + "\n"
    if args.output:
        Path(args.output).write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
