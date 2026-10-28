import contextlib
import importlib.util
import io
import json
import pathlib
import subprocess
import unittest
from unittest import mock


spec = importlib.util.spec_from_file_location(
    "audit_dependencies", pathlib.Path(__file__).with_name("audit_dependencies.py")
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

# These previously accepted findings must never regain an exception.
FORMER_ZSTD_ADVISORIES = (
    "RUSTSEC-2026-0391",
    "RUSTSEC-2026-0395",
    "RUSTSEC-2026-0396",
    "RUSTSEC-2026-0397",
    "RUSTSEC-2026-0398",
    "RUSTSEC-2026-0399",
    "RUSTSEC-2026-0400",
    "RUSTSEC-2026-0401",
    "RUSTSEC-2026-0402",
    "RUSTSEC-2026-0403",
    "RUSTSEC-2026-0404",
    "RUSTSEC-2026-0405",
    "RUSTSEC-2026-0406",
)


def finding(advisory="RUSTSEC-2099-0001", package="dependency"):
    return {"advisory": {"id": advisory}, "package": {"name": package, "version": "1.0.0"}}


def report(vulnerabilities=(), **warnings):
    return {"vulnerabilities": {"list": list(vulnerabilities)}, "warnings": warnings}


class AuditPolicyTests(unittest.TestCase):
    def test_empty_report_passes(self):
        self.assertEqual(module.evaluate_report(report()), [])

    def test_all_former_zstd_exceptions_fail_individually_and_together(self):
        vulnerabilities, warnings = [], []
        for advisory in FORMER_ZSTD_ADVISORIES:
            package = "zstd" if advisory.endswith("0391") else "zstd-safe"
            item = finding(advisory, package)
            if advisory in {"RUSTSEC-2026-0391", "RUSTSEC-2026-0398", "RUSTSEC-2026-0403"}:
                vulnerabilities.append(item)
                data = report([item])
            else:
                warnings.append(item)
                data = report(unsound=[item])
            with self.subTest(advisory=advisory):
                rejected = module.evaluate_report(data)
                self.assertEqual(len(rejected), 1)
                self.assertIn(advisory, rejected[0])
                self.assertIn(package, rejected[0])
        self.assertEqual(len(module.evaluate_report(report(vulnerabilities, unsound=warnings))), 13)

    def test_unknown_vulnerability_and_missing_advisory_id_fail(self):
        rejected = module.evaluate_report(report([finding(), finding(None)]))
        self.assertEqual(len(rejected), 2)
        self.assertIn("RUSTSEC-2099-0001", rejected[0])
        self.assertIn("without advisory ID", rejected[1])

    def test_every_warning_category_fails_with_or_without_advisory_ids(self):
        no_advisory = {"package": {"name": "dependency", "version": "1.0.0"}}
        for category in ["unsound", "unmaintained", "notice", "yanked", "future-warning"]:
            for item in [finding(), finding(None), {"advisory": None}, no_advisory]:
                with self.subTest(category=category, finding=item):
                    rejected = module.evaluate_report(report(**{category: [item]}))
                    self.assertEqual(len(rejected), 1)
                    self.assertIn(category, rejected[0])

    def test_all_categories_are_reported_together(self):
        data = report([finding()], unsound=[finding()], yanked=[finding(None)], new=[finding()])
        rejected = module.evaluate_report(data)
        self.assertEqual(len(rejected), 4)
        for category in ["vulnerability", "unsound", "yanked", "new"]:
            self.assertTrue(any(item.startswith(category + " ") for item in rejected))


class AuditCommandTests(unittest.TestCase):
    def run_audit(self, stdout, returncode=0, stderr=""):
        completed = subprocess.CompletedProcess(["cargo", "audit", "--json"], returncode, stdout, stderr)
        output, errors = io.StringIO(), io.StringIO()
        with mock.patch.object(module.subprocess, "run", return_value=completed) as run:
            with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                result = module.main()
        run.assert_called_once_with(["cargo", "audit", "--json"], capture_output=True, text=True)
        return result, output.getvalue(), errors.getvalue()

    def test_clean_audit_passes_without_running_a_dependency_tree(self):
        result, output, errors = self.run_audit(json.dumps(report()))
        self.assertEqual(result, 0)
        self.assertEqual(output, "No dependency audit findings.\n")
        self.assertEqual(errors, "")

    def test_warnings_fail_even_when_cargo_audit_returns_success(self):
        result, _, errors = self.run_audit(json.dumps(report(yanked=[finding(None)])))
        self.assertEqual(result, 1)
        self.assertIn("yanked without advisory ID", errors)

    def test_vulnerability_failure_reports_the_finding(self):
        result, _, errors = self.run_audit(json.dumps(report([finding()])), returncode=1)
        self.assertEqual(result, 1)
        self.assertIn("vulnerability RUSTSEC-2099-0001", errors)

    def test_nonzero_audit_without_findings_still_fails(self):
        for returncode in [1, 2]:
            with self.subTest(returncode=returncode):
                result, _, errors = self.run_audit(json.dumps(report()), returncode, "audit error\n")
                self.assertEqual(result, 1)
                self.assertIn("audit error", errors)

    def test_invalid_json_and_incomplete_reports_fail(self):
        for data in ["not json", "null", "[]", "{}", '{"vulnerabilities": {}}']:
            with self.subTest(report=data):
                result, _, errors = self.run_audit(data)
                self.assertEqual(result, 1)
                self.assertIn("Invalid dependency audit report", errors)


if __name__ == "__main__":
    unittest.main()
