#!/usr/bin/env bash
set -euo pipefail

runtime_version="${1:?usage: prepare-qemu-runtime.sh VERSION PLATFORM GUEST_ASSET_DIR}"
platform="${2:?usage: prepare-qemu-runtime.sh VERSION PLATFORM GUEST_ASSET_DIR}"
guest_asset_dir="${3:?usage: prepare-qemu-runtime.sh VERSION PLATFORM GUEST_ASSET_DIR}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
runtime_root="${repo_root}/crates/rootglass-desktop/runtime-assets/${runtime_version}"
bin_dir="${runtime_root}/bin"
lib_dir="${runtime_root}/lib"
share_dir="${runtime_root}/share/qemu"

case "${platform}" in
  linux-x86_64|macos-arm64) ;;
  *) echo "unsupported host platform: ${platform}" >&2; exit 2 ;;
esac

if [[ ! -f "${guest_asset_dir}/vmlinuz-virt" || ! -f "${guest_asset_dir}/initramfs-virt" ]]; then
  echo "guest kernel and initramfs are missing from ${guest_asset_dir}" >&2
  exit 1
fi

command -v qemu-system-aarch64 >/dev/null || {
  echo "qemu-system-aarch64 is not installed" >&2
  exit 1
}

rm -rf "${runtime_root}"
mkdir -p "${bin_dir}" "${lib_dir}" "${share_dir}" "${runtime_root}/boot" "${runtime_root}/licenses"
install -m 0755 "$(command -v qemu-system-aarch64)" "${bin_dir}/qemu-system-aarch64"
install -m 0644 "${guest_asset_dir}/vmlinuz-virt" "${runtime_root}/boot/vmlinuz-virt"
install -m 0644 "${guest_asset_dir}/initramfs-virt" "${runtime_root}/boot/initramfs-virt"
install -m 0644 "${repo_root}/licenses/GPL-2.0.txt" \
  "${runtime_root}/licenses/QEMU-GPL-2.0.txt"
cat > "${runtime_root}/licenses/ALPINE-RUNTIME-NOTICE.txt" <<'NOTICE'
The kernel and initramfs in this runtime bundle are derived from the Alpine Linux
3.23.8 aarch64 netboot release. Alpine Linux and its included components are
distributed under their respective upstream licenses. Source and release files:
https://dl-cdn.alpinelinux.org/alpine/v3.23/releases/aarch64/netboot-3.23.8/
NOTICE

