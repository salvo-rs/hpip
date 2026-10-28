# Dependency advisory policy

The production server uses an explicit Salvo feature set. Salvo's test client and
response decoders are enabled only as development dependencies. ZIP uses the
patched 9.x series; the lockfile also updates the HTTP and TLS dependencies.

Run `cargo audit` for the complete lockfile result and
`python .github/scripts/audit_dependencies.py` for the CI policy. The policy rejects
every new advisory and every advisory present in the production dependency tree.

Three findings remain in Salvo's test-only zstd dependency: RUSTSEC-2026-0391,
RUSTSEC-2026-0398 and RUSTSEC-2026-0403. hpip does not enable zstd in its runtime
feature set, use seekable zstd decompression, reuse zstd encoding contexts, or
decode untrusted external zstd fixtures in its tests. These narrowly scoped
exceptions expire on 2026-12-31. They must be removed once Salvo's test feature
uses patched zstd releases; moving the affected package into runtime dependencies
immediately fails the policy, even before the expiration date.

The policy is a reachability decision, not a claim that the lockfile has zero
advisories. Reassess it whenever tests begin consuming external compressed data
or the production feature set changes.
