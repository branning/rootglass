#!/usr/bin/env python3
"""Write the versioned asset manifest and inventory for a packaged runtime."""

import hashlib
import json
import pathlib
import platform as host_platform
import subprocess
import sys


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> int:
    if len(sys.argv) != 5:
        raise SystemExit(
            "usage: write-runtime-manifest.py RUNTIME_ROOT VERSION PLATFORM QEMU_BINARY"
        )
    runtime_root = pathlib.Path(sys.argv[1]).resolve()
    runtime_version = sys.argv[2]
    target_platform = sys.argv[3]
    qemu_binary = pathlib.Path(sys.argv[4]).resolve()
    paths = {
        "kernel": runtime_root / "boot" / "vmlinuz-virt",
        "initramfs": runtime_root / "boot" / "initramfs-virt",
        "qemu": qemu_binary,
    }
    for kind, path in paths.items():
        if not path.is_file():
            raise SystemExit(f"missing {kind} runtime asset: {path}")
        try:
            path.relative_to(runtime_root)
        except ValueError as error:
            raise SystemExit(f"runtime asset is outside its root: {path}") from error

    support_paths = sorted(
        path
        for directory in (
            runtime_root / "bin",
            runtime_root / "lib",
            runtime_root / "share" / "qemu",
        )
        if directory.is_dir()
        for path in directory.rglob("*")
        if path.is_file()
        and path.resolve() != qemu_binary
    )
    support_assets = [
        {
            "kind": "support",
            "path": path.relative_to(runtime_root).as_posix(),
            "sha256": sha256(path),
        }
        for path in support_paths
    ]

    manifest = {
        "schema_version": 1,
        "runtime_version": runtime_version,
        "assets": [
            {
                "kind": kind,
                "path": path.relative_to(runtime_root).as_posix(),
                "sha256": sha256(path),
            }
            for kind, path in paths.items()
        ]
        + support_assets,
    }
    (runtime_root / "assets.json").write_text(
        json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
    )
    qemu_version = subprocess.run(
        [str(qemu_binary), "--version"], check=True, capture_output=True, text=True
    ).stdout.splitlines()[0]
    info = {
        "runtime_version": runtime_version,
        "target_platform": target_platform,
        "host_platform": host_platform.platform(),
        "guest_os": "Alpine Linux",
        "guest_version": "3.23.8",
        "guest_source_url": "https://dl-cdn.alpinelinux.org/alpine/v3.23/releases/aarch64/netboot-3.23.8/",
        "qemu_version": qemu_version,
        "qemu_source": {
            "linux-x86_64": "Ubuntu 24.04 qemu-system-arm package",
            "macos-arm64": "Homebrew qemu formula",
            "windows-x86_64": "Chocolatey QEMU package",
        }[target_platform],
        "qemu_source_url": {
            "linux-x86_64": "https://packages.ubuntu.com/noble/qemu-system-arm",
            "macos-arm64": "https://formulae.brew.sh/formula/qemu",
            "windows-x86_64": "https://community.chocolatey.org/packages/qemu",
        }[target_platform],
        "qemu_source_code_url": {
            "linux-x86_64": "https://launchpad.net/ubuntu/+source/qemu",
            "macos-arm64": "https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/q/qemu.rb",
            "windows-x86_64": "https://download.qemu.org/",
        }[target_platform],
        "qemu_sha256": sha256(qemu_binary),
        "support_assets": support_assets,
        "license_files": sorted(
            path.relative_to(runtime_root).as_posix()
            for path in (runtime_root / "licenses").rglob("*")
            if path.is_file()
        ),
    }
    (runtime_root / "runtime-info.json").write_text(
        json.dumps(info, indent=2) + "\n", encoding="utf-8"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
