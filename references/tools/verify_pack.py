#!/usr/bin/env python3
# SPDX-License-Identifier: CC0-1.0
from __future__ import annotations

import csv
import hashlib
import json
import sys
from pathlib import Path

root = Path(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).resolve().parents[1]).resolve()
errors: list[str] = []

manifest = root / '09_MANIFEST/files.csv'
summary_path = root / '09_MANIFEST/SUMMARY.json'
if not manifest.exists():
    errors.append('missing manifest: 09_MANIFEST/files.csv')
else:
    with manifest.open(encoding='utf-8', newline='') as stream:
        rows = list(csv.DictReader(stream))
    for row in rows:
        path = root / row['path']
        if not path.exists():
            errors.append(f"missing: {row['path']}")
            continue
        data = path.read_bytes()
        digest = hashlib.sha256(data).hexdigest()
        if digest != row['sha256']:
            errors.append(f"hash mismatch: {row['path']}")
        actual_lines = data.count(b'\n') + (1 if data and not data.endswith(b'\n') else 0)
        if actual_lines != int(row['lines']):
            errors.append(f"line mismatch: {row['path']}")
        if len(data) != int(row['bytes']):
            errors.append(f"size mismatch: {row['path']}")

    if summary_path.exists():
        summary = json.loads(summary_path.read_text(encoding='utf-8'))
        if summary.get('files') != len(rows):
            errors.append('SUMMARY files count mismatch')
        if summary.get('bytes') != sum(int(r['bytes']) for r in rows):
            errors.append('SUMMARY byte count mismatch')
        if summary.get('lines') != sum(int(r['lines']) for r in rows):
            errors.append('SUMMARY line count mismatch')
    else:
        errors.append('missing manifest: 09_MANIFEST/SUMMARY.json')

required = [
    '00_START_HERE/README_KO.md',
    '00_START_HERE/LLM_SYSTEM_PROMPT_KO.md',
    '01_LINUX/MAINTAINERS',
    '01_LINUX/docs_process/submitting-patches.rst',
    '01_LINUX/docs_concurrency/memory-barriers.txt',
    '01_LINUX/docs_abi/adding-syscalls.rst',
    '02_DUAL_ABI/DUAL_ABI_DESIGN_GUIDE_KO.md',
    '03_MICROKERNELS/sel4/CAVEATS.md',
    '04_QUBES_XEN/qubes_docs/architecture.rst',
    '05_VERIFICATION/VERIFICATION_LADDER_KO.md',
    '07_ENGINEERING_PRACTICE/OS_COMMON_SENSE_CHECKLIST_KO.md',
    '08_REVIEW_CASES/all_pr_cases.csv',
    '09_MANIFEST/rag_corpus.jsonl',
    'tools/fetch_full_reference_pack.sh',
]
for rel in required:
    if not (root / rel).exists():
        errors.append(f'required missing: {rel}')

junk = [
    path.relative_to(root).as_posix()
    for path in root.rglob('*')
    if path.is_file() and ('__pycache__' in path.parts or path.suffix == '.pyc' or path.name == '.DS_Store')
]
if junk:
    errors.append('junk files present: ' + ', '.join(junk[:10]))

rag = root / '09_MANIFEST/rag_corpus.jsonl'
rag_rows = 0
if rag.exists():
    try:
        with rag.open(encoding='utf-8') as stream:
            for line_no, line in enumerate(stream, 1):
                if not line.strip():
                    continue
                record = json.loads(line)
                for key in ('id', 'path', 'topic', 'chunk_index', 'text'):
                    if key not in record:
                        errors.append(f'RAG missing key {key} at line {line_no}')
                        break
                rag_rows += 1
    except (OSError, json.JSONDecodeError) as exc:
        errors.append(f'RAG parse error: {exc}')
if rag_rows < 100:
    errors.append(f'RAG corpus unexpectedly small: {rag_rows} rows')

if errors:
    print('\n'.join(errors), file=sys.stderr)
    raise SystemExit(1)
print(f'OK: hashes, required files and {rag_rows} RAG chunks verified')
