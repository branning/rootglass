#!/usr/bin/env python3
"""Collect the exact runtime license and notice files into one release asset."""

from __future__ import annotations

import pathlib
import shutil
import sys
import tarfile
import tempfile
import zipfile


TARGETS = (
    "linux-x86_64",
    "linux-aarch64",
    "macos-arm64",
    "windows-x86_64",
    "windows-aarch64",
)
REQUIRED = {"QEMU-GPL-2.0.txt", "ALPINE-RUNTIME-NOTICE.txt"}


def license_relative_path(name: str) -> pathlib.PurePosixPath | None:
    path = pathlib.PurePosixPath(name)
    if path.is_absolute() or ".." in path.parts or "licenses" not in path.parts:
        return None
    index = path.parts.index("licenses")
    remainder = pathlib.PurePosixPath(*path.parts[index + 1 :])
    if not remainder.parts:
        return None
    return remainder


def main() -> int:
    if len(sys.argv) != 3:
        raise SystemExit("usage: collect-runtime-licenses.py READY_ROOT OUTPUT_TAR_GZ")
    ready_root = pathlib.Path(sys.argv[1]).resolve()
    output = pathlib.Path(sys.argv[2]).resolve()
    with tempfile.TemporaryDirectory(prefix="rootglass-runtime-licenses-") as temp:
        staging = pathlib.Path(temp)
        for target in TARGETS:
            candidates = [
                path
                for path in ready_root.glob(f"rootglass-runtime-{target}-*")
                if path.suffix in {".gz", ".zip"}
            ]
            if len(candidates) != 1:
                raise SystemExit(f"expected one runtime archive for {target}, found {len(candidates)}")
            target_root = staging / target
            target_root.mkdir()
            present: set[str] = set()
            archive = candidates[0]
            if archive.suffix == ".zip":
                with zipfile.ZipFile(archive) as source:
                    for member in source.infolist():
                        relative = license_relative_path(member.filename)
                        if relative is None or member.is_dir():
                            continue
                        destination = target_root.joinpath(*relative.parts)
                        destination.parent.mkdir(parents=True, exist_ok=True)
                        with source.open(member) as input_file, destination.open("wb") as output_file:
                            shutil.copyfileobj(input_file, output_file)
                        present.add(destination.name)
            else:
                with tarfile.open(archive, "r:gz") as source:
                    for member in source.getmembers():
                        relative = license_relative_path(member.name)
                        if relative is None or not member.isfile():
                            continue
                        input_file = source.extractfile(member)
                        if input_file is None:
                            raise SystemExit(f"could not read {member.name} from {archive.name}")
                        destination = target_root.joinpath(*relative.parts)
                        destination.parent.mkdir(parents=True, exist_ok=True)
                        with input_file, destination.open("wb") as output_file:
                            shutil.copyfileobj(input_file, output_file)
                        present.add(destination.name)
            missing = REQUIRED - present
            if missing:
                raise SystemExit(f"{archive.name} is missing required license files: {sorted(missing)}")

        output.parent.mkdir(parents=True, exist_ok=True)
        if output.exists():
            output.unlink()
        with tarfile.open(output, "w:gz") as bundle:
            for path in sorted(staging.rglob("*")):
                if path.is_file():
                    bundle.add(path, arcname=path.relative_to(staging).as_posix())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
