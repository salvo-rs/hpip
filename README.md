# hpip

**Host These Things Please** - a modern async HTTP file server written in Rust.

> This project is AI-generated and experimental in nature.

Quickly host files and directories over HTTP with support for WebDAV, compression, authentication, TLS, and more.

## Features

- **Directory Listing** - Browse files with responsive HTML interface (desktop & mobile)
- **File Upload & Deletion** - PUT/DELETE support with `-w` flag
- **WebDAV** - Full or lite WebDAV support for compatible clients
- **Compression** - On-the-fly Gzip, Deflate, and Brotli with filesystem caching
- **Authentication** - Basic HTTP auth, global or per-path, with auto-generated credentials
- **TLS/HTTPS** - Use existing certificates or generate self-signed ones
- **Archive Downloads** - Create TAR/ZIP archives from directories on the fly
- **JSON API** - Raw filesystem metadata via `Accept: application/json`
- **Range Requests** - Partial content serving for resumable downloads
- **Symlink Control** - Follow, block, or sandbox symlinks
- **Custom Headers** - Inject arbitrary HTTP headers
- **Cross-Platform** - Windows, macOS, and Linux

## Installation

Rust 1.99 or newer is required. OpenSSL must be installed to import PKCS12
identities with `--ssl` or generate a certificate with `--gen-ssl`.

```bash
cargo install --path .
```

Or build from source:

```bash
cargo build --release
```

## Usage

```bash
# Serve current directory on auto-selected port
hpip

# Serve a specific directory on port 8080
hpip -p 8080 /path/to/files

# Enable file uploads and deletion
hpip -w /path/to/files

# Enable HTTPS with a self-signed certificate
hpip --gen-ssl /path/to/files

# Enable authentication
hpip --auth admin:password /path/to/files

# Enable WebDAV
hpip -d /path/to/files

# Enable archive downloads
hpip -A /path/to/files
```

## Options

### Core

| Option | Description |
|--------|-------------|
| `[DIR]` | Directory to host (default: current directory) |
| `-p, --port <PORT>` | Port to listen on (default: auto 8000-9999) |
| `-a, --address <ADDRESS>` | Bind address (default: `0.0.0.0`) |
| `-t, --temp-dir <PATH>` | Temporary directory for uploads and cache |
| `--404 <PATH>` | Custom 404 fallback file |

### File Serving

| Option | Description |
|--------|-------------|
| `-l, --no-listings` | Disable directory listings |
| `-i, --no-indices` | Don't auto-serve index files |
| `-x, --strip-extensions` | Strip index extensions from URLs |
| `-e, --no-encode` | Disable on-the-fly compression |

### Write Access

| Option | Description |
|--------|-------------|
| `-w, --allow-write` | Enable PUT/DELETE for file upload and deletion |
| `--max-upload-size <SIZE>` | Maximum upload size (default `1G`, suffixes K/M/G accepted) |

Uploads stream to a private staging file and become visible at their destination only after successful completion. Requests larger than the configured limit return HTTP 413. An upload must finish within 15 minutes and must not stall for more than 30 seconds between body frames; interrupted uploads preserve an existing destination.

### Symlinks

| Option | Description |
|--------|-------------|
| `-s, --no-follow-symlinks` | Don't follow symlinks |
| `-r, --sandbox-symlinks` | Restrict symlinks to hosted directory |

### WebDAV & Archives

| Option | Description |
|--------|-------------|
| `-d, --webdav` | Enable full WebDAV (PROPFIND, PROPPATCH, MKCOL, COPY, MOVE) |
| `-D, --convenient-webdav` | Enable lite WebDAV (MKCOL, MOVE only) |
| `-A, --archives` | Enable TAR/ZIP archive downloads |

Write operations also require `-w`. PROPFIND supports explicit `Depth: 0` and
`Depth: 1`; infinite depth is rejected. Cross-filesystem MOVE fails safely rather
than copying and deleting the source. Uploads use staged atomic replacement.

### Security

