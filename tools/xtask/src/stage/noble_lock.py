#!/usr/bin/env python3
"""Add reviewed library packages to the immutable, authenticated Noble lock.

This is not an apt installer: non-library package assets and maintainer scripts
are not installed. ELF closure and final target-loader checks remain mandatory.
Existing pinned records are preserved unless a new dependency constraint cannot
be met, in which case this tool refuses rather than silently updating them.
"""
import json
from pathlib import Path
import re
import sys
import noble_runtime as runtime


def version_compare(left, right):
    """Debian Policy 5.6.12: epoch, upstream, then Debian revision ordering.

    https://www.debian.org/doc/debian-policy/ch-controlfields.html#version
    """
    def split(value):
        runtime.require(re.fullmatch(r'(?:[0-9]+:)?[0-9][A-Za-z0-9.+:~\-]*', value) is not None,
                        'invalid Debian version: ' + value)
        epoch, body = value.split(':', 1) if ':' in value else ('0', value)
        upstream, revision = body.rsplit('-', 1) if '-' in body else (body, '0')
        return int(epoch), upstream, revision
    def sign(a, b):
        return (a > b) - (a < b)
    def order(char):
        if char == '~': return -1
        if not char or char.isdigit(): return 0
        return ord(char) if char.isalpha() else ord(char) + 256
    def part(a, b):
        i = j = 0
        while i < len(a) or j < len(b):
            while (i < len(a) and not a[i].isdigit()) or (j < len(b) and not b[j].isdigit()):
                x = a[i] if i < len(a) else ''
                y = b[j] if j < len(b) else ''
                result = sign(order(x), order(y))
                if result: return result
                if i < len(a): i += 1
                if j < len(b): j += 1
            end_i, end_j = i, j
            while end_i < len(a) and a[end_i].isdigit(): end_i += 1
            while end_j < len(b) and b[end_j].isdigit(): end_j += 1
            x, y = a[i:end_i].lstrip('0'), b[j:end_j].lstrip('0')
            result = sign(len(x), len(y)) or sign(x, y)
            if result: return result
            i, j = end_i, end_j
        return 0
    a, b = split(left), split(right)
    return sign(a[0], b[0]) or part(a[1], b[1]) or part(a[2], b[2])


def extend(build, roots):
    cache = build / 'runtime-libs/noble/cache'
    cache.mkdir(parents=True, exist_ok=True)
    indexes = runtime.signed_indexes(cache, metadata=True)
    selected = dict(runtime.LOCK)
    visited = set()
    providers = {}
    for (suite, component, _), record in indexes.items():
        for value in record.get('Provides', '').split(','):
            match = re.fullmatch(r'\s*([a-z0-9][a-z0-9+.-]*)(?:\s*\(=\s*([^ )]+)\))?\s*', value)
            if match:
                providers.setdefault(match[1], []).append((suite, component, record, match[2]))

    # Authenticate every retained record, not only those reached by new roots.
    for name, (suite, component, filename, digest, size) in selected.items():
        record = indexes.get((suite, component, name))
        runtime.require(record is not None and
                        (record['Filename'], record['SHA256'], int(record['Size'])) == (filename, digest, size),
                        'existing lock is not authenticated: ' + name)

    class NoCandidate(ValueError):
        pass

    def choose(name, operation=None, required_version=None):
        candidates = []
        if name in selected:
            suite, component = selected[name][:2]
            candidates.append((suite, component, indexes[(suite, component, name)]))
        else:
            for suite in ('noble-updates', 'noble'):
                for component in ('main', 'universe'):
                    record = indexes.get((suite, component, name))
                    if record is not None:
                        candidates.append((suite, component, record))
        provided = [item for item in providers.get(name, [])
                    if item[2]['Package'] not in selected
                    or (item[0], item[1]) == tuple(selected[item[2]['Package']][:2])]
        provided.sort(key=lambda item: (item[2]['Package'] not in selected,
                                       item[0] != 'noble-updates', item[1] != 'main', item[2]['Package']))
        candidates.extend((suite, component, dict(record, _provided_version=version))
                          for suite, component, record, version in provided)
        for suite, component, record in candidates:
            if operation:
                version = record.get('_provided_version', record['Version'])
                if version is None:
                    continue
                comparison = version_compare(version, required_version)
                if not {'<<': comparison < 0, '<=': comparison <= 0, '=': comparison == 0,
                        '>=': comparison >= 0, '>>': comparison > 0}[operation]:
                    continue
            return suite, component, record
        raise NoCandidate('no authenticated compatible package: ' + name)

    queue = list(roots)
    while queue:
        name = queue.pop(0)
        suite, component, record = choose(name)
        name = record['Package']
        if name in visited:
            continue
        visited.add(name)
        runtime.require(record.get('Architecture') in ('amd64', 'all'), 'wrong package architecture')
        selected[name] = [suite, component, record['Filename'], record['SHA256'], int(record['Size'])]
        for group in (record.get('Pre-Depends', '') + ',' + record.get('Depends', '')).split(','):
            alternatives = []
            for term in group.split('|'):
                match = re.fullmatch(r'\s*([a-z0-9][a-z0-9+.-]*)(?::(?:any|amd64))?(?:\s*\((<<|<=|=|>=|>>)\s*([^ )]+)\))?\s*', term)
                if match:
                    alternatives.append(match.groups())
                elif term.strip():
                    raise ValueError('unsupported dependency syntax: ' + term)
            # This lock supplies libraries, not package-manager assets/services.
            # Non-library dependencies must be provisioned separately if needed.
            library_alternatives = [a for a in alternatives if a[0].startswith('lib') or a[0] == 'zlib1g']
            if not library_alternatives:
                continue
            found = False
            for dep, operation, version in library_alternatives:
                try:
                    dep_suite, dep_component, dependency = choose(dep, operation, version)
                except NoCandidate:
                    continue
                actual = dependency['Package']
                # Bind the exact version/provider checked above; later queue
                # expansion must not re-resolve a virtual name differently.
                if actual not in selected:
                    selected[actual] = [dep_suite, dep_component, dependency['Filename'],
                                        dependency['SHA256'], int(dependency['Size'])]
                queue.append(actual)
                found = True
                break
            runtime.require(found, 'no authenticated compatible library dependency: ' + group)
    path = runtime.HERE / 'noble/lock.json'
    path.write_text(json.dumps(dict(sorted(selected.items())), indent=2) + '\n')
    print(f'Authenticated lock: {len(selected)} packages; added {len(set(selected) - set(runtime.LOCK))}')


if __name__ == '__main__':
    try:
        runtime.require(len(sys.argv) >= 3, 'usage: noble_lock.py BUILD_DIR LIBRARY_PACKAGE...')
        extend(Path(sys.argv[1]), sys.argv[2:])
    except Exception as error:
        print('Noble lock update failed:', error, file=sys.stderr)
        sys.exit(1)
