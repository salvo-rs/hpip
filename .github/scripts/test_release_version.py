import importlib.util
import pathlib
import unittest


spec = importlib.util.spec_from_file_location(
    "release_version", pathlib.Path(__file__).with_name("check-release-version.py")
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class ReleaseVersionTests(unittest.TestCase):
    def test_matching_version_is_accepted(self):
        self.assertEqual(module.release_version("v0.2.0", {"package": {"version": "0.2.0"}}), "0.2.0")

    def test_mismatched_version_is_rejected(self):
        with self.assertRaises(ValueError):
            module.release_version("v0.3.0", {"package": {"version": "0.2.0"}})

    def test_invalid_tags_cannot_enter_release_commands(self):
        for tag in ["0.2.0", "v0.2.0-rc1", "v00.2.0", "v0.2.0\nextra", "v0.2.0; command"]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                module.release_version(tag, {"package": {"version": "0.2.0"}})


if __name__ == "__main__":
    unittest.main()
