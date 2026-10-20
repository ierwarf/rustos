#!/usr/bin/env python3
"""Keep owner documents bounded and their local Markdown links navigable."""
from __future__ import annotations

import re
import sys
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[2]
MAX_LINES = 650


def prose_lines(text: str):
    fence = None
    for number, line in enumerate(text.splitlines(), 1):
        marker = re.match(r"^\s{0,3}(`{3,}|~{3,})", line)
        if marker:
            value = marker.group(1)
            if fence is None:
                fence = value
            elif value[0] == fence[0] and len(value) >= len(fence):
                fence = None
            continue
        if fence is None:
            yield number, line


def heading_ids(text: str) -> set[str]:
    result = set()
    counts: dict[str, int] = {}
    for _, line in prose_lines(text):
        result.update(re.findall(r"<a\s+(?:id|name)=[\"']([^\"']+)[\"']", line))
        heading = re.match(r"^#{1,6}\s+(.+?)(?:\s+#+)?$", line)
        if not heading:
            continue
        # GitHub-style IDs retain Unicode word characters, hyphens and
        # underscores, while dropping punctuation and inline formatting.
        title = re.sub(r"<[^>]+>", "", heading.group(1)).lower()
        slug = re.sub(r"[^\w\- ]", "", title).replace(" ", "-")
        count = counts.get(slug, 0)
        counts[slug] = count + 1
        result.add(f"{slug}-{count}" if count else slug)
    return result


def check() -> list[str]:
    errors = []
    documents = sorted([*(ROOT / "docs").rglob("*.md"), *(ROOT / "formal").rglob("*.md")])
    for path in documents:
        text = path.read_text()
        relative = path.relative_to(ROOT)
        count = len(text.splitlines())
        if count > MAX_LINES:
            errors.append(f"{relative}: {count} lines > {MAX_LINES}; split by owner, retaining entrypoint links")
        for number, line in prose_lines(text):
            line = re.sub(r"(`+).*?\1", "", line)
            for target in re.findall(r"\]\(([^\s)]+)(?:\s+\"[^\"]*\")?\)", line):
                url = urlsplit(target)
                if url.scheme or url.netloc:
                    continue
                destination = (path.parent / unquote(url.path)).resolve() if url.path else path
                if not destination.exists():
                    errors.append(f"{relative}:{number}: absent link target {target}")
                elif url.fragment and destination.suffix == ".md":
                    if unquote(url.fragment) not in heading_ids(destination.read_text()):
                        errors.append(f"{relative}:{number}: absent heading {target}")
    return errors


def selftest():
    assert heading_ids("# ABI `X` (v1)\n# ABI `X` (v1)\n") == {"abi-x-v1", "abi-x-v1-1"}
    assert heading_ids("```md\n# hidden\n```\n## Visible\n") == {"visible"}
    assert list(prose_lines("~~~\n[x](missing.md)\n~~~\ntext")) == [(4, "text")]
    assert heading_ids('<a id="korean"></a>\n## 한국어\n') == {"korean", "한국어"}
    print("documentation layout selftest passed")


if __name__ == "__main__":
    if sys.argv[1:] == ["--selftest"]:
        selftest()
    elif sys.argv[1:]:
        raise SystemExit("usage: check-doc-layout.py [--selftest]")
    else:
        errors = check()
        for error in errors[:20]:
            print(error, file=sys.stderr)
        if errors:
            raise SystemExit(f"documentation layout: {len(errors)} error(s)")
        print(f"documentation layout passed: owner pages <= {MAX_LINES} lines; local links and headings resolve")
