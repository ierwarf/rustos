#!/usr/bin/env python3
"""Provision the locked Noble runtime from authenticated Ubuntu archives; never run package scripts."""
import hashlib
import io
import json
import lzma
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.request

HERE = Path(__file__).resolve().parent
LOCK = json.loads((HERE / 'noble/lock.json').read_text())
BASE = 'https://snapshot.ubuntu.com/ubuntu/20261110T000000Z/'
FINGERPRINT = 'F6ECB3762474EDA9D21B7022871920D1991BC93C'
RELEASES = {'noble': 'cdb2f31d809f589719a53c6ad15f255b27569c4059542ada282aaa21b8e164b0',
            'noble-updates': '376987eebd4dea43d28af8ac80b79a3503f928aec90cc4129d5263e641bb7b78'}
PREFIX = 'usr/lib/x86_64-linux-gnu/'
PACKAGE_MIRRORS = ('https://archive.ubuntu.com/ubuntu/', 'https://security.ubuntu.com/ubuntu/')
UNAVAILABLE_PACKAGE_MIRRORS = set()


def require(condition, message):
    if not condition:
        raise ValueError(message)


def fetch(cache, path, digest, size=None):
    require(re.fullmatch('[0-9a-f]{64}', digest) is not None, 'invalid sha256')
    require(not path.startswith('/') and '..' not in path.split('/'), 'unsafe archive path')
    dst = cache / digest
    # Hash exactly the bounded bytes returned to the caller, never validate
    # one read and then use a second, potentially replaced, cache read.
    limit = size if size is not None else 16 * 1024 * 1024
    def valid(data):
        return len(data) <= limit and (size is None or len(data) == size) and hashlib.sha256(data).hexdigest() == digest
    if dst.is_file():
        with dst.open('rb') as f:
            data = f.read(limit + 1)
        if valid(data):
            return data
    # Only package bytes may use mirrors. InRelease/index authority remains
    # the pinned snapshot, and every returned body must match its signed hash.
    package = path.startswith('pool/')
    mirrors = (BASE,) + PACKAGE_MIRRORS if package else (BASE,)
    data = None
    failures = []
    for mirror in mirrors:
        if package and mirror in UNAVAILABLE_PACKAGE_MIRRORS:
            continue
        for attempt in range(3):
            try:
                with urllib.request.urlopen(mirror + path, timeout=15) as response:
                    data = response.read(limit + 1)
                break
            except OSError as error:
                permanent = isinstance(error, urllib.error.HTTPError) and error.code != 429 and error.code < 500
                if attempt == 2 or permanent:
                    failures.append(mirror + path + ': ' + str(error))
                    if package and not permanent:
                        UNAVAILABLE_PACKAGE_MIRRORS.add(mirror)
                    if package:
                        print('Noble package transport failed; trying identical-hash mirror: ' + failures[-1], file=sys.stderr)
                    break
                time.sleep(1 << attempt)
        if data is not None:
            break
    require(data is not None, 'archive download failed: ' + '; '.join(failures))
    require(valid(data), 'archive hash mismatch: ' + path)
    with tempfile.NamedTemporaryFile(dir=cache, delete=False) as f:
        f.write(data)
        tmp = f.name
    os.replace(tmp, dst)
    return data


