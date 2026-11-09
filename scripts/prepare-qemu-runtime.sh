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
  ipxe_qemu_dir=/usr/lib/ipxe/qemu
  if [[ ! -f "${ipxe_qemu_dir}/efi-virtio.rom" ]]; then
    echo "the required EFI virtio ROM is missing at ${ipxe_qemu_dir}/efi-virtio.rom" >&2
    echo "install the ipxe-qemu package before preparing the Linux runtime" >&2
    exit 1
  fi
  cp -aL "${ipxe_qemu_dir}/." "${share_dir}/"
  if [[ -f /usr/share/doc/ipxe-qemu/copyright ]]; then
    install -m 0644 /usr/share/doc/ipxe-qemu/copyright \
      "${runtime_root}/licenses/ipxe-qemu-copyright.txt"
  fi
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

  # Homebrew QEMU and its dependencies carry absolute /opt/homebrew load
  # commands. Copying the dylibs is not enough: dyld still tries the original
  # build-machine paths unless each Mach-O image is rewritten to use the
  # bundled runtime directory.
  macho_images=("${bin_dir}/qemu-system-aarch64")
  while IFS= read -r image; do
    macho_images+=("${image}")
  done < <(find "${lib_dir}" -type f \( -name '*.dylib' -o -name '*.so' \) -print)

  read_macho_load_paths() {
    otool -L "$1" | awk '
      NR > 1 {
        sub(/^[[:space:]]*/, "")
        sub(/[[:space:]]+\(compatibility version.*$/, "")
        print
      }
    '
  }

  read_macho_rpaths() {
    otool -l "$1" | awk '
      /cmd LC_RPATH/ {
        getline
        getline
        sub(/^[[:space:]]*path[[:space:]]+/, "")
        sub(/[[:space:]]+\(offset [0-9]+\)$/, "")
        print
      }
    '
  }

  while IFS= read -r library; do
    library_id="$(otool -D "${library}" | awk 'NR > 1 && NF { print; exit }')"
    if [[ -n "${library_id}" ]]; then
      install_name_tool -id "@rpath/$(basename "${library}")" "${library}"
    fi
  done < <(find "${lib_dir}" -type f -name '*.dylib' -print)

  for image in "${macho_images[@]}"; do
    if [[ "${image}" == "${bin_dir}/qemu-system-aarch64" ]]; then
      bundled_rpath='@executable_path/../lib'
    elif [[ "${image}" == "${lib_dir}/qemu/"* ]]; then
      bundled_rpath='@loader_path/..'
    else
      bundled_rpath='@loader_path'
    fi

    # Remove build-machine search paths, then give dyld a path to the copied
    # libraries relative to this image's location in the app bundle.
    existing_rpaths="$(read_macho_rpaths "${image}")"
    while IFS= read -r existing_rpath; do
      [[ -n "${existing_rpath}" ]] || continue
      case "${existing_rpath}" in
        @*|/usr/lib/*|/System/Library/*) continue ;;
        /*) install_name_tool -delete_rpath "${existing_rpath}" "${image}" ;;
      esac
    done <<< "${existing_rpaths}"
    if ! grep -Fqx "${bundled_rpath}" <<< "${existing_rpaths}"; then
      install_name_tool -add_rpath "${bundled_rpath}" "${image}"
    fi

    load_paths="$(read_macho_load_paths "${image}")"
    while IFS= read -r load_path; do
      [[ -n "${load_path}" ]] || continue
      case "${load_path}" in
        @*|/usr/lib/*|/System/Library/*) continue ;;
        /*)
          library_name="$(basename "${load_path}")"
          if [[ ! -f "${lib_dir}/${library_name}" ]]; then
            echo "QEMU dependency ${load_path} was not copied into ${lib_dir}" >&2
            exit 1
          fi
          install_name_tool -change "${load_path}" "@rpath/${library_name}" "${image}"
          ;;
        *)
          echo "QEMU has an unsupported non-relative dependency ${load_path} in ${image}" >&2
          exit 1
          ;;
      esac
    done <<< "${load_paths}"

    remaining_external_loads="$(read_macho_load_paths "${image}" | awk '
      $0 !~ /^@/ && $0 !~ /^\/usr\/lib\// && $0 !~ /^\/System\/Library\// { print }
    ')"
    if [[ -n "${remaining_external_loads}" ]]; then
      echo "QEMU image ${image} still has external load commands:" >&2
      printf '%s\n' "${remaining_external_loads}" >&2
      exit 1
    fi
  done

  # install_name_tool changes the Mach-O load commands and invalidates any
  # upstream code signature. Re-sign every bundled runtime image ad hoc so
  # macOS will execute QEMU and load its relocated libraries.
  codesign --force --sign - --timestamp=none "${bin_dir}/qemu-system-aarch64"
  while IFS= read -r image; do
    codesign --force --sign - --timestamp=none "${image}"
  done < <(find "${lib_dir}" -type f \( -name '*.dylib' -o -name '*.so' \) -print)

  codesign --verify --strict --verbose=2 "${bin_dir}/qemu-system-aarch64"
  while IFS= read -r image; do
    codesign --verify --strict --verbose=2 "${image}"
  done < <(find "${lib_dir}" -type f \( -name '*.dylib' -o -name '*.so' \) -print)
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
