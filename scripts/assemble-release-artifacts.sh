#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <downloaded-artifact-directory> <ready-directory> <release-tag>" >&2
  exit 2
fi

artifact_root="$1"
ready_root="$2"
release_tag="$3"
: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
: "${GH_TOKEN:?GH_TOKEN is required}"

release_record="$(bash scripts/find-release-by-tag.sh "$release_tag")"
IFS=$'\t' read -r release_id release_draft release_immutable release_prerelease <<< "$release_record"
echo "Preparing assets for draft release $release_tag (release ID $release_id)"
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
  rootglass-dmg-macOS-ARM64
  rootglass-nsis-Windows-X64
  rootglass-example-glass
)
mapfile -t found_dirs < <(find "$artifact_root" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort)
mapfile -t expected_sorted < <(printf '%s\n' "${expected_dirs[@]}" | sort)
if [[ "${found_dirs[*]}" != "${expected_sorted[*]}" ]]; then
  printf 'Expected package artifacts %s; found %s\n' "${expected_sorted[*]}" "${found_dirs[*]}" >&2
  exit 1
fi

mkdir -p "$ready_root"
declare -a assets=()
for platform in linux macos windows; do
  case "$platform" in
    linux) artifact_dir="$artifact_root/rootglass-deb-Linux-X64"; package_glob='*.deb'; runtime_glob='*.tar.gz' ;;
    macos) artifact_dir="$artifact_root/rootglass-dmg-macOS-ARM64"; package_glob='*.dmg'; runtime_glob='*.tar.gz' ;;
    windows) artifact_dir="$artifact_root/rootglass-nsis-Windows-X64"; package_glob='*-setup.exe'; runtime_glob='*.zip' ;;
  esac
  mapfile -t packages < <(find "$artifact_dir" -type f -name "$package_glob" -print)
  mapfile -t runtimes < <(find "$artifact_dir" -type f -name "$runtime_glob" -print)
  mapfile -t runtime_checksums < <(find "$artifact_dir" -type f -name '*.sha256' -print)
  if [[ "${#packages[@]}" -ne 1 || "${#runtimes[@]}" -ne 1 || "${#runtime_checksums[@]}" -ne 1 ]]; then
    echo "Expected one installer, runtime archive, and runtime checksum for $platform" >&2
    find "$artifact_dir" -type f -print >&2
    exit 1
  fi

  destination="$ready_root/$platform"
  mkdir -p "$destination"
  cp "${packages[0]}" "${runtimes[0]}" "${runtime_checksums[0]}" "$destination/"
  runtime_checksum_name="$(basename "${runtime_checksums[0]}")"
  manifest="SHA256SUMS-$platform.txt"
  (
    cd "$destination"
    sha256sum --check "$runtime_checksum_name"
    sha256sum "$(basename "${packages[0]}")" "$(basename "${runtimes[0]}")" > "$manifest"
    sha256sum --check "$manifest"
  )
  assets+=("$destination/$(basename "${packages[0]}")" "$destination/$manifest" "$destination/$(basename "${runtimes[0]}")" "$destination/$runtime_checksum_name")
done

example_dir="$artifact_root/rootglass-example-glass"
mapfile -t example_glass < <(find "$example_dir" -type f -name 'rootglass-hello-world-*.glass' -print)
mapfile -t example_checksums < <(find "$example_dir" -type f -name 'SHA256SUMS-example.txt' -print)
if [[ "${#example_glass[@]}" -ne 1 || "${#example_checksums[@]}" -ne 1 ]]; then
  echo "Expected one materialized example .glass and its checksum manifest" >&2
  find "$example_dir" -type f -print >&2
  exit 1
fi
mkdir -p "$ready_root/example"
cp "${example_glass[0]}" "${example_checksums[0]}" "$ready_root/example/"
(
  cd "$ready_root/example"
  sha256sum --check SHA256SUMS-example.txt
)
assets+=("$ready_root/example/$(basename "${example_glass[0]}")" "$ready_root/example/SHA256SUMS-example.txt")

if [[ "${#assets[@]}" -ne 14 ]]; then
  printf 'Expected 14 release assets, found %s\n' "${#assets[@]}" >&2
  printf '%s\n' "${assets[@]}" >&2
  exit 1
fi

gh release upload "$release_tag" "${assets[@]}" --repo "$GITHUB_REPOSITORY" --clobber
