#!/usr/bin/env python3
# SPDX-License-Identifier: CC0-1.0
from __future__ import annotations

import hashlib
import json
import re
import sys
from pathlib import Path

root = Path(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).resolve().parents[1]).resolve()
out = Path(sys.argv[2] if len(sys.argv) > 2 else root / '09_MANIFEST/rag_corpus.jsonl').resolve()
max_chars = 1400
overlap = 180
binary_suffixes = {'.zip', '.gz', '.tgz', '.png', '.jpg', '.jpeg', '.gif', '.webp', '.pdf', '.pyc'}
generated_meta = {
    '09_MANIFEST/rag_corpus.jsonl',
    '09_MANIFEST/files.csv',
    '09_MANIFEST/files.json',
    '09_MANIFEST/SHA256SUMS',
    '09_MANIFEST/SUMMARY.json',
}


def topic(path: Path) -> str:
    p = path.as_posix()
    for prefix, name in [
        ('01_LINUX', 'linux'),
        ('02_DUAL_ABI', 'dual_abi'),
        ('03_MICROKERNELS', 'microkernel'),
        ('04_QUBES_XEN', 'qubes_xen'),
        ('05_VERIFICATION', 'verification'),
        ('06_SPECS', 'specification'),
        ('07_ENGINEERING_PRACTICE', 'engineering'),
        ('08_REVIEW_CASES', 'review_cases'),
        ('09_MANIFEST', 'source_catalog'),
    ]:
        if p.startswith(prefix):
            return name
    return 'meta'


def chunks(text: str):
    text = text.replace('\r\n', '\n').replace('\r', '\n')
    paragraphs = [p.strip() for p in re.split(r'\n{2,}', text) if p.strip()]
    current = ''
    for paragraph in paragraphs:
        if len(current) + len(paragraph) + 2 <= max_chars:
            current += ('\n\n' if current else '') + paragraph
            continue
        if current:
            yield current
        while len(paragraph) > max_chars:
            yield paragraph[:max_chars]
            paragraph = paragraph[max_chars - overlap:]
        current = paragraph
    if current:
        yield current


def eligible(path: Path) -> bool:
    if not path.is_file() or path.resolve() == out:
        return False
    rel = path.relative_to(root).as_posix()
    if rel in generated_meta:
        return False
    if '__pycache__' in path.parts or path.name == '.DS_Store':
        return False
    return path.suffix.lower() not in binary_suffixes


out.parent.mkdir(parents=True, exist_ok=True)
rows = 0
with out.open('w', encoding='utf-8', newline='\n') as stream:
    for path in sorted(root.rglob('*')):
        if not eligible(path):
            continue
        try:
            text = path.read_text(encoding='utf-8')
        except (UnicodeDecodeError, OSError):
            continue
        rel = path.relative_to(root)
        for index, chunk in enumerate(chunks(text)):
            record = {
                'id': hashlib.sha256((rel.as_posix() + f':{index}').encode()).hexdigest()[:20],
                'path': rel.as_posix(),
                'topic': topic(rel),
                'chunk_index': index,
                'text': chunk,
            }
            stream.write(json.dumps(record, ensure_ascii=False, sort_keys=True) + '\n')
            rows += 1
print(f'wrote {rows} chunks to {out}')
