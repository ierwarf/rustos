"""Offline import tests; no browser, download, or host installation is run."""
import importlib.util
import io
from pathlib import Path
import struct
import tarfile
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("import_chrome", Path(__file__).with_name("import-chrome.py"))
chrome = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(chrome)


def archive(extra=(), elf=True):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w") as tar:
        header = bytearray(20)
        header[:6] = b"\x7fELF\x02\x01" if elf else b"script"
        struct.pack_into("<H", header, 18, 62)
        entries = [("./opt/google/chrome/chrome", bytes(header), tarfile.REGTYPE)] + list(extra)
        for name, data, kind in entries:
            member = tarfile.TarInfo(name)
            member.type = kind
            member.size = len(data) if kind == tarfile.REGTYPE else 0
            member.mode = 0o4755
            member.linkname = "/outside"
            tar.addfile(member, io.BytesIO(data) if member.size else None)
    output.seek(0)
    return output


class ChromeImportTests(unittest.TestCase):
    def extract(self, data):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            chrome.extract_bundle(data, target)
            self.assertEqual((target / "chrome").stat().st_mode & 0o7777, 0o755)
            self.assertFalse((target / "etc").exists())

    def test_imports_only_bundle_and_removes_privileged_bits(self):
        self.extract(archive([("./etc/cron.daily/chrome", b"never install", tarfile.REGTYPE)]))

    def test_rejects_traversal_links_duplicates_and_non_elf(self):
        for name, kind in [
            ("./opt/google/chrome/../outside", tarfile.REGTYPE),
            ("/opt/google/chrome/absolute", tarfile.REGTYPE),
            ("./opt/google/chrome/link", tarfile.SYMTYPE),
            ("./opt/google/chrome/hardlink", tarfile.LNKTYPE),
            ("./opt/google/chrome/chrome", tarfile.REGTYPE),
        ]:
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.extract(archive([(name, b"bad", kind)]))
        with self.assertRaises(ValueError):
            self.extract(archive(elf=False))

    def test_digest_must_match(self):
        with tempfile.TemporaryDirectory() as directory:
            package = Path(directory) / "fake.deb"
            package.write_bytes(b"not the package")
            for digest in [chrome.DEFAULT_SHA256, "bad"]:
                with self.assertRaises(ValueError):
                    chrome.check_digest(package, digest)

    def test_manifest_is_visible_but_has_no_autostart_or_sandbox_bypass(self):
        import tomllib
        manifest = tomllib.loads((chrome.ROOT / "apps/chrome/RUSTOS.package.toml.example").read_text())
        self.assertEqual(manifest["startup"], "none")
        self.assertEqual(manifest["build"]["builder"], "prebuilt-app")
        entry = manifest["desktop"]["entries"][0]
        self.assertEqual(entry["display_name"], "Google Chrome")
        self.assertFalse(entry["console_hosted"])
        self.assertFalse(entry.get("no_display", False))
        self.assertNotIn("--no-sandbox", entry["args"])


if __name__ == "__main__":
    unittest.main()
