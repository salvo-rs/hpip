"""Fail every audit finding except reviewed, temporary test-only zstd advisories."""

import datetime
import json
import subprocess
import sys


TEST_ONLY = {
    "RUSTSEC-2026-0391": ("zstd", "Salvo ResponseExt test decompression; no runtime zstd feature"),
    "RUSTSEC-2026-0395": ("zstd-safe", "Tests do not enable experimental StableInBuffer"),
    "RUSTSEC-2026-0396": ("zstd-safe", "Tests do not use seekable decoders or seek tables"),
    "RUSTSEC-2026-0397": ("zstd-safe", "Tests do not construct OutBuffer with around_pos"),
    "RUSTSEC-2026-0398": ("zstd-safe", "Tests do not use seekable zstd decompression"),
    "RUSTSEC-2026-0399": ("zstd-safe", "Tests do not initialize contexts using borrowed dictionaries"),
    "RUSTSEC-2026-0400": ("zstd-safe", "Tests do not reuse dictionaries across one-shot and streaming calls"),
    "RUSTSEC-2026-0401": ("zstd-safe", "Tests do not train zstd dictionaries"),
    "RUSTSEC-2026-0402": ("zstd-safe", "Tests do not clone multithreaded compression contexts"),
    "RUSTSEC-2026-0403": ("zstd-safe", "Tests do not consume untrusted external zstd fixtures"),
    "RUSTSEC-2026-0404": ("zstd-safe", "Tests do not clone decompression contexts"),
    "RUSTSEC-2026-0405": ("zstd-safe", "Tests do not use seekable compression streams"),
    "RUSTSEC-2026-0406": ("zstd-safe", "Tests do not use multithreaded compression contexts"),
}
REVIEW_BY = datetime.date(2026, 12, 31)


def evaluate_report(report, production, today=None):
    """Assess every category, including warnings without an advisory such as yanks."""
    today = today or datetime.date.today()
    findings = [("vulnerability", finding) for finding in report["vulnerabilities"]["list"]]
    for category, warnings in report.get("warnings", {}).items():
        findings.extend((category, finding) for finding in warnings)
    reviewed, rejected = [], []
    for category, finding in findings:
        advisory = (finding.get("advisory") or {}).get("id")
        package = finding.get("package", {}).get("name", "unknown package")
        version = finding.get("package", {}).get("version", "unknown version")
        exception = TEST_ONLY.get(advisory)
        if (exception is not None and package == exception[0]
                and package not in production and today <= REVIEW_BY):
            reviewed.append(f"Reviewed test-only {category} {advisory}: {exception[1]}; expires {REVIEW_BY}")
        else:
            rejected.append(f"{category} {advisory or 'without advisory ID'}: {package} {version}")
    return reviewed, rejected


def main():
    audit = subprocess.run(["cargo", "audit", "--json"], capture_output=True, text=True)
    if audit.returncode not in (0, 1):
        sys.stderr.write(audit.stderr)
        return 1
    try:
        report = json.loads(audit.stdout)
    except json.JSONDecodeError:
        sys.stderr.write(audit.stderr or audit.stdout)
        return 1
    tree = subprocess.run(
        ["cargo", "tree", "--locked", "--target", "all", "--edges", "normal", "--prefix", "none", "--format", "{p}"],
        capture_output=True, text=True, check=True,
    )
    production = {line.split(" ", 1)[0] for line in tree.stdout.splitlines()}
    reviewed, rejected = evaluate_report(report, production)
    for finding in reviewed:
        print(finding)
    if rejected:
        print("Unresolved dependency audit findings:\n" + "\n".join(rejected), file=sys.stderr)
        return 1
    print("No unreviewed dependency audit findings; reviewed exceptions are absent from the production dependency tree.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
