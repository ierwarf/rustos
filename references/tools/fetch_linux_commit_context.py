#!/usr/bin/env python3
"""Fetch Linux commit messages and extract review/backport provenance tags.
SPDX-License-Identifier: CC0-1.0
"""
from __future__ import annotations
import argparse,csv,json,os,re,urllib.request
from pathlib import Path
TAG=re.compile(r'^(Fixes|Link|Closes|Reported-by|Tested-by|Reviewed-by|Acked-by|Signed-off-by|Cc):\s*(.*)$',re.M)
def get(url,token):
 h={'Accept':'application/vnd.github+json','User-Agent':'os-ai-reference-pack/1.0'}
 if token:h['Authorization']=f'Bearer {token}'
 with urllib.request.urlopen(urllib.request.Request(url,headers=h),timeout=30) as r:return json.load(r)
def main():
 ap=argparse.ArgumentParser();ap.add_argument('csv');ap.add_argument('--out',default='linux_commit_context.jsonl');a=ap.parse_args();token=os.getenv('GITHUB_TOKEN','')
 rows=[]
 for r in csv.DictReader(open(a.csv,encoding='utf-8')):
  obj=get(f"https://api.github.com/repos/torvalds/linux/commits/{r['sha']}",token);msg=obj['commit']['message']
  rows.append({'sha':r['sha'],'url':r['url'],'message':msg,'tags':[{'name':k,'value':v} for k,v in TAG.findall(msg)],'parents':[p['sha'] for p in obj.get('parents',[])]})
 with Path(a.out).open('w',encoding='utf-8') as f:
  for x in rows:f.write(json.dumps(x,ensure_ascii=False)+'\n')
 print(f'wrote {len(rows)} commits')
if __name__=='__main__':main()
