#!/usr/bin/env python3
# SPDX-License-Identifier: CC0-1.0
from pathlib import Path
import csv, subprocess, sys
root=Path(sys.argv[1] if len(sys.argv)>1 else './upstream-sources')
rows=[]
for d in sorted(root.iterdir() if root.exists() else []):
    if not (d/'.git').exists(): continue
    def git(*args):
        return subprocess.check_output(['git','-C',str(d),*args], text=True).strip()
    rows.append({'name':d.name,'commit':git('rev-parse','HEAD'),'date':git('show','-s','--format=%cI','HEAD'),'remote':git('remote','get-url','origin')})
with (root/'SNAPSHOT_COMMITS.csv').open('w',newline='',encoding='utf-8') as f:
    w=csv.DictWriter(f,fieldnames=['name','commit','date','remote']); w.writeheader(); w.writerows(rows)
print(f'wrote {len(rows)} revisions')