def signed_indexes(cache, metadata=False):
    indexes = {}
    with tempfile.TemporaryDirectory() as temp:
        key = Path(temp) / 'key.gpg'
        with key.open('wb') as f:
            subprocess.run(['gpg', '--dearmor'], input=(HERE / 'noble/ubuntu-archive-2018.asc').read_bytes(), stdout=f, check=True)
        for suite, digest in RELEASES.items():
            data = fetch(cache, 'dists/' + suite + '/InRelease', digest)
            release = Path(temp) / 'InRelease'
            release.write_bytes(data)
            result = subprocess.run(['gpgv', '--status-fd', '1', '--keyring', str(key), str(release)], capture_output=True, check=True)
            require(('[GNUPG:] VALIDSIG ' + FINGERPRINT + ' ').encode() in result.stdout, 'unexpected archive signing key')
            text = data.decode('utf-8')
            require('\nCodename: noble\n' in text and '\nSuite: ' + suite + '\n' in text, 'unexpected archive suite')
            entries = {}
            for line in text.split('\nSHA256:\n', 1)[1].split('\nSHA512:', 1)[0].splitlines():
                m = re.fullmatch(r' ([0-9a-f]{64})\s+(\d+)\s+(\S+)', line)
                if m: entries[m[3]] = (m[1], int(m[2]))
            for component in ('main', 'universe'):
                name = component + '/binary-amd64/Packages.xz'
                digest, size = entries[name]
                content = lzma.decompress(fetch(cache, 'dists/' + suite + '/' + name, digest, size)).decode('utf-8')
                for block in content.split('\n\n'):
                    fields = dict(line.split(': ', 1) for line in block.splitlines() if ': ' in line and not line.startswith(' '))
                    if all(k in fields for k in ('Package', 'Filename', 'SHA256', 'Size')):
                        indexes[(suite, component, fields['Package'])] = fields if metadata else (fields['Filename'], fields['SHA256'], int(fields['Size']))
    return indexes


def libraries(deb):
    require(deb[:8] == b'!<arch>\n', 'invalid deb archive')
    pos = 8
    data = None
    while pos < len(deb):
        header = deb[pos:pos + 60]
        require(len(header) == 60 and header[58:60] == b'`\n', 'invalid ar header')
        size = int(header[48:58]); name = header[:16].decode('ascii').strip().rstrip('/')
        require(pos + 60 + size <= len(deb), 'truncated ar member')
        if name.startswith('data.tar'):
            require(data is None, 'multiple data archives')
            data = deb[pos + 60:pos + 60 + size]
        pos += 60 + size + size % 2
    require(data is not None, 'missing data archive')
    with tarfile.open(fileobj=io.BytesIO(data), mode='r:*') as tar:
        members = {}
        for member in tar.getmembers():
            name = member.name.removeprefix('./')
            require(name not in members, 'duplicate package member')
            members[name] = member
        for name, member in members.items():
            if not name.startswith(PREFIX): continue
            basename = name[len(PREFIX):]
            if not basename or '/' in basename or '.so' not in basename: continue
            seen = set()
            while member.issym() or member.islnk():
                require(member.name not in seen, 'package symlink cycle'); seen.add(member.name)
                target = os.path.normpath(os.path.join(os.path.dirname(member.name), member.linkname) if member.issym() else member.linkname).removeprefix('./')
                require(not target.startswith('/') and not target.startswith('../'), 'package link escapes archive')
                member = members.get(target)
                if member is None: break
            if member is None or not member.isfile(): continue
            require(member.size <= 128 * 1024 * 1024, 'oversized runtime library')
            content = tar.extractfile(member).read()
            if content.startswith(b'\x7fELF'): yield basename, content


def provision(build):
    work = build / 'runtime-libs/noble'
    cache = work / 'cache'; cache.mkdir(parents=True, exist_ok=True)
    indexes = signed_indexes(cache)
    with tempfile.TemporaryDirectory(dir=work) as tmp:
        root = Path(tmp) / 'root'; lib = root / PREFIX; lib.mkdir(parents=True)
        for name, record in sorted(LOCK.items()):
            suite, component, filename, digest, size = record
            require(indexes.get((suite, component, name)) == (filename, digest, size), 'lock differs from signed package index: ' + name)
            for basename, content in libraries(fetch(cache, filename, digest, size)):
                path = lib / basename
                if path.exists(): require(path.read_bytes() == content, 'conflicting package libraries: ' + basename)
                else: path.write_bytes(content)
        dest = work / 'root'
        if dest.exists(): shutil.rmtree(dest)
        os.rename(root, dest)

if __name__ == '__main__':
    try:
        require(len(sys.argv) == 3 and sys.argv[1] == 'provision', 'usage: noble_runtime.py provision BUILD_DIR')
        provision(Path(sys.argv[2]))
    except Exception as e:
        print('Noble runtime provisioning failed:', e, file=sys.stderr)
        sys.exit(1)
