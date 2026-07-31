#!/usr/bin/env python3
"""Fetch Linux Patchwork samples by state without third-party Python packages.
SPDX-License-Identifier: CC0-1.0

Examples:
  python3 fetch_linux_patchwork_cases.py --project netdevbpf --per-state 50
  python3 fetch_linux_patchwork_cases.py --list-projects

Patchwork states are workflow metadata, not a ground-truth label of technical quality.
"""
from __future__ import annotations
import argparse, json, sys, urllib.parse, urllib.request
from pathlib import Path

BASE='https://patchwork.kernel.org/api/1.2'

def get(path, params=None):
    url=BASE+path
    if params: url += '?' + urllib.parse.urlencode(params)
    req=urllib.request.Request(url,headers={'User-Agent':'os-ai-reference-pack/1.0'})
    with urllib.request.urlopen(req,timeout=30) as r: return json.load(r)

def all_pages(path, params, limit):
    out=[]; page=1
    while len(out)<limit:
        p=dict(params); p.update({'page':page,'per_page':min(100,limit-len(out))})
        data=get(path,p)
        if not data: break
        out.extend(data); page+=1
    return out[:limit]

def main():
    ap=argparse.ArgumentParser()
    ap.add_argument('--project',default='netdevbpf',help='Patchwork project linkname')
    ap.add_argument('--per-state',type=int,default=30)
    ap.add_argument('--output',default='linux_patchwork_cases.jsonl')
    ap.add_argument('--list-projects',action='store_true')
    args=ap.parse_args()
    projects=get('/projects/',{'per_page':100})
    if args.list_projects:
        for p in projects: print(p.get('link_name') or p.get('linkname'), p.get('name'))
        return
    proj=next((p for p in projects if (p.get('link_name') or p.get('linkname'))==args.project),None)
    if not proj: raise SystemExit(f'project not found: {args.project}')
    states=get('/states/',{'per_page':100})
    wanted={'accepted','rejected','superseded','changes-requested','under-review','new'}
    rows=[]
    for st in states:
        slug=(st.get('slug') or st.get('name','').lower().replace(' ','-'))
        if slug not in wanted: continue
        patches=all_pages('/patches/',{'project':proj['id'],'state':st['id'],'order':'-date'},args.per_state)
        for p in patches:
            rows.append({'project':args.project,'state':slug,'id':p.get('id'),'name':p.get('name'),'date':p.get('date'),'web_url':p.get('web_url'),'mbox':p.get('mbox'),'series':p.get('series'),'submitter':p.get('submitter')})
    with Path(args.output).open('w',encoding='utf-8') as f:
        for row in rows: f.write(json.dumps(row,ensure_ascii=False)+'\n')
    print(f'wrote {len(rows)} cases to {args.output}',file=sys.stderr)
if __name__=='__main__': main()
