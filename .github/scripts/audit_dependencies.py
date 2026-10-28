"""Reject every dependency audit finding, including development dependencies."""

import json
import subprocess
import sys


def evaluate_report(report):
    """Collect all findings, including unknown warnings and yanks without IDs."""
    findings = [("vulnerability", finding) for finding in report["vulnerabilities"]["list"]]
    for category, warnings in report.get("warnings", {}).items():
        findings.extend((category, finding) for finding in warnings)
    rejected = []
    for category, finding in findings:
        advisory = (finding.get("advisory") or {}).get("id")
        package = finding.get("package", {}).get("name", "unknown package")
        version = finding.get("package", {}).get("version", "unknown version")
        rejected.append(f"{category} {advisory or 'without advisory ID'}: {package} {version}")
    return rejected


def main():
    audit = subprocess.run(["cargo", "audit", "--json"], capture_output=True, text=True)
    if audit.returncode not in (0, 1):
        sys.stderr.write(audit.stderr)
        return 1
    try:
        rejected = evaluate_report(json.loads(audit.stdout))
    except (ValueError, KeyError, TypeError, AttributeError) as error:
        print(f"Invalid dependency audit report: {error}", file=sys.stderr)
        sys.stderr.write(audit.stderr)
        return 1
    if rejected:
        print("Unresolved dependency audit findings:\n" + "\n".join(rejected), file=sys.stderr)
        return 1
    if audit.returncode != 0:
        print("Dependency audit failed without reporting findings.", file=sys.stderr)
        sys.stderr.write(audit.stderr)
        return 1
    print("No dependency audit findings.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
