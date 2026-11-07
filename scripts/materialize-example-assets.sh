#!/usr/bin/env bash
set -euo pipefail

# Materialize local, generated assets for examples/hello-world.glass.
#
# This intentionally does not commit large binary assets. It downloads canned
# Alpine Linux aarch64 material and produces the files that the example glass
# expects locally:
#
#   examples/hello-world.glass/rootfs.img
#   examples/hello-world.glass/state.img
#   runtime-assets/alpine-aarch64/vmlinuz-virt
#   runtime-assets/alpine-aarch64/initramfs-virt
#
# Dependencies:
#   curl, tar, dd, mkfs.ext4, sudo, mountpoint, mount, umount,
#   cpio, gzip, squashfs-tools
#
# Linux is currently required for rootfs image creation because this script
# mounts an ext4 image locally. macOS/Windows users can run this inside an
# Ubuntu 24.04 VM/container with loop-device support.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GLASS_DIR="${ROOT_DIR}/examples/hello-world.glass"
ASSET_DIR="${ROOT_DIR}/runtime-assets/alpine-aarch64"
WORK_DIR="${ROOT_DIR}/.rootglass/build/hello-world-assets"
MOUNT_DIR="${WORK_DIR}/mnt-rootfs"

ALPINE_RELEASE="${ALPINE_RELEASE:-v3.23}"
ALPINE_VERSION="${ALPINE_VERSION:-3.23.8}"
ALPINE_BASE_URL="${ALPINE_BASE_URL:-https://dl-cdn.alpinelinux.org/alpine/${ALPINE_RELEASE}/releases/aarch64}"
ALPINE_MINIROOTFS="${ALPINE_MINIROOTFS:-alpine-minirootfs-${ALPINE_VERSION}-aarch64.tar.gz}"
ROOTFS_SIZE_MB="${ROOTFS_SIZE_MB:-256}"
STATE_SIZE_MB="${STATE_SIZE_MB:-64}"
ROOTFS_LABEL="rootglass-root"
STATE_LABEL="rootglass-state"
EXPECTED_MINIROOTFS="alpine-minirootfs-${ALPINE_VERSION}-aarch64.tar.gz"
PINNED_KERNEL="${ASSET_DIR}/vmlinuz-virt-${ALPINE_VERSION}"
PINNED_INITRAMFS="${ASSET_DIR}/initramfs-virt-${ALPINE_VERSION}"
PINNED_MODLOOP="${ASSET_DIR}/modloop-virt-${ALPINE_VERSION}"
INITRAMFS_WORK="${WORK_DIR}/initramfs"
MODLOOP_WORK="${WORK_DIR}/modloop"

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

require_cmd curl
require_cmd tar
require_cmd dd
require_cmd mkfs.ext4
require_cmd e2label
require_cmd sudo
require_cmd mountpoint
require_cmd mount
require_cmd umount
require_cmd cpio
require_cmd gzip
require_cmd unsquashfs

mkdir -p "${GLASS_DIR}" "${ASSET_DIR}" "${WORK_DIR}" "${MOUNT_DIR}"

if [[ "${ALPINE_MINIROOTFS}" != "${EXPECTED_MINIROOTFS}" ]]; then
  echo "ALPINE_MINIROOTFS must match the pinned Alpine version ${ALPINE_VERSION}" >&2
  exit 1
fi

MINIROOTFS_URL="${ALPINE_BASE_URL}/${ALPINE_MINIROOTFS}"
MINIROOTFS_SHA_URL="${MINIROOTFS_URL}.sha256"
NETBOOT_URL="${ALPINE_BASE_URL}/netboot-${ALPINE_VERSION}"
KERNEL_URL="${NETBOOT_URL}/vmlinuz-virt"
INITRAMFS_URL="${NETBOOT_URL}/initramfs-virt"
MODLOOP_URL="${NETBOOT_URL}/modloop-virt"

MINIROOTFS_TARBALL="${ASSET_DIR}/${ALPINE_MINIROOTFS}"
ROOTFS_IMG="${GLASS_DIR}/rootfs.img"
STATE_IMG="${GLASS_DIR}/state.img"
ROOTFS_VERSION_FILE="${ASSET_DIR}/rootfs-alpine-version"

cleanup_mount() {
  if mountpoint -q "${MOUNT_DIR}"; then
    echo "unmount: ${MOUNT_DIR}"
    sudo umount "${MOUNT_DIR}"
  fi
}
trap cleanup_mount EXIT

curl_if_missing() {
  local url="$1"
  local output="$2"
  if [[ -f "${output}" ]]; then
    echo "exists: ${output}"
    return 0
  fi
  echo "download: ${url}"
  curl --fail --location --show-error --output "${output}" "${url}"
}

create_ext4_image() {
  local path="$1"
  local size_mb="$2"
  local description="$3"
  local label="$4"

  if [[ -f "${path}" ]]; then
    echo "exists: ${path}"
    return 0
  fi

  echo "create ${description} image: ${path}"
  dd if=/dev/zero of="${path}" bs=1M count="${size_mb}" status=progress
  mkfs.ext4 -F -L "${label}" "${path}"
}

