#!/usr/bin/env python3
"""Fetch full public PR metadata/comments/reviews for cases.csv.
SPDX-License-Identifier: CC0-1.0
Use GITHUB_TOKEN to avoid low unauthenticated rate limits.
"""
from __future__ import annotations
import argparse, csv, json, os, time, urllib.request
from pathlib import Path

def get(url, token):
    headers={'Accept':'application/vnd.github+json','User-Agent':'os-ai-reference-pack/1.0','X-GitHub-Api-Version':'2022-11-28'}
    if token: headers['Authorization']=f'Bearer {token}'
    req=urllib.request.Request(url,headers=headers)
    with urllib.request.urlopen(req,timeout=45) as r:
        data=json.load(r); links=r.headers.get('Link','')
    return data,links

def paged(url,token,max_pages=20):
    out=[]; sep='&' if '?' in url else '?'; url=f'{url}{sep}per_page=100'
    for _ in range(max_pages):
        data,links=get(url,token); out.extend(data if isinstance(data,list) else [data])
        nxt=None
        for part in links.split(','):
            if 'rel="next"' in part: nxt=part[part.find('<')+1:part.find('>')]
        if not nxt: break
        url=nxt
    return out

def main():
    ap=argparse.ArgumentParser(); ap.add_argument('csv'); ap.add_argument('--out',default='pr_threads'); ap.add_argument('--limit',type=int,default=0)
    a=ap.parse_args(); token=os.getenv('GITHUB_TOKEN',''); out=Path(a.out); out.mkdir(parents=True,exist_ok=True)
    rows=list(csv.DictReader(open(a.csv,encoding='utf-8'))); rows=rows[:a.limit or None]
    for i,r in enumerate(rows,1):
        repo=r['repository']; n=r['number']; base=f'https://api.github.com/repos/{repo}'
        payload={'case':r}
        payload['pull'],_=get(f'{base}/pulls/{n}',token)
        payload['issue_comments']=paged(f'{base}/issues/{n}/comments',token)
        payload['reviews']=paged(f'{base}/pulls/{n}/reviews',token)
        payload['review_comments']=paged(f'{base}/pulls/{n}/comments',token)
        path=out/f"{repo.replace('/','__')}__{n}.json"; path.write_text(json.dumps(payload,ensure_ascii=False,indent=2)+'\n',encoding='utf-8')
        print(f'[{i}/{len(rows)}] {repo}#{n}')
        if not token: time.sleep(1)
if __name__=='__main__': main()
