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
#   curl, tar, dd, mkfs.ext4, sudo, mountpoint, mount, umount
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
ALPINE_VERSION="${ALPINE_VERSION:-3.23.4}"
ALPINE_BASE_URL="${ALPINE_BASE_URL:-https://dl-cdn.alpinelinux.org/alpine/${ALPINE_RELEASE}/releases/aarch64}"
ALPINE_MINIROOTFS="${ALPINE_MINIROOTFS:-alpine-minirootfs-${ALPINE_VERSION}-aarch64.tar.gz}"
ROOTFS_SIZE_MB="${ROOTFS_SIZE_MB:-256}"
STATE_SIZE_MB="${STATE_SIZE_MB:-64}"
ROOTFS_LABEL="rootglass-root"
STATE_LABEL="rootglass-state"

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
require_cmd sudo
require_cmd mountpoint
require_cmd mount
require_cmd umount

mkdir -p "${GLASS_DIR}" "${ASSET_DIR}" "${WORK_DIR}" "${MOUNT_DIR}"

MINIROOTFS_URL="${ALPINE_BASE_URL}/${ALPINE_MINIROOTFS}"
MINIROOTFS_SHA_URL="${MINIROOTFS_URL}.sha256"
NETBOOT_URL="${ALPINE_BASE_URL}/netboot-${ALPINE_VERSION}"
KERNEL_URL="${NETBOOT_URL}/vmlinuz-virt"
INITRAMFS_URL="${NETBOOT_URL}/initramfs-virt"

MINIROOTFS_TARBALL="${ASSET_DIR}/${ALPINE_MINIROOTFS}"
ROOTFS_IMG="${GLASS_DIR}/rootfs.img"
STATE_IMG="${GLASS_DIR}/state.img"

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
curl_if_missing "${KERNEL_URL}" "${ASSET_DIR}/vmlinuz-virt"
curl_if_missing "${INITRAMFS_URL}" "${ASSET_DIR}/initramfs-virt"

(
  cd "${ASSET_DIR}"
  sha256sum --check "${ALPINE_MINIROOTFS}.sha256"
)

if [[ ! -f "${ROOTFS_IMG}" ]]; then
  create_ext4_image "${ROOTFS_IMG}" "${ROOTFS_SIZE_MB}" "rootfs" "${ROOTFS_LABEL}"
  sudo mount -o loop "${ROOTFS_IMG}" "${MOUNT_DIR}"
  sudo tar -xzf "${MINIROOTFS_TARBALL}" -C "${MOUNT_DIR}"

  # Make the minirootfs slightly friendlier for an interactive serial boot.
  if ! sudo grep -q '^ttyAMA0::respawn:' "${MOUNT_DIR}/etc/inittab"; then
    echo 'ttyAMA0::respawn:/sbin/getty -L ttyAMA0 115200 vt100' \
      | sudo tee -a "${MOUNT_DIR}/etc/inittab" >/dev/null
  fi
  echo 'rootglass-hello' | sudo tee "${MOUNT_DIR}/etc/hostname" >/dev/null

  cleanup_mount
else
  echo "exists: ${ROOTFS_IMG}"
fi

create_ext4_image "${STATE_IMG}" "${STATE_SIZE_MB}" "writable state" "${STATE_LABEL}"

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
