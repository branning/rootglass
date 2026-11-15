#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: $0 <downloaded-artifact-directory> <ready-directory> <release-tag> <signed-artifact-directory>" >&2
  exit 2
fi

artifact_root="$1"
ready_root="$2"
release_tag="$3"
signed_root="$4"
: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
: "${GH_TOKEN:?GH_TOKEN is required}"

normalize_checksum_file() {
  local checksum_file="$1"
  local normalized="${checksum_file}.normalized"
  sed 's/\r$//' "$checksum_file" > "$normalized"
  mv "$normalized" "$checksum_file"
}

release_record="$(bash scripts/find-release-by-tag.sh "$release_tag")"
IFS=$'\t' read -r release_id release_draft release_immutable release_prerelease <<< "$release_record"
echo "Preparing signed assets for draft release $release_tag (release ID $release_id)"
[[ "$release_draft" == true ]] || {
  echo "Release $release_tag must remain a draft while assets are attached" >&2
  exit 1
}
[[ "$release_immutable" == false ]] || {
  echo "Release $release_tag is immutable and cannot accept assets" >&2
  exit 1
}
[[ "$release_prerelease" == true ]] || {
  echo "Release $release_tag must be marked as a prerelease" >&2
  exit 1
}

expected_dirs=(
  rootglass-deb-Linux-X64
  rootglass-deb-Linux-ARM64
  rootglass-dmg-macOS-ARM64
  rootglass-nsis-Windows-X64
  rootglass-nsis-Windows-ARM64
  rootglass-example-glass
)
mapfile -t found_dirs < <(find "$artifact_root" -mindepth 1 -maxdepth 1 -type d \
  ! -name 'rootglass-release-*' -printf '%f\n' | sort)
mapfile -t expected_sorted < <(printf '%s\n' "${expected_dirs[@]}" | sort)
if [[ "${found_dirs[*]}" != "${expected_sorted[*]}" ]]; then
  printf 'Expected package artifacts %s; found %s\n' "${expected_sorted[*]}" "${found_dirs[*]}" >&2
  exit 1
fi

mkdir -p "$ready_root"
for platform in linux linux-arm64 macos windows windows-arm64; do
  case "$platform" in
    linux)
      artifact_dir="$artifact_root/rootglass-deb-Linux-X64"
      package_glob='*.deb'
      runtime_glob='*.tar.gz'
      source_manifest='SHA256SUMS-linux.txt'
      signed_dir=''
      signed_glob=''
      ;;
    linux-arm64)
      artifact_dir="$artifact_root/rootglass-deb-Linux-ARM64"
      package_glob='*.deb'
      runtime_glob='*.tar.gz'
      source_manifest='SHA256SUMS-linux-arm64.txt'
      signed_dir=''
      signed_glob=''
      ;;
    macos)
      artifact_dir="$artifact_root/rootglass-dmg-macOS-ARM64"
      package_glob='*.dmg'
      runtime_glob='*.tar.gz'
      source_manifest='SHA256SUMS-macos.txt'
      signed_dir="$signed_root/rootglass-release-macos-arm64"
      signed_glob='*.dmg'
      ;;
    windows)
      artifact_dir="$artifact_root/rootglass-nsis-Windows-X64"
      package_glob='*-setup.exe'
      runtime_glob='*.zip'
      source_manifest='SHA256SUMS-windows.txt'
      signed_dir="$signed_root/rootglass-release-windows-x64"
      signed_glob='*-setup.exe'
      ;;
    windows-arm64)
      artifact_dir="$artifact_root/rootglass-nsis-Windows-ARM64"
      package_glob='*-setup.exe'
      runtime_glob='*.zip'
      source_manifest='SHA256SUMS-windows-arm64.txt'
      signed_dir="$signed_root/rootglass-release-windows-arm64"
      signed_glob='*-setup.exe'
      ;;
  esac
  mapfile -t packages < <(find "$artifact_dir" -type f -name "$package_glob" -print)
  mapfile -t runtimes < <(find "$artifact_dir" -type f -name "$runtime_glob" -print)
  mapfile -t runtime_checksums < <(find "$artifact_dir" -type f -name '*.sha256' -print)
  if [[ "${#packages[@]}" -ne 1 || "${#runtimes[@]}" -ne 1 || "${#runtime_checksums[@]}" -ne 1 ]]; then
    echo "Expected one installer, runtime archive, and runtime checksum for $platform" >&2
    find "$artifact_dir" -type f -print >&2
    exit 1
  fi
  [[ -f "$artifact_dir/$source_manifest" ]] || {
    echo "Missing source package checksum manifest $source_manifest for $platform" >&2
    exit 1
  }
  normalize_checksum_file "$artifact_dir/$source_manifest"
  normalize_checksum_file "${runtime_checksums[0]}"
  (cd "$artifact_dir" && sha256sum --check "$source_manifest")

  package="${packages[0]}"
  if [[ -n "$signed_dir" ]]; then
    mapfile -t signed_packages < <(find "$signed_dir" -type f -name "$signed_glob" -print)
    if [[ "${#signed_packages[@]}" -ne 1 || "$(basename "${signed_packages[0]}")" != "$(basename "$package")" ]]; then
      echo "Expected one signed installer with the original package name for $platform" >&2
      find "$signed_dir" -type f -print >&2
      exit 1
    fi
    package="${signed_packages[0]}"
  fi

  cp "$package" "${runtimes[0]}" "${runtime_checksums[0]}" "$ready_root/"
  runtime_checksum_name="$(basename "${runtime_checksums[0]}")"
  manifest="SHA256SUMS-$platform.txt"
  (
    cd "$ready_root"
    sha256sum --check "$runtime_checksum_name"
    sha256sum "$(basename "$package")" "$(basename "${runtimes[0]}")" > "$manifest"
    sha256sum --check "$manifest"
  )
