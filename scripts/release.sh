#!/usr/bin/env bash
# Release helpers for the release workflow: the version named by a tag, and the
# draft release notes for the container image.
set -euo pipefail

readonly IMAGE=ghcr.io/phongguy/potluck

# Print X.Y.Z[-pre] for a tag vX.Y.Z[-pre]; any other tag is refused.
tag_version() {
  if [[ ! $1 =~ ^v([0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?)$ ]]; then
    echo "release.sh: '$1' is not a vX.Y.Z[-pre] tag" >&2
    return 1
  fi
  echo "${BASH_REMATCH[1]}"
}

# Print the user-facing feat and fix subjects in RANGE, oldest first, as
# "feat|fix<TAB>description"; scopes for tests, CI, releases, builds and
# dependencies are housekeeping and left out, alone or as a comma list.
change_subjects() {
  git log --no-merges --reverse --format=%s "$1" |
    sed -nE '/^(feat|fix)\(((test|tests|ci|release|build|deps) *, *)*(test|tests|ci|release|build|deps)\)!?:/d
      s/^(feat|fix)(\([^)]*\))?!?: *(.+)$/\1	\3/p'
}

# "N noun" with an s for any N but 1.
plural() {
  if (($1 == 1)); then echo "1 $2"; else echo "$1 $2s"; fi
}

# The one-line intro for COUNTS of features and fixes; a person rewords it in the draft.
notes_intro() {
  local feats=$1 fixes=$2 first=$3 parts=()
  if [[ $first == true ]]; then
    echo "The first release of Potluck."
    return 0
  fi
  ((feats == 0)) || parts+=("adds $(plural "$feats" feature)")
  ((fixes == 0)) || parts+=("fixes $(plural "$fixes" bug)")
  case ${#parts[@]} in
    0) echo "This release has maintenance changes only." ;;
    1) echo "This release ${parts[0]}." ;;
    *) echo "This release ${parts[0]} and ${parts[1]}." ;;
  esac
}

# GitHub's list of the pull requests merged into TAG in REPO since PREV (or since
# the start, when PREV is empty), without the heading and changelog link the
# notes add themselves. A failed or empty reply fails, so no draft loses its list.
generated_changes() {
  local tag=$1 repo=$2 prev=$3 body
  local args=(api "repos/$repo/releases/generate-notes" -f "tag_name=$tag")
  [[ -z $prev ]] || args+=(-f "previous_tag_name=$prev")
  if ! body=$("${GH:-gh}" "${args[@]}" --jq .body) || [[ -z $body ]]; then
    echo "release.sh: gh api generate-notes gave no release notes for $tag in $repo" >&2
    return 1
  fi
  printf '%s\n' "$body" |
    sed -e "/^## What's Changed\$/d" -e '/^\*\*Full Changelog\*\*/d' |
    awk 'NF { for (; blank > 0; blank--) print ""; print; seen = 1; next } seen { blank++ }'
}

# Write OUT, the draft notes for TAG in REPO (owner/name) whose image index is
# DIGEST: the intro, What's Changed since the previous vX.Y.Z tag, the image to
# pull, and the install and update notes. Runs in the repository, which needs
# full history and tags. The attestation check is named only when ATTESTED=true.
release_notes() {
  local tag=$1 repo=$2 digest=$3 out=$4
  local prev="" range=$tag first=true subjects changes feats fixes
  tag_version "$tag" >/dev/null || return 1
  # A pre-release tag (v1.0.0-rc.1) is never the base of a release.
  if prev=$(git describe --tags --abbrev=0 --match 'v*' --exclude 'v*-*' "$tag^" 2>/dev/null); then
    range=$prev..$tag
    first=false
  fi
  subjects=$(change_subjects "$range") || return 1
  changes=$(generated_changes "$tag" "$repo" "$prev") || return 1
  feats=$(printf '%s\n' "$subjects" | grep -c '^feat	' || true)
  fixes=$(printf '%s\n' "$subjects" | grep -c '^fix	' || true)
  {
    notes_intro "$feats" "$fixes" "$first"
    echo
    echo "## What's Changed"
    echo
    if [[ -n $changes ]]; then
      printf '%s\n' "$changes"
    else
      echo "- Maintenance only."
    fi
    if [[ $first == false ]]; then
      echo
      echo "**Full Changelog**: https://github.com/$repo/compare/$prev...$tag"
    fi
    cat <<NOTES

## Downloads

| Platform | Image |
| --- | --- |
| Linux x86_64 and arm64 | \`$IMAGE:$tag\` |

Pinned: \`$IMAGE@$digest\`

NOTES
    # latest moves only on a final vX.Y.Z, as the release workflow's merge job tags it.
    local pull="Pull with \`podman pull $IMAGE:$tag\` or \`docker pull $IMAGE:$tag\`"
    if [[ $tag == *-* ]]; then
      echo "$pull."
    else
      echo "$pull; the image is also tagged \`latest\`."
    fi
    if [[ ${ATTESTED:-} == true ]]; then
      echo "Check that this project's release workflow built it with \`gh attestation verify oci://$IMAGE:$tag --repo $repo\`."
    fi
    cat <<'NOTES'

## Before you install

- Put a reverse proxy with TLS in front of the server, and set `POTLUCK_PUBLIC_URL` to the public address it serves.
- The `/data` volume holds all of the server's state; keep it on persistent storage.
- The README's run section shows how to start the container with compose or Quadlet.

## Updates

To update, pull the new tag and restart the container; Quadlet users can enable `podman auto-update` to do this for them. Back up the `/data` volume first.
NOTES
  } >"$out"
}

usage() {
  echo "usage: release.sh version TAG | notes TAG REPO DIGEST OUT" >&2
  exit 2
}

main() {
  case ${1:-} in
    version)
      (($# == 2)) || usage
      tag_version "$2" || exit 1
      ;;
    notes)
      (($# == 5)) || usage
      release_notes "$2" "$3" "$4" "$5"
      ;;
    *) usage ;;
  esac
}

main "$@"