| Option | Description |
|--------|-------------|
| `--ssl <PATH>` | TLS identity file (PKCS12) |
| `--gen-ssl` | Generate self-signed TLS certificate |
| `--auth <USER[:PASS]>` | Global basic authentication |
| `--gen-auth` | Generate random credentials |
| `--path-auth <PATH=USER[:PASS]>` | Per-path authentication |
| `--gen-path-auth <PATH>` | Generate random per-path credentials |

### Proxy & Headers

| Option | Description |
|--------|-------------|
| `--proxy <HEADER:CIDR>` | Reserved; currently rejected because forwarding is not implemented |
| `--proxy-redir <HEADER:CIDR>` | Reserved; currently rejected because redirect forwarding is not implemented |
| `-m, --mime-type <EXT:MIME>` | Override MIME type for extension |
| `-H, --header <NAME:VALUE>` | Add custom response header |

### Compression Cache

| Option | Description |
|--------|-------------|
| `--encoded-filesystem <SIZE>` | Max disk cache for compressed files (e.g. `100M`, `1G`) |
| `--encoded-generated <SIZE>` | Max memory cache for compressed responses |
| `--encoded-prune <TIME>` | Prune cached files older than TIME (e.g. `1h`, `1d`) |

Cache admission enforces aggregate limits even when no expiry interval is set.
Defaults are 256 MiB on disk and 16 MiB for generated responses, with a separate
4096-entry metadata cap. Negative compression decisions expire after five minutes.

### Logging

| Option | Description |
|--------|-------------|
| `-q, --quiet` | Suppress output (repeat for more quiet) |
| `-Q, --quiet-time` | Don't prefix logs with timestamps |
| `-c, --no-colour` | Disable colored output |

Request completion records contain a request ID, method, path without its query,
handler duration, explicit response status (or null before finalization), and
expected body length when known. Handler duration excludes subsequent streaming
transfer time; expected body length is not a count of bytes delivered. Configured
passwords are not echoed at startup; generated credentials are displayed unless
authentication output is suppressed.

### Bandwidth

| Option | Description |
|--------|-------------|
| `--request-bandwidth <BYTES/S>` | Only 0 (unlimited) is supported; nonzero limits are rejected |

## Collection Authorization

Directory archives, recursive WebDAV operations, and directory deletion require
the request's credentials to satisfy every protected descendant, including
protected targets reached through directory aliases. If descendants use
different credentials, operate on those resources individually or configure a
consistent collection policy. The server rejects an operation before producing
archive bytes or changing collection members when authorization fails. When
per-path rules are configured, authorization traversal rejects unreadable trees,
link cycles, more than 100,000 members, or nesting beyond 256 levels.

## Environment Variables

| Variable | Description |
|----------|-------------|
| `HTTP_SSL_PASS` | Password for TLS identity file |

## Supported HTTP Methods

| Method | Description |
|--------|-------------|
| GET | Serve files and directory listings |
| HEAD | File metadata without body |
| PUT | Upload files (requires `-w`) |
| DELETE | Delete files (requires `-w`) |
| POST | Create archives from directories |
| OPTIONS | List allowed methods |
| PROPFIND | WebDAV directory listing (requires `-d`) |
| PROPPATCH | WebDAV property modification (requires `-d`) |
| MKCOL | WebDAV create directory (requires `-d` or `-D`) |
| MOVE | WebDAV move/rename (requires `-d` or `-D`) |
| COPY | WebDAV copy (requires `-d`) |

## License

Apache-2.0

## Behavior and development checks

File and archive responses use bounded streaming. Compression caches have total
byte budgets and entry limits; `--encoded-prune` controls expiration independently
of those capacity limits. Temporary state belongs to one server instance and is
cleaned after graceful request and worker shutdown.

Symlinks are followed by default. Use `-s` to block them or `-r` to restrict their
targets to the hosted directory. These options do not establish an operating
system sandbox against another process changing the filesystem concurrently.

Run `cargo test --locked --all-targets`, `cargo clippy --locked --all-targets -- -D warnings`,
and `cargo +nightly fmt --all --check`. The dependency audit rejects all findings,
including development dependency warnings, as documented in
[docs/dependency-audit.md](docs/dependency-audit.md).