done

signing_reports=(
  "$signed_root/rootglass-release-macos-arm64/SIGNING-PROVENANCE-macos-arm64.json"
  "$signed_root/rootglass-release-windows-x64/SIGNING-PROVENANCE-windows-x64.json"
  "$signed_root/rootglass-release-windows-arm64/SIGNING-PROVENANCE-windows-arm64.json"
)
for report in "${signing_reports[@]}"; do
  [[ -f "$report" ]] || { echo "Missing signing report: $report" >&2; exit 1; }
  cp "$report" "$ready_root/"
done
jq -e '.notarization.application.status == "Accepted" and .notarization.application.stapled == true and .notarization.disk_image.status == "Accepted" and .notarization.disk_image.stapled == true' \
  "$ready_root/SIGNING-PROVENANCE-macos-arm64.json" >/dev/null
for architecture in x64 arm64; do
  jq -e '.signature_status == "Valid"' \
    "$ready_root/SIGNING-PROVENANCE-windows-$architecture.json" >/dev/null
done

example_dir="$artifact_root/rootglass-example-glass"
mapfile -t example_glass < <(find "$example_dir" -type f -name 'rootglass-hello-world-*.glass' -print)
mapfile -t example_checksums < <(find "$example_dir" -type f -name 'SHA256SUMS-example.txt' -print)
if [[ "${#example_glass[@]}" -ne 1 || "${#example_checksums[@]}" -ne 1 ]]; then
  echo "Expected one materialized example .glass and its checksum manifest" >&2
  find "$example_dir" -type f -print >&2
  exit 1
fi
cp "${example_glass[0]}" "${example_checksums[0]}" "$ready_root/"
(
  cd "$ready_root"
  sha256sum --check SHA256SUMS-example.txt
)

expected_product_assets=25
mapfile -t product_assets < <(find "$ready_root" -maxdepth 1 -type f -printf '%f\n' | sort)
if [[ "${#product_assets[@]}" -ne "$expected_product_assets" ]]; then
  printf 'Expected %s product/signing assets before release metadata, found %s\n' \
    "$expected_product_assets" "${#product_assets[@]}" >&2
  printf '%s\n' "${product_assets[@]}" >&2
  exit 1
fi
echo "Prepared ${#product_assets[@]} verified product and signing assets in $ready_root"
