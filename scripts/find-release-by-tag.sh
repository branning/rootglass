#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 || -z "$1" ]]; then
  echo "usage: $0 <release-tag>" >&2
  exit 2
fi

: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
: "${GH_TOKEN:?GH_TOKEN is required}"

release_tag="$1"
matches="$(RELEASE_TAG="$release_tag" gh api --paginate \
  "repos/$GITHUB_REPOSITORY/releases?per_page=100" \
  --jq '.[] | select(.tag_name == env.RELEASE_TAG) | [.id, .draft, (.immutable // false), .prerelease] | @tsv')"

if [[ -z "$matches" ]]; then
  echo "Release $release_tag was not found" >&2
  exit 1
fi

mapfile -t matching_releases <<<"$matches"
if [[ "${#matching_releases[@]}" -ne 1 ]]; then
  echo "Expected one release for tag $release_tag, found ${#matching_releases[@]}" >&2
  exit 1
fi

printf '%s\n' "${matching_releases[0]}"
