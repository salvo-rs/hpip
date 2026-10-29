import contextlib
import importlib.util
import io
import json
import os
import pathlib
import tempfile
import unittest
import urllib.error
from unittest import mock


spec = importlib.util.spec_from_file_location(
    "registry_version", pathlib.Path(__file__).with_name("check-registry-version.py")
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def record(version="0.2.0", **updates):
    entry = {
        "name": "hpip",
        "vers": version,
        "cksum": "a" * 64,
        "deps": [],
        "features": {},
        "yanked": False,
    }
    entry.update(updates)
    return json.dumps(entry).encode()


def response(body, status=200):
    result = mock.MagicMock()
    result.__enter__.return_value.status = status
    result.__enter__.return_value.read.return_value = body
    return result


class RegistryVersionTests(unittest.TestCase):
    def test_exact_existing_version_including_yanked_versions_is_present(self):
        body = record() + b"\n" + record("0.2.1", yanked=True) + b"\n"
        with mock.patch.object(module.urllib.request, "urlopen", return_value=response(body)) as get:
            self.assertTrue(module.version_exists("0.2.1"))
        request = get.call_args.args[0]
        self.assertEqual(request.full_url, "https://index.crates.io/hp/ip/hpip")
        self.assertEqual(request.get_header("User-agent"), module.USER_AGENT)
        self.assertEqual(get.call_args.kwargs["timeout"], 30)

    def test_missing_exact_version_is_false_only_after_valid_index(self):
        body = record("0.2.1-rc.1") + b"\n" + record("0.2.10") + b"\n"
        with mock.patch.object(module.urllib.request, "urlopen", return_value=response(body)):
            self.assertFalse(module.version_exists("0.2.1"))

    def test_empty_malformed_and_non_index_responses_are_rejected(self):
        for body in [b"", b"\n", b"<html>Forbidden</html>", b"[]", b"{}", b"\xff"]:
            with self.subTest(body=body):
                with mock.patch.object(module.urllib.request, "urlopen", return_value=response(body)):
                    with self.assertRaises(ValueError):
                        module.version_exists("0.2.1")

    def test_a_matching_record_does_not_hide_later_parse_errors(self):
        with mock.patch.object(
            module.urllib.request, "urlopen", return_value=response(record("0.2.1") + b"\ninvalid")
        ):
            with self.assertRaises(ValueError):
                module.version_exists("0.2.1")

    def test_invalid_record_fields_are_rejected(self):
        for updates in [
            {"name": "different"},
            {"vers": "garbage"},
            {"vers": "0.2.1-01"},
            {"vers": None},
            {"cksum": "invalid"},
            {"deps": None},
            {"features": []},
            {"yanked": "false"},
        ]:
            with self.subTest(updates=updates):
                with self.assertRaises(ValueError):
                    module.index_versions(record(**updates))

    def test_http_failure_is_never_treated_as_a_missing_version(self):
        for status in [403, 404, 429, 500]:
            with self.subTest(status=status):
                with mock.patch.object(module.urllib.request, "urlopen", return_value=response(record(), status)):
                    with self.assertRaises(ValueError):
                        module.version_exists("0.2.1")

    def test_network_and_http_exceptions_propagate(self):
        for error in [
            urllib.error.URLError("unavailable"),
            urllib.error.HTTPError(module.INDEX_URL, 403, "Forbidden", {}, None),
            TimeoutError("timed out"),
        ]:
            with self.subTest(error=error):
                with mock.patch.object(module.urllib.request, "urlopen", side_effect=error):
                    with self.assertRaises(OSError):
                        module.version_exists("0.2.1")

    def test_invalid_requested_versions_do_not_make_a_request(self):
        with mock.patch.object(module.urllib.request, "urlopen") as get:
            for version in ["v0.2.1", "0.02.1", "0.2.1\n", "0.2.1; command", "0.2.1-rc1"]:
                with self.subTest(version=version), self.assertRaises(ValueError):
                    module.version_exists(version)
        get.assert_not_called()

    def test_cli_writes_an_output_only_after_successful_verification(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "output"
            with mock.patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}):
                with mock.patch("sys.argv", ["check-registry-version.py", "0.2.1"]):
                    with mock.patch.object(module, "version_exists", side_effect=ValueError("bad index")):
                        with contextlib.redirect_stderr(io.StringIO()):
                            self.assertEqual(module.main(), 1)
                    self.assertFalse(output.exists())
                    for exists in [False, True]:
                        with mock.patch.object(module, "version_exists", return_value=exists):
                            with contextlib.redirect_stdout(io.StringIO()):
                                self.assertEqual(module.main(), 0)
            self.assertEqual(output.read_text().splitlines(), ["exists=false", "exists=true"])


if __name__ == "__main__":
    unittest.main()
