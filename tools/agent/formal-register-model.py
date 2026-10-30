#!/usr/bin/env python3
"""Register a TLA+ model in every formal registry in one step.

A new model touches nine files whose rules are checked only by later gates (sorted
ids, unique REQ/HAZ numbers, matching flows, the witness catalogue). Doing that by
hand produced one failure per file. This tool writes all of them from one set of
arguments, using the same insertion rules the registries already follow, and
prints exactly what it changed.

  tools/agent/formal-register-model.py dir/Module --flow FLOW \
      --transition ID --event TEXT --binding TEXT --coverage TEXT \
      --conformance-owner TEXT --conformance TEXT --hazard TEXT \
      --source PATH --witness-package PKG --witness-test PATH::TEST [--skeleton] [--dry-run]

It does not write the model's real invariants or mutants: those are the design. With
--skeleton it creates a minimal passing .tla/.cfg pair to start from. Afterwards run
`tools/agent/formal-preflight.sh`.
"""
import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def die(message: str) -> "None":
    print(f"formal-register-model: {message}", file=sys.stderr)
    sys.exit(1)


def read(path: Path) -> str:
    return path.read_text()


def insert_after_block(lines: list, matcher, new_line: str, what: str) -> None:
    """Insert `new_line` after the contiguous run of lines around the first match."""
    first = next((i for i, line in enumerate(lines) if matcher(line)), None)
    if first is None:
        die(f"no anchor row found in {what}")
    end = first
    while end + 1 < len(lines) and lines[end + 1].startswith("|"):
        end += 1
    lines.insert(end + 1, new_line)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("model", help="dir/Module, e.g. job-limits/JobLimits")
    ap.add_argument("--flow", required=True, help="system flow id, e.g. process-lifecycle-transaction")
    ap.add_argument("--transition", required=True, help="new system-flows transition id")
    ap.add_argument("--event", required=True, help="hyphenated event text for the flow row")
    ap.add_argument("--binding", required=True, help="model-bindings rationale sentence")
    ap.add_argument("--models-md", required=True, help="models.md: owner / what it checks")
    ap.add_argument("--coverage", required=True, help="COVERAGE.md hazard sentence")
    ap.add_argument("--conformance-owner", required=True)
    ap.add_argument("--conformance", required=True)
    ap.add_argument("--source", required=True, help="production source file of the flow row")
    ap.add_argument("--witness-package", required=True)
    ap.add_argument("--witness-test", required=True, help="exact test path, no ::")
    ap.add_argument("--severity", default="critical")
    ap.add_argument("--owner", default=None, help="flow owner; defaults to the witness package")
    ap.add_argument("--deadlock-reason", default="an explicit stutter step is always enabled")
    ap.add_argument("--skeleton", action="store_true", help="create a minimal passing .tla/.cfg")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--root", default=str(ROOT), help="repository root (for tests)")
    args = ap.parse_args()

    root = Path(args.root)
    formal = root / "formal"
    if not re.fullmatch(r"[a-z0-9-]+/[A-Za-z0-9]+", args.model):
        die("model must look like dir/Module")
    directory, module = args.model.split("/")
    short = directory
    if (formal / "models.tsv").read_text().count(f"\n{args.model}\t") or \
            read(formal / "models.tsv").startswith(args.model + "\t"):
        die(f"{args.model} is already registered")

    changes = {}

    def edit(path: Path, new_text: str) -> None:
        changes[path] = new_text

    # models.tsv: model class deadlock reason pr nightly mode apalache tlaps trace
    text = read(formal / "models.tsv").rstrip("\n")
    edit(formal / "models.tsv",
         text + f"\n{args.model}\tsafety\tcheck\t{args.deadlock_reason}\t30\t120\texhaustive\tno\tno\tno\n")

    # model-bindings.tsv
    text = read(formal / "model-bindings.tsv").rstrip("\n")
    edit(formal / "model-bindings.tsv",
         text + f"\n{args.model}\trefinement\t{args.flow}\t{args.binding}\n")

    # Markdown tables: insert after the table that already lists registered models.
    known = [row.split("\t")[0] for row in read(formal / "models.tsv").splitlines()
             if row and not row.startswith("#")]
    known_short = {m.split("/")[0] for m in known}

    lines = read(formal / "models.md").split("\n")
    insert_after_block(lines, lambda l: l.startswith("| ") and l.split("|")[1].strip() in known,
                       f"| {args.model} | {args.models_md} |", "models.md")
    edit(formal / "models.md", "\n".join(lines))

    lines = read(formal / "CONFORMANCE.md").split("\n")
    insert_after_block(lines, lambda l: l.startswith("| ") and l.split("|")[1].strip() in known_short,
                       f"| {short} | {args.conformance_owner} | {args.conformance} |", "CONFORMANCE.md")
    edit(formal / "CONFORMANCE.md", "\n".join(lines))

    lines = read(formal / "COVERAGE.md").split("\n")
    insert_after_block(lines,
                       lambda l: l.startswith("| ") and len(l.split("|")) > 3 and l.split("|")[2].strip() in known_short,
                       f"| {args.coverage} | {short} | {args.source} |", "COVERAGE.md")
    edit(formal / "COVERAGE.md", "\n".join(lines))

    # system-flows.tsv: a unique, increasing REQ/HAZ pair within the flow's prefix.
    flows = read(formal / "system-flows.tsv")
    prefixes = {}
    for row in flows.splitlines():
        cells = row.split("\t")
        if len(cells) > 3 and cells[0] == args.flow:
            m = re.fullmatch(r"REQ-([A-Z]+)-(\d+)", cells[2])
            if m:
                prefixes[m.group(1)] = max(prefixes.get(m.group(1), 0), int(m.group(2)))
    if len(prefixes) != 1:
        die(f"flow {args.flow} has no single REQ prefix; add the row by hand")
    (prefix, highest), = prefixes.items()
    used = set(re.findall(r"(?:REQ|HAZ)-[A-Z]+-\d+", flows))
    number = highest + 1
    while f"REQ-{prefix}-{number:03d}" in used or f"HAZ-{prefix}-{number:03d}" in used:
        number += 1
    owner = args.owner or args.witness_package
    row = "\t".join([
        args.flow, args.transition, f"REQ-{prefix}-{number:03d}", f"HAZ-{prefix}-{number:03d}",
        args.severity, owner, "exiting", args.event, "exiting", "continue", "0",
        args.model, args.source, args.witness_package, args.witness_test,
    ])
    edit(formal / "system-flows.tsv", flows.rstrip("\n") + "\n" + row + "\n")

    # run-source-conformance.sh witness catalogue (a heredoc of model|package|test).
    script = read(formal / "run-source-conformance.sh").split("\n")
    witness = f"{args.model}|{args.witness_package}|{args.witness_test}"
    indexes = [i for i, l in enumerate(script) if re.fullmatch(r"[a-z0-9-]+/[A-Za-z0-9]+\|[a-z0-9-]+\|.+", l)]
    if not indexes:
        die("witness catalogue not found in run-source-conformance.sh")
    script.insert(indexes[-1] + 1, witness)
    edit(formal / "run-source-conformance.sh", "\n".join(script))

    # spec-mutations.toml: required_models stays sorted and unique.
    toml = read(formal / "spec-mutations.toml")
    m = re.search(r"required_models = \[\n(.*?)\n\]", toml, re.S)
    if not m:
        die("required_models not found in spec-mutations.toml")
    items = sorted(set(re.findall(r'"([^"]+)"', m.group(1))) | {args.model})
    block = "required_models = [\n" + "".join(f'    "{i}",\n' for i in items) + "]"
    edit(formal / "spec-mutations.toml", toml[:m.start()] + block + toml[m.end():])

    if args.skeleton:
        tla = formal / directory / f"{module}.tla"
        cfg = formal / directory / f"{module}.cfg"
        if tla.exists() or cfg.exists():
            die("skeleton target already exists")
        edit(tla, f"""-------------------- MODULE {module} --------------------
EXTENDS Naturals

(* TODO: owners, state, actions, and the invariants that make this model mean something. *)

VARIABLES state
vars == <<state>>

TypeOK == state \\in 0..1
Init == state = 0
Next == state' = state \\* explicit stutter: replace with the real actions
Spec == Init /\\ [][Next]_vars

=============================================================================
""")
        edit(cfg, "SPECIFICATION Spec\n\nINVARIANTS\n    TypeOK\n")

    for path, new_text in changes.items():
        rel = path.relative_to(root)
        old = path.read_text() if path.exists() else ""
        delta = len(new_text.splitlines()) - len(old.splitlines())
        print(f"{'would write' if args.dry_run else 'wrote'} {rel} ({delta:+d} lines)")
        if not args.dry_run:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(new_text if new_text.endswith("\n") else new_text + "\n")
    if not args.dry_run:
        # The generated contract document must match the registries, or the next
        # gate fails for a reason that has nothing to do with the model.
        import subprocess
        subprocess.run(["cargo", "xtask", "formal-contracts", "generate"], cwd=root,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
    print(f"REQ/HAZ-{prefix}-{number:03d} assigned and docs/ai/formal-contracts.generated.md regenerated. "
          "Still yours: write the model, add at least one spec mutant (the registry requires one per "
          "model) and implementation mutants, then run tools/agent/formal-preflight.sh")


if __name__ == "__main__":
    main()
