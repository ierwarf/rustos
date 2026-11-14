import hashlib
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('noble_runtime', Path(__file__).with_name('noble_runtime.py'))
runtime = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runtime)


def deb_with(entries):
    data = io.BytesIO()
    with tarfile.open(fileobj=data, mode='w:xz') as tar:
        for name, payload, link in entries:
            info = tarfile.TarInfo(name)
            if link:
                info.type = tarfile.SYMTYPE
                info.linkname = link
                tar.addfile(info)
            else:
                info.size = len(payload)
                tar.addfile(info, io.BytesIO(payload))
    payload = data.getvalue()
    header = f'{"data.tar.xz/":16}{0:<12}{0:<6}{0:<6}{0:<8}{len(payload):<10}`\n'.encode()
    return b'!<arch>\n' + header + payload + (b'\n' if len(payload) % 2 else b'')


class NobleRuntimeTests(unittest.TestCase):
    def test_debian_version_constraints_follow_epoch_tilde_and_numeric_policy(self):
        import noble_lock
        for left, right in [('1.0~rc1', '1.0'), ('1.9', '1.10'), ('1.0-1', '1.0-1ubuntu1'),
                            ('9.0', '1:1.0'), ('1.0a', '1.0+'), ('1.0~~', '1.0~')]:
            self.assertLess(noble_lock.version_compare(left, right), 0)
            self.assertGreater(noble_lock.version_compare(right, left), 0)
        for left, right in [('1.0', '0:1.0-0'), ('1.0001-01', '1.1-1')]:
            self.assertEqual(noble_lock.version_compare(left, right), 0)

    def test_lock_uses_versioned_provider_and_preserves_existing_pin(self):
        import json
        import noble_lock
        def record(name, version, **fields):
            return dict(Package=name, Version=version, Architecture='amd64',
                        Filename='pool/' + name + '.deb', SHA256='1' * 64, Size='2', **fields)
        provider = record('libtime64', '8.0', Provides='libold (= 2.2)')
        newer = record('libtime64', '9.0', Provides='libold (= 3.0)')
        root = record('libroot', '1.0', Depends='libold (>= 2.0)')
        indexes = {('noble', 'main', 'libroot'): root,
                   ('noble', 'main', 'libtime64'): provider,
                   ('noble-updates', 'main', 'libtime64'): newer}
        pinned = ['noble', 'main', provider['Filename'], provider['SHA256'], 2]
        with tempfile.TemporaryDirectory() as d:
            here = Path(d); (here / 'noble').mkdir()
            with patch.object(noble_lock.runtime, 'HERE', here), \
                 patch.object(noble_lock.runtime, 'LOCK', {'libtime64': pinned}), \
                 patch.object(noble_lock.runtime, 'signed_indexes', return_value=indexes):
                noble_lock.extend(here, ['libroot'])
                result = json.loads((here / 'noble/lock.json').read_text())
                self.assertEqual(set(result), {'libroot', 'libtime64'})
                self.assertEqual(result['libtime64'], pinned)
                root['Depends'] = 'libold (>= 2.9)'
                with self.assertRaises(ValueError):
                    noble_lock.extend(here, ['libroot'])
            # An incompatible first provider/suite must not hide another
            # authenticated candidate that satisfies the same dependency.
            root['Depends'] = 'libvirtual (>= 2.0), libversion (<= 1.0)'
            indexes[('noble', 'main', 'libalpha')] = record('libalpha', '9.0', Provides='libvirtual (= 1.0)')
            indexes[('noble', 'main', 'libbeta')] = record('libbeta', '1.0', Provides='libvirtual (= 2.0)')
            indexes[('noble', 'main', 'libversion')] = record('libversion', '1.0')
            indexes[('noble-updates', 'main', 'libversion')] = record('libversion', '2.0')
            with patch.object(noble_lock.runtime, 'HERE', here), \
                 patch.object(noble_lock.runtime, 'LOCK', {}), \
                 patch.object(noble_lock.runtime, 'signed_indexes', return_value=indexes):
                noble_lock.extend(here, ['libroot'])
                result = json.loads((here / 'noble/lock.json').read_text())
                self.assertIn('libbeta', result)
                self.assertNotIn('libalpha', result)
                self.assertEqual(result['libversion'][0], 'noble')

    def test_extracts_only_library_files_without_running_scripts(self):
        prefix = runtime.PREFIX
        deb = deb_with([(prefix + 'libfoo.so.1.0', b'\x7fELFfixture', None),
                        (prefix + 'libfoo.so.1', b'', 'libfoo.so.1.0'),
                        ('postinst', b'exit 99', None)])
        libs = dict(runtime.libraries(deb))
        self.assertEqual(libs['libfoo.so.1'], b'\x7fELFfixture')
        self.assertNotIn('postinst', libs)

    def test_rejects_escaping_link_and_duplicate_members(self):
        prefix = runtime.PREFIX
        with self.assertRaises(ValueError):
            list(runtime.libraries(deb_with([(prefix + 'libbad.so', b'', '../../../../outside')])))
        with self.assertRaises(ValueError):
            list(runtime.libraries(deb_with([(prefix + 'libbad.so', b'a', None), (prefix + 'libbad.so', b'b', None)])))

    def test_cache_replacement_after_hash_cannot_change_returned_bytes(self):
        good = b'expected'; digest_fn = hashlib.sha256
        digest = digest_fn(good).hexdigest()
        with tempfile.TemporaryDirectory() as d:
            cache = Path(d); entry = cache / digest; entry.write_bytes(good)
            def hash_then_replace(data):
                result = digest_fn(data)
                entry.write_bytes(b'tampered')
                return result
            with patch.object(runtime.hashlib, 'sha256', side_effect=hash_then_replace):
                self.assertEqual(runtime.fetch(cache, 'pool/example.deb', digest, len(good)), good)
            self.assertEqual(entry.read_bytes(), b'tampered')

    def test_network_reset_retries_but_uses_verified_bytes(self):
        good = b'expected'; digest = hashlib.sha256(good).hexdigest()
        with tempfile.TemporaryDirectory() as d:
            with patch.object(runtime.urllib.request, 'urlopen', side_effect=[ConnectionResetError(), io.BytesIO(good)]) as fetch:
                with patch.object(runtime.time, 'sleep'):
                    self.assertEqual(runtime.fetch(Path(d), 'pool/example.deb', digest, len(good)), good)
                self.assertEqual(fetch.call_count, 2)

    def test_package_transport_mirror_keeps_the_exact_authenticated_hash(self):
        good = b'expected'; digest = hashlib.sha256(good).hexdigest()
        unavailable = runtime.urllib.error.HTTPError('snapshot', 404, 'missing', {}, None)
        with tempfile.TemporaryDirectory() as d:
            with patch.object(runtime.urllib.request, 'urlopen', side_effect=[unavailable, io.BytesIO(good)]) as fetch:
                self.assertEqual(runtime.fetch(Path(d), 'pool/example.deb', digest, len(good)), good)
                self.assertEqual(fetch.call_count, 2)
            with patch.object(runtime.urllib.request, 'urlopen', side_effect=unavailable) as fetch:
                with self.assertRaises(ValueError):
                    runtime.fetch(Path(d), 'dists/noble/InRelease', '0' * 64)
                self.assertEqual(fetch.call_count, 1, 'metadata must not use mutable mirror authority')

    def test_corrupt_cache_is_not_accepted(self):
        good = b'expected'; digest = hashlib.sha256(good).hexdigest()
        with tempfile.TemporaryDirectory() as d:
            cache = Path(d); (cache / digest).write_bytes(b'tampered')
            with patch.object(runtime.urllib.request, 'urlopen', return_value=io.BytesIO(b'tampered')):
                with self.assertRaises(ValueError):
                    runtime.fetch(cache, 'pool/example.deb', digest, len(good))


if __name__ == '__main__':
    unittest.main()
