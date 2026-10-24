import importlib.util
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).parents[1] / "set_version.py"
SPEC = importlib.util.spec_from_file_location("set_version", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
set_version = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(set_version)


class SetVersionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        directory = Path(self.temporary_directory.name)
        self.manifest = directory / "Cargo.toml"
        self.lockfile = directory / "Cargo.lock"

    def tearDown(self) -> None:
        self.temporary_directory.cleanup()

    def write_versions(self, manifest_version: str, lockfile_version: str) -> None:
        self.manifest.write_text(
            f'[package]\nname = "astesia"\nversion = "{manifest_version}"\n',
            encoding="utf-8",
        )
        self.lockfile.write_text(
            "[[package]]\nname = \"dependency\"\nversion = \"9.0.0\"\n\n"
            f'[[package]]\nname = "astesia"\nversion = "{lockfile_version}"\n',
            encoding="utf-8",
        )

    def test_sets_matching_package_and_lockfile_versions(self) -> None:
        self.write_versions("1.2.3", "1.2.3")

        version = set_version.set_version(self.manifest, self.lockfile, "1.3.0")

        self.assertEqual(version, "1.3.0")
        self.assertIn('version = "1.3.0"', self.manifest.read_text(encoding="utf-8"))
        lockfile = self.lockfile.read_text(encoding="utf-8")
        self.assertIn('name = "dependency"\nversion = "9.0.0"', lockfile)
        self.assertIn('name = "astesia"\nversion = "1.3.0"', lockfile)

    def test_accepts_prerelease_versions(self) -> None:
        self.write_versions("2.0.0", "2.0.0")

        set_version.set_version(self.manifest, self.lockfile, "2.1.0-beta.1")

        self.assertIn(
            'version = "2.1.0-beta.1"', self.manifest.read_text(encoding="utf-8")
        )

    def test_rejects_invalid_version(self) -> None:
        self.write_versions("1.2.3", "1.2.3")
        before = (self.manifest.read_bytes(), self.lockfile.read_bytes())

        with self.assertRaisesRegex(ValueError, "Unexpected Astesia version format"):
            set_version.set_version(self.manifest, self.lockfile, "v1.3.0")

        self.assertEqual(before, (self.manifest.read_bytes(), self.lockfile.read_bytes()))

    def test_mismatch_leaves_both_files_unchanged(self) -> None:
        self.write_versions("1.2.3", "1.2.2")
        before = (self.manifest.read_bytes(), self.lockfile.read_bytes())

        with self.assertRaisesRegex(ValueError, "version mismatch"):
            set_version.set_version(self.manifest, self.lockfile, "1.3.0")

        self.assertEqual(before, (self.manifest.read_bytes(), self.lockfile.read_bytes()))


if __name__ == "__main__":
    unittest.main()