curl_if_missing "${MINIROOTFS_URL}" "${MINIROOTFS_TARBALL}"
curl_if_missing "${MINIROOTFS_SHA_URL}" "${MINIROOTFS_TARBALL}.sha256"
curl_if_missing "${KERNEL_URL}" "${PINNED_KERNEL}"
curl_if_missing "${INITRAMFS_URL}" "${PINNED_INITRAMFS}"
curl_if_missing "${MODLOOP_URL}" "${PINNED_MODLOOP}"
cp "${PINNED_KERNEL}" "${ASSET_DIR}/vmlinuz-virt"

# Alpine's netboot initramfs omits most filesystem modules. Its normal disk
# boot obtains those from modloop after mounting the root filesystem, which is
# too late for this guest. Embed the matching module tree into a rebuilt initrd
# so ext4 is available during the initial root mount.
rm -rf "${INITRAMFS_WORK}" "${MODLOOP_WORK}"
mkdir -p "${INITRAMFS_WORK}" "${MODLOOP_WORK}"
gzip -dc "${PINNED_INITRAMFS}" \
  | (cd "${INITRAMFS_WORK}" && cpio -id --quiet --no-absolute-filenames)
unsquashfs -quiet -d "${MODLOOP_WORK}" "${PINNED_MODLOOP}"
if [[ ! -d "${MODLOOP_WORK}/modules" ]]; then
  echo "Alpine modloop does not contain its expected modules directory" >&2
  exit 1
fi
mkdir -p "${INITRAMFS_WORK}/lib"
rm -rf "${INITRAMFS_WORK}/lib/modules"
cp -a "${MODLOOP_WORK}/modules" "${INITRAMFS_WORK}/lib/modules"
(
  cd "${INITRAMFS_WORK}"
  find . -print0 | cpio --null -o --quiet -H newc | gzip -9n
) > "${ASSET_DIR}/initramfs-virt"

(
  cd "${ASSET_DIR}"
  sha256sum --check "${ALPINE_MINIROOTFS}.sha256"
)

if [[ -f "${ROOTFS_IMG}" ]] && {
  [[ ! -f "${ROOTFS_VERSION_FILE}" ]] ||
    ! grep -Fqx "${ALPINE_VERSION}" "${ROOTFS_VERSION_FILE}"
}; then
  echo "rootfs image was not generated from Alpine ${ALPINE_VERSION}: ${ROOTFS_IMG}" >&2
  echo "remove the generated example assets with 'just clean-assets', then retry" >&2
  exit 1
fi

if [[ ! -f "${ROOTFS_IMG}" ]]; then
  create_ext4_image "${ROOTFS_IMG}" "${ROOTFS_SIZE_MB}" "rootfs" "${ROOTFS_LABEL}"
  sudo mount -o loop "${ROOTFS_IMG}" "${MOUNT_DIR}"
  sudo tar -xzf "${MINIROOTFS_TARBALL}" -C "${MOUNT_DIR}"
else
  echo "exists: ${ROOTFS_IMG}"
  sudo mount -o loop "${ROOTFS_IMG}" "${MOUNT_DIR}"
fi

# Make the minirootfs slightly friendlier for an interactive serial boot.
if ! sudo grep -q '^ttyAMA0::respawn:' "${MOUNT_DIR}/etc/inittab"; then
  echo 'ttyAMA0::respawn:/sbin/getty -L ttyAMA0 115200 vt100' \
    | sudo tee -a "${MOUNT_DIR}/etc/inittab" >/dev/null
fi
echo 'rootglass-hello' | sudo tee "${MOUNT_DIR}/etc/hostname" >/dev/null
if ! sudo grep -Fq '::sysinit:/bin/hostname rootglass-hello' "${MOUNT_DIR}/etc/inittab"; then
  echo '::sysinit:/bin/hostname rootglass-hello' \
    | sudo tee -a "${MOUNT_DIR}/etc/inittab" >/dev/null
fi

# This credential is only for the disposable, offline CI/developer fixture.
# Production bundles must use their own locked-down account policy.
sudo sed -i 's#^root:[^:]*:#root:$6$rootglassci$03D/aOvVxNSI1GfoL9z.d7c6a3pHi1LcBO.LHfVJI5IivzB4MpmYicGvCKJ8gGCfHVYxgpSAYOb9ideoLea/j.:#' "${MOUNT_DIR}/etc/shadow"
cleanup_mount
printf '%s\n' "${ALPINE_VERSION}" > "${ROOTFS_VERSION_FILE}"

if [[ "$(sudo e2label "${ROOTFS_IMG}")" != "${ROOTFS_LABEL}" ]]; then
  echo "rootfs image has an unexpected filesystem label" >&2
  exit 1
fi

create_ext4_image "${STATE_IMG}" "${STATE_SIZE_MB}" "writable state" "${STATE_LABEL}"
if [[ "$(sudo e2label "${STATE_IMG}")" != "${STATE_LABEL}" ]]; then
  echo "state image has an unexpected filesystem label" >&2
  exit 1
fi

cat <<EOF

Generated assets:
  ${ROOTFS_IMG}
  ${STATE_IMG}
  ${ASSET_DIR}/vmlinuz-virt
  ${ASSET_DIR}/initramfs-virt

Next:
  cargo run --locked -p rootglass-cli -- validate examples/hello-world.glass
  cargo run --locked -p rootglass-cli -- plan examples/hello-world.glass
  cargo run --locked -p rootglass-cli -- boot examples/hello-world.glass
EOF

