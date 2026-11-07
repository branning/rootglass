#!/usr/bin/env python3
"""Build signed libghostty-vt sources and run the portable PTY smoke."""

from __future__ import annotations

import hashlib
import os
import platform
import subprocess
import tarfile
import tempfile
import urllib.request
import zipfile
from pathlib import Path


GHOSTTY_VT_VERSION = "1.0.0"
ZIG_VERSION = "0.17.0"
GHOSTTY_MINISIGN_PUBLIC_KEY = "RWQlAjJC23149WL2sEpT/l0QKy7hMIFhYdQOFy0Z7z7PbneUgvlsnYcV"
ROOT_DIR = Path(__file__).resolve().parent.parent

ZIG_ARCHIVES = {
    ("Linux", "x86_64"): (
        "zig-x86_64-linux-0.17.0.tar.xz",
        "1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026",
        "tar.xz",
    ),
    ("Darwin", "arm64"): (
        "zig-aarch64-macos-0.17.0.tar.xz",
        "b607e9b9234790a008116ae5bdb71c6243b84b9fb42a53a9e70fde41c06c536a",
        "tar.xz",
    ),
    ("Windows", "x86_64"): (
        "zig-x86_64-windows-0.17.0.zip",
        "b5663f69581dcf391293fbf16c06cb80d81d806545ce618b4d0bab7f0eb8c428",
        "zip",
    ),
}


def download(url: str, destination: Path) -> None:
    request = urllib.request.Request(url, headers={"User-Agent": "rootglass-ci"})
    with urllib.request.urlopen(request, timeout=120) as response:
        destination.write_bytes(response.read())


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_manifest_entry(manifest: Path, artifact: Path) -> None:
    for line in manifest.read_text(encoding="utf-8").splitlines():
        fields = line.split(maxsplit=1)
        if len(fields) == 2 and fields[1].lstrip("* ") == artifact.name:
            expected = fields[0].lower()
            actual = sha256(artifact)
            if actual != expected:
                raise RuntimeError(
                    f"SHA256 mismatch for {artifact.name}: expected {expected}, got {actual}"
                )
            return
    raise RuntimeError(f"{artifact.name} is missing from the signed checksum manifest")


def system(command: list[str], *, cwd: Path | None = None, env: dict[str, str] | None = None) -> None:
    print("+", " ".join(command), flush=True)
    subprocess.run(command, cwd=cwd, env=env, check=True)


def normalized_architecture() -> str:
    architecture = platform.machine().lower()
    if architecture in {"amd64", "x64"}:
        return "x86_64"
    if architecture in {"aarch64", "arm64"}:
        return "arm64"
    return architecture


def main() -> None:
    host = (platform.system(), normalized_architecture())
    if host not in ZIG_ARCHIVES:
        raise SystemExit(f"unsupported smoke host {host}; expected Linux x86_64, macOS arm64, or Windows x86_64")

    zig_archive_name, zig_checksum, zig_format = ZIG_ARCHIVES[host]
    release_url = f"https://release.files.ghostty.org/libghostty-vt/{GHOSTTY_VT_VERSION}"

    with tempfile.TemporaryDirectory(prefix="rootglass-libghostty-vt-") as temporary:
        work = Path(temporary)
        manifest = work / "SHA256SUMS"
        signature = work / "SHA256SUMS.minisig"
        source_archive = work / f"libghostty-vt-{GHOSTTY_VT_VERSION}.tar.gz"
        deps_archive = work / f"libghostty-vt-{GHOSTTY_VT_VERSION}-deps.tar.gz"
        download(f"{release_url}/SHA256SUMS", manifest)
        download(f"{release_url}/SHA256SUMS.minisig", signature)
        download(f"{release_url}/{source_archive.name}", source_archive)
        download(f"{release_url}/{deps_archive.name}", deps_archive)

        system(
            [
                "minisign",
                "-V",
                "-m",
                str(manifest),
                "-P",
                GHOSTTY_MINISIGN_PUBLIC_KEY,
            ]
        )
        verify_manifest_entry(manifest, source_archive)
        verify_manifest_entry(manifest, deps_archive)

        zig_archive = work / zig_archive_name
        download(f"https://ziglang.org/download/{ZIG_VERSION}/{zig_archive_name}", zig_archive)
        actual_zig_checksum = sha256(zig_archive)
        if actual_zig_checksum != zig_checksum:
            raise RuntimeError(
                f"SHA256 mismatch for {zig_archive.name}: expected {zig_checksum}, got {actual_zig_checksum}"
            )

        if zig_format == "zip":
            with zipfile.ZipFile(zig_archive) as archive:
                archive.extractall(work)
            zig_binary = work / f"zig-x86_64-windows-{ZIG_VERSION}" / "zig.exe"
        else:
            with tarfile.open(zig_archive, "r:xz") as archive:
                archive.extractall(work, filter="data")
            zig_dir = "zig-x86_64-linux" if host[0] == "Linux" else "zig-aarch64-macos"
            zig_binary = work / f"{zig_dir}-{ZIG_VERSION}" / "zig"

        source_dir = work / f"libghostty-vt-{GHOSTTY_VT_VERSION}"
        deps_dir = work / f"libghostty-vt-{GHOSTTY_VT_VERSION}-deps"
        with tarfile.open(source_archive, "r:gz") as archive:
            archive.extractall(work, filter="data")
        with tarfile.open(deps_archive, "r:gz") as archive:
            archive.extractall(work, filter="data")

        prefix = work / "prefix"
        zig_cache = work / "zig-global-cache"
        prefix.mkdir()
        zig_cache.mkdir()
        build_env = os.environ.copy()
        build_env["ZIG_GLOBAL_CACHE_DIR"] = str(zig_cache)
        system(
            [
                str(zig_binary),
                "build",
                "--prefix",
                str(prefix),
                "--system",
                str(deps_dir),
                "-Doptimize=ReleaseFast",
                "-Dcpu=baseline",
            ],
            cwd=source_dir,
            env=build_env,
        )

        library = prefix / "lib"
        if host[0] == "Linux":
            expected_library = library / "libghostty-vt.so"
        elif host[0] == "Darwin":
            expected_library = library / "libghostty-vt.dylib"
        else:
            expected_library = library / "ghostty-vt.lib"
        if not expected_library.is_file():
            raise RuntimeError(f"native libghostty-vt build did not install {expected_library}")

        test_env = os.environ.copy()
        test_env["GHOSTTY_VT_PREFIX"] = str(prefix)
        test_env["PATH"] = str(prefix / "bin") + os.pathsep + test_env.get("PATH", "")
        test_env["CARGO_PROFILE_DEV_DEBUG"] = "0"
        test_env["CARGO_PROFILE_TEST_DEBUG"] = "0"
        if host[0] == "Linux":
            test_env["LD_LIBRARY_PATH"] = str(library) + os.pathsep + test_env.get("LD_LIBRARY_PATH", "")
        elif host[0] == "Darwin":
            test_env["DYLD_LIBRARY_PATH"] = str(library) + os.pathsep + test_env.get("DYLD_LIBRARY_PATH", "")

        system(
            [
                "cargo",
                "test",
                "--locked",
                "--manifest-path",
                str(ROOT_DIR / "prototypes/libghostty-vt-ratatui/Cargo.toml"),
                "--",
                "--nocapture",
            ],
            cwd=ROOT_DIR,
            env=test_env,
        )


if __name__ == "__main__":
    main()