if [[ "${platform}" == linux-x86_64 ]]; then
  if [[ ! -d /usr/share/qemu ]]; then
    echo "the QEMU share directory is missing at /usr/share/qemu" >&2
    exit 1
  fi
  cp -aL /usr/share/qemu/. "${share_dir}/"
  multiarch="$(dpkg-architecture -qDEB_HOST_MULTIARCH)"
  qemu_module_dir="/usr/lib/${multiarch}/qemu"
  if [[ -d "${qemu_module_dir}" ]]; then
    mkdir -p "${lib_dir}/qemu"
    cp -aL "${qemu_module_dir}/." "${lib_dir}/qemu/"
  fi

  if ! command -v lddtree >/dev/null; then
    echo "lddtree is required to collect QEMU's shared libraries" >&2
    exit 1
  fi
  runtime_objects=("${bin_dir}/qemu-system-aarch64")
  if [[ -d "${lib_dir}/qemu" ]]; then
    while IFS= read -r module; do
      runtime_objects+=("${module}")
    done < <(find "${lib_dir}/qemu" -type f -name '*.so*' -print)
  fi
  for runtime_object in "${runtime_objects[@]}"; do
    dependencies="$(lddtree -l "${runtime_object}")"
    while IFS= read -r dependency; do
      [[ -f "${dependency}" ]] || continue
      case "${dependency}" in
        "${runtime_root}"/*) continue ;;
      esac
      case "$(basename "${dependency}")" in
        libc.so.*|libm.so.*|libpthread.so.*|libdl.so.*|librt.so.*|ld-linux*.so.*) continue ;;
      esac
      install -m 0644 "${dependency}" "${lib_dir}/$(basename "${dependency}")"
      package="$(dpkg-query -S "${dependency}" 2>/dev/null | head -n1 | cut -d: -f1 || true)"
      if [[ -n "${package}" && -f "/usr/share/doc/${package}/copyright" ]]; then
        install -m 0644 "/usr/share/doc/${package}/copyright" \
          "${runtime_root}/licenses/${package}-copyright.txt"
      fi
    done <<< "${dependencies}"
  done
  package="$(dpkg-query -S "$(command -v qemu-system-aarch64)" 2>/dev/null | head -n1 | cut -d: -f1 || true)"
  if [[ -n "${package}" && -f "/usr/share/doc/${package}/copyright" ]]; then
    install -m 0644 "/usr/share/doc/${package}/copyright" \
      "${runtime_root}/licenses/${package}-copyright.txt"
  fi
elif [[ "${platform}" == macos-arm64 ]]; then
  qemu_prefix="$(brew --prefix qemu)"
  if [[ -d "${qemu_prefix}/share/qemu" ]]; then
    cp -RL "${qemu_prefix}/share/qemu/." "${share_dir}/"
  fi
  if [[ -d "${qemu_prefix}/lib/qemu" ]]; then
    mkdir -p "${lib_dir}/qemu"
    cp -RL "${qemu_prefix}/lib/qemu/." "${lib_dir}/qemu/"
  fi

  dependency_formulas="$(brew deps qemu; printf '%s\n' qemu)"
  while IFS= read -r formula; do
    [[ -n "${formula}" ]] || continue
    formula_prefix="$(brew --prefix "${formula}")"
    if [[ -d "${formula_prefix}/lib" ]]; then
      while IFS= read -r library; do
        [[ -f "${library}" || -L "${library}" ]] || continue
        install -m 0644 "${library}" "${lib_dir}/$(basename "${library}")"
      done < <(find "${formula_prefix}/lib" -maxdepth 1 \( -type f -o -type l \) -name '*.dylib' -print)
    fi
    while IFS= read -r license_path; do
      [[ -f "${license_path}" ]] || continue
      install -m 0644 "${license_path}" \
        "${runtime_root}/licenses/${formula}-$(basename "${license_path}")"
    done < <(find "${formula_prefix}" -maxdepth 6 -type f \
      \( -iname 'LICENSE*' -o -iname 'COPYING*' -o -iname 'COPYRIGHT*' -o -iname 'NOTICE*' \) -print)
  done <<< "${dependency_formulas}"
  install_name_tool -add_rpath "@executable_path/../lib" \
    "${bin_dir}/qemu-system-aarch64"
  while IFS= read -r library; do
    install_name_tool -add_rpath "@loader_path" "${library}"
  done < <(find "${lib_dir}" -maxdepth 1 -type f -name '*.dylib' -print)
fi

if [[ -z "$(find "${share_dir}" -mindepth 1 -print -quit)" ]]; then
  echo "QEMU share data was not found; refusing to create an incomplete runtime" >&2
  exit 1
fi

python3 "${repo_root}/scripts/write-runtime-manifest.py" \
  "${runtime_root}" "${runtime_version}" "${platform}" "${bin_dir}/qemu-system-aarch64"

archive="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/rootglass-runtime-${platform}-${runtime_version}.tar.gz"
tar -czf "${archive}" -C "${repo_root}/crates/rootglass-desktop/runtime-assets" "${runtime_version}"
python3 - "${archive}" <<'PY'
import hashlib
import pathlib
import sys

archive = pathlib.Path(sys.argv[1])
hasher = hashlib.sha256()
with archive.open("rb") as source:
    for chunk in iter(lambda: source.read(1024 * 1024), b""):
        hasher.update(chunk)
digest = hasher.hexdigest()
archive.with_suffix(archive.suffix + ".sha256").write_text(
    f"{digest}  {archive.name}\n", encoding="ascii"
)
PY
echo "runtime archive: ${archive}"
