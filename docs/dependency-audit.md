# Dependency advisory policy

The production server uses an explicit Salvo feature set. Salvo's test client and
response decoders are enabled only as development dependencies. ZIP uses the
patched 9.x series; the lockfile also updates the HTTP and TLS dependencies.

Run `cargo audit` for the complete lockfile result and
`python .github/scripts/audit_dependencies.py` for the CI policy. The policy rejects
every new finding, including all warning categories and yanks without advisory
IDs, and every finding present in the production dependency tree for any target.

Thirteen advisories remain in Salvo's test-only zstd dependencies: three
vulnerabilities and ten unsoundness warnings. hpip does not enable zstd in its
runtime feature set. Each exception is limited to its named package and the test
usage below; every exception expires on 2026-12-31.

| Advisory | Package | Finding | Reviewed test-only scope |
|---|---|---|---|
| RUSTSEC-2026-0391 | zstd | Vulnerability | Salvo response test decoder only; no runtime zstd feature |
| RUSTSEC-2026-0395 | zstd-safe | Unsound | No experimental StableInBuffer in tests |
| RUSTSEC-2026-0396 | zstd-safe | Unsound | No seekable decoders or seek tables in tests |
| RUSTSEC-2026-0397 | zstd-safe | Unsound | Tests do not construct OutBuffer with around_pos |
| RUSTSEC-2026-0398 | zstd-safe | Vulnerability | No seekable zstd decompression in tests |
| RUSTSEC-2026-0399 | zstd-safe | Unsound | No context initialization using borrowed dictionaries in tests |
| RUSTSEC-2026-0400 | zstd-safe | Unsound | No dictionaries reused across one-shot and streaming calls in tests |
| RUSTSEC-2026-0401 | zstd-safe | Unsound | No dictionary training in tests |
| RUSTSEC-2026-0402 | zstd-safe | Unsound | No multithreaded compression context cloning in tests |
| RUSTSEC-2026-0403 | zstd-safe | Vulnerability | Tests do not consume untrusted external zstd fixtures |
| RUSTSEC-2026-0404 | zstd-safe | Unsound | No decompression context cloning in tests |
| RUSTSEC-2026-0405 | zstd-safe | Unsound | No seekable compression streams in tests |
| RUSTSEC-2026-0406 | zstd-safe | Unsound | No multithreaded compression contexts in tests |

Remove these exceptions once Salvo's test feature uses patched zstd releases.
Moving an affected package into runtime dependencies immediately fails the policy,
even before the expiration date. New test-only advisories also fail until reviewed.

The policy is a reachability decision, not a claim that the lockfile has zero
advisories. Reassess it whenever tests begin consuming external compressed data
or the production feature set changes.
