import datetime
import importlib.util
import pathlib
import unittest


spec = importlib.util.spec_from_file_location(
    "audit_dependencies", pathlib.Path(__file__).with_name("audit_dependencies.py")
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def finding(advisory="RUSTSEC-2099-0001", package="zstd-safe"):
    return {"advisory": {"id": advisory}, "package": {"name": package, "version": "7.3.0"}}


def report(vulnerabilities=(), **warnings):
    return {"vulnerabilities": {"list": list(vulnerabilities)}, "warnings": warnings}


class AuditPolicyTests(unittest.TestCase):
    def assess(self, data, production=(), today=datetime.date(2026, 10, 28)):
        return module.evaluate_report(data, set(production), today)

    def test_unknown_unsound_findings_fail_for_production_and_tests(self):
        for production in [(), ("zstd-safe",)]:
            with self.subTest(production=production):
                reviewed, rejected = self.assess(report(unsound=[finding()]), production)
                self.assertEqual(reviewed, [])
                self.assertEqual(len(rejected), 1)

    def test_all_reviewed_advisories_pass_only_outside_production_before_expiry(self):
        self.assertEqual(len(module.TEST_ONLY), 13)
        for advisory, (package, _) in module.TEST_ONLY.items():
            for category in ["vulnerability", "unsound"]:
                data = (report([finding(advisory, package)]) if category == "vulnerability"
                        else report(unsound=[finding(advisory, package)]))
                with self.subTest(advisory=advisory, category=category):
                    reviewed, rejected = self.assess(data)
                    self.assertEqual(len(reviewed), 1)
                    self.assertEqual(rejected, [])
                    self.assertEqual(len(self.assess(data, (package,))[1]), 1)
                    self.assertEqual(len(self.assess(data, today=datetime.date(2027, 1, 1))[1]), 1)

    def test_every_warning_category_and_findings_without_advisories_fail(self):
        for category in ["yanked", "unmaintained", "future-warning"]:
            warning = {"package": {"name": "dependency", "version": "1.0.0"}}
            with self.subTest(category=category):
                reviewed, rejected = self.assess(report(**{category: [warning]}))
                self.assertEqual(reviewed, [])
                self.assertEqual(len(rejected), 1)
                self.assertIn(category, rejected[0])

    def test_reviewed_id_cannot_exempt_a_different_package(self):
        _, rejected = self.assess(report(unsound=[finding("RUSTSEC-2026-0395", "other")]))
        self.assertEqual(len(rejected), 1)


if __name__ == "__main__":
    unittest.main()
