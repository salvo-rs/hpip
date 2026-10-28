"""Reject tags that do not describe the checked-out Cargo package."""

import os
import pathlib
import re
import tomllib


def release_version(tag, manifest):
    match = re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag)
    if match is None:
        raise ValueError("release tag must be vMAJOR.MINOR.PATCH")
    version = tag[1:]
    if version != manifest["package"]["version"]:
        raise ValueError("release tag does not match Cargo.toml package.version")
    return version


if __name__ == "__main__":
    with pathlib.Path("Cargo.toml").open("rb") as source:
        manifest = tomllib.load(source)
    version = release_version(os.environ["GITHUB_REF_NAME"], manifest)
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        output.write(f"version={version}\n")
    print(f"Validated hpip {version}")
