#!/usr/bin/env bash
set -euo pipefail

if [[ $# -eq 0 ]]; then
  echo "usage: run-offline-network-namespace.sh COMMAND [ARG ...]" >&2
  exit 2
fi

runner_uid="$(id -u)"
runner_gid="$(id -g)"
if [[ "$runner_uid" == 0 ]]; then
  echo "run this helper as the unprivileged Actions runner user" >&2
  exit 1
fi

exec sudo -n unshare --net -- bash -euo pipefail -c '
  runner_uid="$1"
  runner_gid="$2"
  shift 2

  ip link set lo up
  unexpected_interfaces="$(ip -o link show | awk -F ": " '\''$2 !~ /^lo(@.*)?$/ { print $2 }'\'')"
  if [[ -n "$unexpected_interfaces" ]]; then
    printf "Unexpected interfaces in offline namespace:\n%s\n" "$unexpected_interfaces" >&2
    exit 1
  fi
  default_route="$(ip route show default)"
  if [[ -n "$default_route" ]]; then
    printf "Unexpected default route in offline namespace:\n%s\n" "$default_route" >&2
    exit 1
  fi

  printf "[offline-netns] interfaces:\n"
  ip -brief link show
  printf "[offline-netns] no default route; running as uid=%s gid=%s\n" "$runner_uid" "$runner_gid"
  exec setpriv --reuid "$runner_uid" --regid "$runner_gid" --init-groups "$@"
' rootglass-offline "$runner_uid" "$runner_gid" "$@"
