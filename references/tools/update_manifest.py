#!/usr/bin/env python3
# SPDX-License-Identifier: CC0-1.0
from __future__ import annotations

import csv
import hashlib
import json
import sys
from pathlib import Path

root = Path(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).resolve().parents[1]).resolve()
outdir = root / '09_MANIFEST'
outdir.mkdir(parents=True, exist_ok=True)
excluded = {
    '09_MANIFEST/files.csv',
    '09_MANIFEST/files.json',
    '09_MANIFEST/SHA256SUMS',
    '09_MANIFEST/SUMMARY.json',
}

rows: list[dict[str, object]] = []
for path in sorted(root.rglob('*')):
    if not path.is_file():
        continue
    rel = path.relative_to(root).as_posix()
    if rel in excluded or '__pycache__' in path.parts or path.suffix == '.pyc' or path.name == '.DS_Store':
        continue
    data = path.read_bytes()
    lines = data.count(b'\n') + (1 if data and not data.endswith(b'\n') else 0)
    rows.append({
        'path': rel,
        'bytes': len(data),
        'lines': lines,
        'sha256': hashlib.sha256(data).hexdigest(),
    })

with (outdir / 'files.csv').open('w', newline='', encoding='utf-8') as stream:
    writer = csv.DictWriter(stream, fieldnames=['path', 'bytes', 'lines', 'sha256'])
    writer.writeheader()
    writer.writerows(rows)
(outdir / 'files.json').write_text(json.dumps(rows, indent=2, ensure_ascii=False) + '\n', encoding='utf-8')
(outdir / 'SHA256SUMS').write_text(
    ''.join(f"{row['sha256']}  {row['path']}\n" for row in rows),
    encoding='utf-8',
)
summary = {
    'manifest_version': 2,
    'files': len(rows),
    'bytes': sum(int(row['bytes']) for row in rows),
    'lines': sum(int(row['lines']) for row in rows),
}
(outdir / 'SUMMARY.json').write_text(json.dumps(summary, indent=2, sort_keys=True) + '\n', encoding='utf-8')
print(summary)
