#!/usr/bin/env bash
set -euo pipefail

REPO="salvo-rs/hpip"
BINARY="hpip"

# --- Helpers ---

info() { printf '%s\n' "$@"; }
error() { printf 'Error: %s\n' "$@" >&2; exit 1; }

need_cmd() {
    if ! command -v "$1" > /dev/null 2>&1; then
        error "need '$1' (command not found)"
    fi
}

# --- Detect platform ---

detect_target() {
    local os arch target

    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)  os="unknown-linux-gnu" ;;
        Darwin) os="apple-darwin" ;;
        *)      error "unsupported OS: $os" ;;
    esac

    case "$arch" in
        x86_64|amd64)  arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *)             error "unsupported architecture: $arch" ;;
    esac

    target="${arch}-${os}"
    printf '%s' "$target"
}

# --- Resolve install directory ---

resolve_install_dir() {
    if [ -n "${INSTALL_DIR:-}" ]; then
        printf '%s' "$INSTALL_DIR"
        return
    fi

    if [ -w /usr/local/bin ]; then
        printf '%s' "/usr/local/bin"
    else
        local dir="${HOME}/.local/bin"
        mkdir -p "$dir"
        printf '%s' "$dir"
    fi
}

# --- Fetch latest version tag ---

get_latest_version() {
    local url="https://api.github.com/repos/${REPO}/releases/latest"
    local version

    if command -v curl > /dev/null 2>&1; then
        version="$(curl -fsSL "$url" | grep '"tag_name"' | sed -E 's/.*"tag_name":\s*"([^"]+)".*/\1/')"
    elif command -v wget > /dev/null 2>&1; then
        version="$(wget -qO- "$url" | grep '"tag_name"' | sed -E 's/.*"tag_name":\s*"([^"]+)".*/\1/')"
    else
        error "need 'curl' or 'wget' to download"
    fi

    if [ -z "$version" ]; then
        error "could not determine latest version"
    fi

    printf '%s' "$version"
}

# --- Download and install ---

verify_checksum() {
    local archive_path="$1" checksums_path="$2" archive_name="$3"
    local expected actual
    expected=$(awk -v archive="$archive_name" '$2 == archive || $2 == "*" archive { print $1 }' "$checksums_path")
    if [[ ! "$expected" =~ ^[[:xdigit:]]{64}$ ]]; then
        error "release checksum is missing or ambiguous for $archive_name"
    fi
    if command -v sha256sum > /dev/null 2>&1; then
        actual=$(sha256sum "$archive_path" | awk '{print $1}')
    elif command -v shasum > /dev/null 2>&1; then
        actual=$(shasum -a 256 "$archive_path" | awk '{print $1}')
    else
        error "need sha256sum or shasum to verify the release"
    fi
    if [ "$(printf '%s' "$actual" | tr '[:upper:]' '[:lower:]')" != "$(printf '%s' "$expected" | tr '[:upper:]' '[:lower:]')" ]; then
        error "SHA256 verification failed for $archive_name"
    fi
}

download_and_install() {
    local version="$1"
    local target="$2"
    local install_dir="$3"

    local archive_name="${BINARY}-${target}.tar.gz"
    local url="https://github.com/${REPO}/releases/download/${version}/${archive_name}"

    local tmpdir
    tmpdir="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf '$tmpdir'" EXIT

    info "Downloading ${BINARY} ${version} for ${target}..."

    if command -v curl > /dev/null 2>&1; then
        curl -fsSL "$url" -o "${tmpdir}/${archive_name}"
        curl -fsSL "https://github.com/${REPO}/releases/download/${version}/SHA256SUMS" -o "${tmpdir}/SHA256SUMS"
    elif command -v wget > /dev/null 2>&1; then
        wget -q "$url" -O "${tmpdir}/${archive_name}"
        wget -q "https://github.com/${REPO}/releases/download/${version}/SHA256SUMS" -O "${tmpdir}/SHA256SUMS"
    fi

    verify_checksum "${tmpdir}/${archive_name}" "${tmpdir}/SHA256SUMS" "$archive_name"

    info "Extracting to ${install_dir}..."

    tar xzf "${tmpdir}/${archive_name}" -C "$tmpdir"
    install -m 755 "${tmpdir}/${BINARY}" "${install_dir}/${BINARY}"
}

# --- Main ---

main() {
    local version="${1:-}"
    local target install_dir

    need_cmd uname
    need_cmd tar
    need_cmd install

    target="$(detect_target)"
    install_dir="$(resolve_install_dir)"

    if [ -z "$version" ]; then
        version="$(get_latest_version)"
    fi

    download_and_install "$version" "$target" "$install_dir"

    info ""
    info "Successfully installed ${BINARY} ${version} to ${install_dir}/${BINARY}"

    if ! echo "$PATH" | tr ':' '\n' | grep -qx "$install_dir"; then
        info ""
        info "NOTE: ${install_dir} is not in your PATH."
        info "Add it with:"
        info "  export PATH=\"${install_dir}:\$PATH\""
    fi
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    main "$@"
fi
