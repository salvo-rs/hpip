# Dependency advisory policy

Run `cargo audit` for the complete lockfile result and
`python .github/scripts/audit_dependencies.py` for the CI policy. The policy rejects
every vulnerability and every warning in the lockfile, including development and
target-specific dependencies. There are no advisory exceptions.

Warnings fail regardless of their category or whether they have an advisory ID.
This includes unsoundness, unmaintained packages, notices, yanked packages, and
categories added by future versions of cargo-audit. Invalid reports and failed
audit commands also fail the gate.

Salvo 1.0.1 and the updated lockfile resolve the former test-only zstd findings.
The thirteen temporary zstd exceptions have been removed; reintroducing any of
those findings fails the same policy as any other dependency finding.

The production server still uses an explicit Salvo feature set, with its test
client and response decoders enabled only as development dependencies. Dependency
scope does not exempt a finding from this policy.

Run `python -m unittest discover -s .github/scripts -p 'test_audit_dependencies.py'`
to check the policy's acceptance and rejection behavior without running Cargo.
