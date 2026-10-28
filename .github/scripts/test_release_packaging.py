import hashlib
import importlib.util
import pathlib
import tarfile
import tempfile
import unittest
import zipfile


spec = importlib.util.spec_from_file_location(
    "package_release", pathlib.Path(__file__).with_name("package-release.py")
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class ReleasePackagingTests(unittest.TestCase):
    def test_all_published_targets_have_checksummed_archives_with_one_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for target in module.TARGETS:
                path = root / "artifacts" / f"hpip-{target}"
                path.mkdir(parents=True)
                (path / ("hpip.exe" if "windows" in target else "hpip")).write_bytes(target.encode())
            module.package_release(root / "artifacts", root / "release")
            checksums = (root / "release/SHA256SUMS").read_text().splitlines()
            self.assertEqual(len(checksums), 5)
            self.assertTrue(any("aarch64-unknown-linux-gnu" in line for line in checksums))
            for line in checksums:
                digest, name = line.split()
                archive = root / "release" / name
                self.assertEqual(hashlib.sha256(archive.read_bytes()).hexdigest(), digest)
                if name.endswith(".zip"):
                    with zipfile.ZipFile(archive) as reader:
                        self.assertEqual(reader.namelist(), ["hpip.exe"])
                else:
                    with tarfile.open(archive) as reader:
                        self.assertEqual(reader.getnames(), ["hpip"])
                        self.assertEqual(reader.getmember("hpip").mode, 0o755)

    def test_missing_target_prevents_packaging(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            with self.assertRaises(ValueError):
                module.package_release(root / "artifacts", root / "release")
            self.assertFalse((root / "release").exists())


if __name__ == "__main__":
    unittest.main()
