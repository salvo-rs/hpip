"""Fail every advisory except reviewed, temporary test-only zstd findings."""

import datetime
import json
import subprocess
import sys


TEST_ONLY = {
    "RUSTSEC-2026-0391": "Salvo ResponseExt test decompression; no runtime zstd feature",
    "RUSTSEC-2026-0398": "Salvo test helper; hpip does not use seekable zstd decompression",
    "RUSTSEC-2026-0403": "Salvo test helper; hpip does not consume untrusted zstd fixtures",
}
REVIEW_BY = datetime.date(2026, 12, 31)


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
        ["cargo", "tree", "--locked", "--edges", "normal", "--prefix", "none", "--format", "{p}"],
        capture_output=True, text=True, check=True,
    )
    production = {line.split(" ", 1)[0] for line in tree.stdout.splitlines()}
    rejected = []
    for finding in report["vulnerabilities"]["list"]:
        advisory = finding["advisory"]["id"]
        package = finding["package"]["name"]
        if advisory in TEST_ONLY and package not in production and datetime.date.today() <= REVIEW_BY:
            print(f"Reviewed test-only finding {advisory}: {TEST_ONLY[advisory]}; expires {REVIEW_BY}")
        else:
            rejected.append(f"{advisory}: {package} {finding['package']['version']}")
    if rejected:
        print("Unresolved security advisories:\n" + "\n".join(rejected), file=sys.stderr)
        return 1
    print("No unreviewed advisories; reviewed exceptions are absent from the production dependency tree.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
