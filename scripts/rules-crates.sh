#!/usr/bin/env bash
# Drives the CI `rules` job from Cargo.lock: the rules crates pinned there, and the shape of the
# potluck-kind entry they build against.
# Usage: scripts/rules-crates.sh <list|check-lock|test> <Cargo.lock>
#   list        prints "<name> <git url> <tag> <commit>" per pinned *-rules package
#   check-lock  exits 1 unless exactly one potluck-kind package exists and it has no source
#   test        check-lock, then clones each listed crate at its tag and runs its tests
set -euo pipefail
usage="usage: rules-crates.sh <list|check-lock|test> <Cargo.lock>"
mode=${1:?$usage}
lock=${2:?$usage}

# Each [[package]] block is keyed on its unindented name and source lines only, so the indented
# entries of a dependencies list never count as packages. Prints "<name>\t<source>" per package.
packages() {
  awk '
    function flush() { if (inpkg && name != "") printf "%s\t%s\n", name, source }
    function value(line) { sub(/^[a-z]+ = "/, "", line); sub(/"$/, "", line); return line }
    /^\[/ { flush(); inpkg = ($0 == "[[package]]"); name = ""; source = ""; next }
    inpkg && /^name = "/ { name = value($0) }
    inpkg && /^source = "/ { source = value($0) }
    END { flush() }
  ' "$lock"
}

# Only a tag pin on a PhongGuy GitHub repo is accepted: any other pin of a rules crate would
# otherwise leave the job silently testing nothing.
list() {
  packages | awk -F '\t' '
    $1 !~ /-rules$/ { next }
    {
      src = $2
      ok = (src ~ /^git\+https:\/\/github\.com\/PhongGuy\/[A-Za-z0-9._-]+\?tag=[A-Za-z0-9][A-Za-z0-9._-]*#[0-9a-f]+$/)
      commit = src; sub(/^[^#]*#/, "", commit)
      if (!ok || length(commit) != 40 || $1 !~ /^[A-Za-z0-9_-]+$/) {
        printf "rules: %s is not pinned to a PhongGuy git tag (source: %s)\n", $1, (src == "" ? "none" : src) > "/dev/stderr"
        bad = 1
        next
      }
      url = src; sub(/^git\+/, "", url); sub(/\?.*$/, "", url)
      tag = src; sub(/^[^?]*\?tag=/, "", tag); sub(/#.*$/, "", tag)
      out = out sprintf("%s %s %s %s\n", $1, url, tag, commit)
    }
    END { if (bad) exit 1; printf "%s", out }
  '
}

check_lock() {
  packages | awk -F '\t' '
    $1 == "potluck-kind" { n++; if ($2 != "") src = $2 }
    END {
      if (n != 1) { printf "check-lock: expected one potluck-kind package, found %d\n", n > "/dev/stderr"; exit 1 }
      if (src != "") { printf "check-lock: potluck-kind must be the workspace path crate, not %s\n", src > "/dev/stderr"; exit 1 }
    }
  '
}

# Each crate is tested in its own repository at the pinned commit, against the potluck-kind its
# lock names; a tag that has moved since the pin fails the job.
run_tests() {
  check_lock
  local crates
  crates=$(list)
  if [ -z "$crates" ]; then
    echo "rules: no rules crate pinned"
    return 0
  fi
  # Global, since the EXIT trap runs after this function has returned.
  tmp=$(mktemp -d)
  trap 'rm -rf -- "$tmp"' EXIT
  # The list is read on fd 3, so a clone or test that reads stdin cannot swallow later crates.
  local name url tag commit head
  while read -r -u 3 name url tag commit; do
    git clone --quiet --depth 1 --branch "$tag" -- "$url" "$tmp/$name"
    head=$(git -C "$tmp/$name" rev-parse HEAD)
    if [ "$head" != "$commit" ]; then
      echo "rules: $name tag $tag is at $head, the lock pins $commit" >&2
      exit 1
    fi
    (cd "$tmp/$name" && cargo test -p "$name" --locked)
  done 3<<< "$crates"
}

case $mode in
  list) list ;;
  check-lock) check_lock ;;
  test) run_tests ;;
  *) echo "$usage" >&2; exit 2 ;;
esac
