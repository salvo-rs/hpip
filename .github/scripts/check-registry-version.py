"""Check an exact release version against the official Cargo sparse index."""

import argparse
import json
import os
import re
import sys
import urllib.request


INDEX_URL = "https://index.crates.io/hp/ip/hpip"
USER_AGENT = "hpip-release (https://github.com/salvo-rs/hpip)"
NUMBER = r"(?:0|[1-9][0-9]*)"
RELEASE = rf"{NUMBER}\.{NUMBER}\.{NUMBER}"
PRERELEASE_ID = r"(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
SEMVER = re.compile(
    rf"{RELEASE}(?:-{PRERELEASE_ID}(?:\.{PRERELEASE_ID})*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)


def index_versions(data):
    lines = data.decode("utf-8").splitlines()
    if not lines:
        raise ValueError("registry index is empty")
    versions = set()
    for line in lines:
        entry = json.loads(line)
        if not isinstance(entry, dict) or entry.get("name") != "hpip":
            raise ValueError("registry index has an unexpected crate name")
        version = entry.get("vers")
        if not isinstance(version, str) or SEMVER.fullmatch(version) is None:
            raise ValueError("registry index has an invalid version")
        checksum = entry.get("cksum")
        if (
            not isinstance(checksum, str)
            or re.fullmatch(r"[0-9a-f]{64}", checksum) is None
            or not isinstance(entry.get("deps"), list)
            or not isinstance(entry.get("features"), dict)
            or not isinstance(entry.get("yanked"), bool)
        ):
            raise ValueError("registry index has an invalid record")
        versions.add(version)
    return versions


def version_exists(version):
    if re.fullmatch(RELEASE, version) is None:
        raise ValueError("release version must be MAJOR.MINOR.PATCH")
    request = urllib.request.Request(
        INDEX_URL, headers={"User-Agent": USER_AGENT, "Accept": "text/plain"}
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        if response.status != 200:
            raise ValueError(f"registry index returned HTTP {response.status}")
        return version in index_versions(response.read())


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("version")
    args = parser.parse_args()
    try:
        exists = version_exists(args.version)
    except (OSError, ValueError) as error:
        print(f"Cannot verify the registry version: {error}", file=sys.stderr)
        return 1
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        output.write(f"exists={str(exists).lower()}\n")
    print(f"hpip {args.version} present in the registry index: {str(exists).lower()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
