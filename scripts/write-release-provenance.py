#!/usr/bin/env python3
"""Record the exact inputs, runtime inventories, hashes, and qualification refs."""

from __future__ import annotations

import hashlib
import json
import pathlib
import sys
import tarfile
import tomllib
import zipfile


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_json(data: bytes, label: str) -> dict:
    try:
        return json.loads(data.decode("utf-8-sig"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise SystemExit(f"invalid JSON in {label}: {error}") from error


def sha256_stream(source) -> str:
    digest = hashlib.sha256()
    for chunk in iter(lambda: source.read(1024 * 1024), b""):
        digest.update(chunk)
    return digest.hexdigest()


def runtime_manifest(path: pathlib.Path) -> tuple[dict, dict]:
    if path.suffix == ".zip":
        with zipfile.ZipFile(path) as archive:
            info_names = [name for name in archive.namelist() if name.endswith("/runtime-info.json")]
            asset_names = [name for name in archive.namelist() if name.endswith("/assets.json")]
            if len(info_names) != 1 or len(asset_names) != 1:
                raise SystemExit(f"expected one runtime-info.json and assets.json in {path.name}")
            info = read_json(archive.read(info_names[0]), f"{path.name}:{info_names[0]}")
            assets = read_json(archive.read(asset_names[0]), f"{path.name}:{asset_names[0]}")
    else:
        with tarfile.open(path, "r:gz") as archive:
            info_members = [member for member in archive.getmembers() if member.name.endswith("/runtime-info.json")]
            asset_members = [member for member in archive.getmembers() if member.name.endswith("/assets.json")]
            if len(info_members) != 1 or len(asset_members) != 1:
                raise SystemExit(f"expected one runtime-info.json and assets.json in {path.name}")
            info_file = archive.extractfile(info_members[0])
            assets_file = archive.extractfile(asset_members[0])
            if info_file is None or assets_file is None:
                raise SystemExit(f"could not read runtime metadata in {path.name}")
            info = read_json(info_file.read(), f"{path.name}:{info_members[0].name}")
            assets = read_json(assets_file.read(), f"{path.name}:{asset_members[0].name}")
    return info, assets


def verify_runtime_assets(path: pathlib.Path, info: dict, assets: dict) -> None:
    version = info.get("runtime_version")
    if not isinstance(version, str) or not version:
        raise SystemExit(f"runtime version is missing in {path.name}")
    records = assets.get("assets")
    if not isinstance(records, list) or not records:
        raise SystemExit(f"runtime asset list is empty in {path.name}")
    support = sorted(
        (item.get("path"), item.get("sha256"))
        for item in info.get("support_assets", [])
    )
    listed_support = sorted(
        (item.get("path"), item.get("sha256"))
        for item in records
        if item.get("kind") == "support"
    )
    if support != listed_support:
        raise SystemExit(f"runtime support inventory differs between manifests in {path.name}")

    if path.suffix == ".zip":
        with zipfile.ZipFile(path) as archive:
            for asset in records:
                relative = pathlib.PurePosixPath(asset.get("path", ""))
                if relative.is_absolute() or ".." in relative.parts or not relative.parts:
                    raise SystemExit(f"unsafe runtime asset path in {path.name}: {relative}")
                member = f"{version}/{relative.as_posix()}"
                try:
                    with archive.open(member) as source:
                        actual = sha256_stream(source)
                except KeyError as error:
                    raise SystemExit(f"runtime asset {member} is missing from {path.name}") from error
                if actual != asset.get("sha256"):
                    raise SystemExit(f"runtime asset checksum mismatch: {path.name}:{relative}")
    else:
        with tarfile.open(path, "r:gz") as archive:
            members = {member.name: member for member in archive.getmembers() if member.isfile()}
            for asset in records:
                relative = pathlib.PurePosixPath(asset.get("path", ""))
                if relative.is_absolute() or ".." in relative.parts or not relative.parts:
                    raise SystemExit(f"unsafe runtime asset path in {path.name}: {relative}")
                member_name = f"{version}/{relative.as_posix()}"
                member = members.get(member_name)
                if member is None:
                    raise SystemExit(f"runtime asset {member_name} is missing from {path.name}")
                source = archive.extractfile(member)
                if source is None:
                    raise SystemExit(f"could not read runtime asset {member_name} in {path.name}")
                with source:
                    actual = sha256_stream(source)
                if actual != asset.get("sha256"):
                    raise SystemExit(f"runtime asset checksum mismatch: {path.name}:{relative}")


def verify_runtime_checksum(archive: pathlib.Path, checksum: pathlib.Path) -> str:
    expected, named = checksum.read_text(encoding="ascii").strip().split(maxsplit=1)
    if pathlib.PurePath(named.strip(" *")).name != archive.name:
        raise SystemExit(f"runtime checksum {checksum.name} names an unexpected archive")
    actual = sha256(archive)
    if actual != expected:
        raise SystemExit(f"runtime archive checksum mismatch: {archive.name}")
    return actual


def main() -> int:
    if len(sys.argv) != 7:
        raise SystemExit(
            "usage: write-release-provenance.py READY_ROOT RELEASE_TAG SOURCE_COMMIT "
            "PACKAGE_RUN_ID SIGNING_RUN_ID REPOSITORY"
        )
    ready_root = pathlib.Path(sys.argv[1]).resolve()
    release_tag, source_commit, package_run_id, signing_run_id, repository = sys.argv[2:]
    desktop_manifest = pathlib.Path("crates/rootglass-desktop/Cargo.toml")
    version = tomllib.loads(desktop_manifest.read_text(encoding="utf-8"))["package"]["version"]
    if release_tag.removeprefix("v") != version:
        raise SystemExit(f"release tag {release_tag} does not match desktop version {version}")
    if len(source_commit) != 40 or any(char not in "0123456789abcdef" for char in source_commit):
        raise SystemExit("source commit must be a full lowercase Git commit SHA")

    runtime_archives = {
        "linux-x86_64": "rootglass-runtime-linux-x86_64-",
        "linux-aarch64": "rootglass-runtime-linux-aarch64-",
        "macos-arm64": "rootglass-runtime-macos-arm64-",
        "windows-x86_64": "rootglass-runtime-windows-x86_64-",
        "windows-aarch64": "rootglass-runtime-windows-aarch64-",
    }
    runtimes = []
    for target, prefix in runtime_archives.items():
        archives = [path for path in ready_root.glob(prefix + "*") if path.suffix in {".gz", ".zip"}]
        if len(archives) != 1:
            raise SystemExit(f"expected one runtime archive for {target}, found {len(archives)}")
        archive = archives[0]
        checksum = pathlib.Path(str(archive) + ".sha256")
        if not checksum.is_file():
            raise SystemExit(f"missing runtime checksum for {archive.name}")
        archive_sha256 = verify_runtime_checksum(archive, checksum)
        info, assets = runtime_manifest(archive)
        if info.get("target_platform") != target:
            raise SystemExit(f"runtime target mismatch in {archive.name}: {info.get('target_platform')}")
        if info.get("runtime_version") != version:
            raise SystemExit(f"runtime version mismatch in {archive.name}: {info.get('runtime_version')}")
        verify_runtime_assets(archive, info, assets)
        if info.get("qemu_sha256") != next(
            (asset["sha256"] for asset in assets.get("assets", []) if asset.get("kind") == "qemu"), None
        ):
            raise SystemExit(f"QEMU hash differs between manifests in {archive.name}")
        runtimes.append(
            {
                "target_platform": target,
                "runtime_version": info.get("runtime_version"),
                "archive": {"name": archive.name, "sha256": archive_sha256},
                "qemu": {
                    "version": info.get("qemu_version"),
                    "source": info.get("qemu_source"),
                    "source_url": info.get("qemu_source_url"),
                    "sha256": info.get("qemu_sha256"),
                },
                "guest": {
                    "os": info.get("guest_os"),
                    "version": info.get("guest_version"),
                    "source_url": info.get("guest_source_url"),
                },
                "assets": assets.get("assets", []),
                "license_files": info.get("license_files", []),
            }
        )

    signing_reports = []
    expected_reports = [
        "SIGNING-PROVENANCE-macos-arm64.json",
        "SIGNING-PROVENANCE-windows-x64.json",
        "SIGNING-PROVENANCE-windows-arm64.json",
    ]
    for name in expected_reports:
        path = ready_root / name
        if not path.is_file():
            raise SystemExit(f"missing platform signing report: {name}")
        signing_reports.append(read_json(path.read_bytes(), name))

    evidence_path = pathlib.Path("scripts/release-platform-evidence.json")
    qualification = read_json(evidence_path.read_bytes(), str(evidence_path))
    artifacts = []
    for path in sorted(ready_root.iterdir()):
        if path.is_file() and path.name != "RELEASE-PROVENANCE.json":
            artifacts.append({"name": path.name, "size_bytes": path.stat().st_size, "sha256": sha256(path)})

    provenance = {
        "schema_version": 1,
        "product": "Rootglass",
        "version": version,
        "release_tag": release_tag,
        "source": {
            "repository": repository,
            "commit": source_commit,
            "ref": f"refs/tags/{release_tag}",
            "package_workflow_run": {
                "id": package_run_id,
                "url": f"https://github.com/{repository}/actions/runs/{package_run_id}",
            },
            "signing_workflow_run": {
                "id": signing_run_id,
                "url": f"https://github.com/{repository}/actions/runs/{signing_run_id}",
            },
        },
        "artifacts": artifacts,
        "runtimes": runtimes,
        "application_signing": signing_reports,
        "platform_qualification": qualification,
        "qualification_scope": "Historical clean-install evidence describes platform capability; it is not a test of these exact release bytes.",
        "notices": [
            "LICENSE",
            "THIRD-PARTY-NOTICES.md",
            "THIRD-PARTY-CARGO-LICENSES.html",
            "THIRD-PARTY-RUNTIME-LICENSES.tar.gz",
            "rootglass-source-sbom.spdx.json",
            "rootglass-release-sbom.spdx.json",
        ],
    }
    (ready_root / "RELEASE-PROVENANCE.json").write_text(
        json.dumps(provenance, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
