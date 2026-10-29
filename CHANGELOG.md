# Changelog

## 0.2.1

### Reliability and security

- Revalidate path and collection authorization inside filesystem transactions,
  including derived index files, aliases, and dangling symlink targets.
- Preserve atomic conditional uploads, copies, and moves, with stable file
  identity validators on Windows and cleanup of unpublished staging files.
- Bind file, metadata, and archive reads to authorized source snapshots.
  Compression and response streaming release the namespace lock first.
- Track admitted requests and background workers through shutdown. Cancelled
  queued upload preparation and expired read waits release capacity without starting
  filesystem work; started write transactions retain ownership until complete.
- Recheck archive cancellation and expiry after a namespace lock is granted.
- Correct directory navigation and JSON root detection for repeated slashes and
  nested directory URLs with trailing slashes.

### HTTP and resource handling

- Improve GET/HEAD consistency, byte ranges, conditional requests, repeated
  header handling, authentication parsing, and content-encoding negotiation.
- Stream files and archives with bounded buffers and separate archive capacity.
  Bound metadata output, cache admission, upload bodies, and control-body waits.
- Preserve complete old or new file versions during concurrent publication,
  with representation-specific entity tags for compressed responses.

### Build and distribution

- Require Rust 1.99 or newer and update the server to Salvo 1.
- Gate releases on native Linux x64/ARM64, macOS Intel/ARM64, and Windows x64
  checks, plus a dependency audit and crate packaging verification.
- Provide archives for those five targets and a SHA256SUMS manifest.

### Compatibility notes

- Clients may need to refresh cached entity tags after upgrading.
- Archive members that cannot map to an exact UTF-8 authorization path, including
  ambiguous backslash names, are rejected.
- Cancelling an already started synchronous write transaction does not promise
  rollback. Shutdown waits for owned filesystem work to finish safely; its
  graceful connection interval is not a hard deadline for OS storage calls.

Previous release: [0.2.0](https://github.com/salvo-rs/hpip/releases/tag/v0.2.0).
