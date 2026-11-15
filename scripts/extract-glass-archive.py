#!/usr/bin/env python3
"""Extract a Rootglass .glass ZIP archive into a directory safely."""

from __future__ import annotations

import sys
from pathlib import Path, PurePosixPath
from zipfile import ZipFile


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit("usage: extract-glass-archive.py ARCHIVE DESTINATION")

    archive_path = Path(sys.argv[1])
    destination = Path(sys.argv[2]).resolve()
    destination.mkdir(parents=True, exist_ok=True)

    with ZipFile(archive_path) as archive:
        for entry in archive.infolist():
            relative = PurePosixPath(entry.filename)
            if relative.is_absolute() or ".." in relative.parts:
                raise SystemExit(f"unsafe path in .glass archive: {entry.filename}")
            target = (destination / Path(*relative.parts)).resolve()
            if target != destination and destination not in target.parents:
                raise SystemExit(f"unsafe path in .glass archive: {entry.filename}")

            if entry.is_dir():
                target.mkdir(parents=True, exist_ok=True)
                continue

            target.parent.mkdir(parents=True, exist_ok=True)
            with archive.open(entry) as source, target.open("wb") as output:
                while chunk := source.read(1024 * 1024):
                    output.write(chunk)

    required = ("rootglass.toml", "rootfs.img", "state.img", "signatures/glass.dsse.json")
    missing = [name for name in required if not (destination / name).is_file()]
    if missing:
        raise SystemExit(
            "materialized .glass archive is missing required files: " + ", ".join(missing)
        )


if __name__ == "__main__":
    main()
