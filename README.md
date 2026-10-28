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
- **Proxy Support** - X-Forwarded-For with CIDR whitelisting
- **Cross-Platform** - Windows, macOS, and Linux

## Installation

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
| `--proxy <HEADER:CIDR>` | Trust X-Forwarded-For from CIDR range |
| `--proxy-redir <HEADER:CIDR>` | Trust X-Original-URL from CIDR range |
| `-m, --mime-type <EXT:MIME>` | Override MIME type for extension |
| `-H, --header <NAME:VALUE>` | Add custom response header |

### Compression Cache

| Option | Description |
|--------|-------------|
| `--encoded-filesystem <SIZE>` | Max disk cache for compressed files (e.g. `100M`, `1G`) |
| `--encoded-generated <SIZE>` | Max memory cache for compressed responses |
| `--encoded-prune <TIME>` | Prune cached files older than TIME (e.g. `1h`, `1d`) |

### Logging

| Option | Description |
|--------|-------------|
| `-q, --quiet` | Suppress output (repeat for more quiet) |
| `-Q, --quiet-time` | Don't prefix logs with timestamps |
| `-c, --no-colour` | Disable colored output |

### Bandwidth

| Option | Description |
|--------|-------------|
| `--request-bandwidth <BYTES/S>` | Limit per-request bandwidth (0 = unlimited) |

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
| TRACE | Echo request headers |
| PROPFIND | WebDAV directory listing (requires `-d`) |
| PROPPATCH | WebDAV property modification (requires `-d`) |
| MKCOL | WebDAV create directory (requires `-d` or `-D`) |
| MOVE | WebDAV move/rename (requires `-d` or `-D`) |
| COPY | WebDAV copy (requires `-d`) |

## License

MIT OR Apache-2.0
