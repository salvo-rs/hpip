#!/usr/bin/env bash
set -euo pipefail

script_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
# shellcheck source-path=SCRIPTDIR
# shellcheck source=../../install.sh
source "$script_root/install.sh"

fixture=$(mktemp -d)
# shellcheck disable=SC2064
trap "rm -rf -- '$fixture'" EXIT
printf 'fixture' > "$fixture/archive.tar.gz"
if command -v sha256sum > /dev/null 2>&1; then
    digest=$(sha256sum "$fixture/archive.tar.gz" | awk '{print $1}')
else
    digest=$(shasum -a 256 "$fixture/archive.tar.gz" | awk '{print $1}')
fi
printf '%s  archive.tar.gz\n' "$digest" > "$fixture/SHA256SUMS"
verify_checksum "$fixture/archive.tar.gz" "$fixture/SHA256SUMS" archive.tar.gz
printf 'corrupt' >> "$fixture/archive.tar.gz"
if (verify_checksum "$fixture/archive.tar.gz" "$fixture/SHA256SUMS" archive.tar.gz); then
    echo 'Corrupt archive unexpectedly passed verification' >&2
    exit 1
fi
if (verify_checksum "$fixture/archive.tar.gz" "$fixture/SHA256SUMS" missing.tar.gz); then
    echo 'Missing checksum unexpectedly passed verification' >&2
    exit 1
fi
printf '%s  archive.tar.gz\n' "$digest" >> "$fixture/SHA256SUMS"
if (verify_checksum "$fixture/archive.tar.gz" "$fixture/SHA256SUMS" archive.tar.gz); then
    echo 'Duplicate checksum unexpectedly passed verification' >&2
    exit 1
fi

uname() {
    case "$1" in
        -s) printf '%s\n' "$fixture_os" ;;
        -m) printf '%s\n' "$fixture_arch" ;;
        *) return 1 ;;
    esac
}
for fixture_os in Linux Darwin; do
    for fixture_arch in x86_64 aarch64; do
        target=$(detect_target)
        case "$target" in
            x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|x86_64-apple-darwin|aarch64-apple-darwin) ;;
            *) echo "Unexpected installer target: $target" >&2; exit 1 ;;
        esac
    done
done
echo 'Unix installer checks passed'
