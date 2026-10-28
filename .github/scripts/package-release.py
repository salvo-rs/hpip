"""Package only the five validated platform binaries and write their checksums."""

import argparse
import hashlib
import pathlib
import tarfile
import zipfile


TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)


def package_release(artifacts, output):
    artifacts, output = pathlib.Path(artifacts), pathlib.Path(output)
    binaries = []
    for target in TARGETS:
        name = "hpip.exe" if "windows" in target else "hpip"
        source = artifacts / f"hpip-{target}" / name
        if not source.is_file() or source.is_symlink():
            raise ValueError(f"missing validated binary: {source}")
        binaries.append((target, name, source))
    output.mkdir(parents=True, exist_ok=True)
    archives = []
    for target, name, source in binaries:
        if "windows" in target:
            archive = output / f"hpip-{target}.zip"
            with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as writer:
                writer.write(source, arcname=name)
        else:
            archive = output / f"hpip-{target}.tar.gz"
            with tarfile.open(archive, "w:gz") as writer:
                metadata = writer.gettarinfo(str(source), arcname=name)
                metadata.mode = 0o755
                with source.open("rb") as content:
                    writer.addfile(metadata, content)
        archives.append(archive)
    with (output / "SHA256SUMS").open("w", encoding="ascii", newline="\n") as manifest:
        for archive in archives:
            with archive.open("rb") as source:
                digest = hashlib.file_digest(source, "sha256").hexdigest()
            manifest.write(f"{digest}  {archive.name}\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--artifacts", default="artifacts")
    parser.add_argument("--output", default="release")
    args = parser.parse_args()
    package_release(args.artifacts, args.output)
