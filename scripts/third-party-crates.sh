#!/usr/bin/env bash
# Print the Rust crates linked into the potluck release binary, one Markdown line per crate
# with its licence, or with --write replace the marked block in THIRD_PARTY_NOTICES.md.
# A crate whose licence falls outside the allowlist fails the run instead of being listed.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
notices=${NOTICES:-$root/THIRD_PARTY_NOTICES.md}
readonly begin='<!-- crates:begin -->' end='<!-- crates:end -->'
readonly allowed=' MIT Apache-2.0 ISC BSD-3-Clause Unicode-3.0 Zlib Unlicense BSL-1.0 0BSD MIT-0 LLVM-exception '

write=0
case ${1:-} in
  '') ;;
  --write) write=1 ;;
  *) echo "third-party-crates.sh: unknown argument '$1'" >&2; exit 2 ;;
esac

# Normal edges without proc-macros, across both image targets, is what the release build links.
tree=$(cd "$root" && "${CARGO:-cargo}" tree -p potluck -e normal,no-proc-macro \
  --target x86_64-unknown-linux-gnu --target aarch64-unknown-linux-gnu \
  --prefix none -f '{p}|{l}' --frozen)

lines=()
bad=0
while IFS= read -r line; do
  [[ -z $line ]] && continue
  line=${line% (\*)}
  pkg=${line%%|*} licence=${line#*|}
  name=${pkg%% *} version=${pkg#* v}
  [[ $name == potluck && $version == *' ('* ]] && continue
  # Every identifier in the expression must be allowed; operators and grouping carry no terms.
  ok=1
  [[ -z $licence ]] && ok=0
  read -ra tokens <<< "${licence//[()\/]/ }"
  for token in "${tokens[@]}"; do
    case $token in AND | OR | WITH) continue ;; esac
    [[ $allowed == *" $token "* ]] || ok=0
  done
  if (( ! ok )); then
    echo "third-party-crates.sh: $name $version has licence '$licence' outside the allowlist" >&2
    bad=1
  fi
  lines+=("- \`$name\` $version: $licence")
done <<< "$tree"
(( bad )) && exit 1

list=$(printf '%s\n' "${lines[@]}" | LC_ALL=C sort -u)

if (( ! write )); then
  printf '%s\n' "$list"
  exit 0
fi

if ! grep -qxF "$begin" "$notices" || ! grep -qxF "$end" "$notices"; then
  echo "third-party-crates.sh: $notices lacks the $begin and $end markers" >&2
  exit 1
fi
# Build the new file beside the old one and move it over, so a failure leaves the old file.
tmp=$(mktemp "$notices.XXXXXX")
trap 'rm -f "$tmp"' EXIT
LIST=$list awk -v begin="$begin" -v end="$end" '
  $0 == end { skip = 0 }
  !skip { print }
  $0 == begin { print ENVIRON["LIST"]; skip = 1 }
' "$notices" > "$tmp"
chmod --reference="$notices" "$tmp"
mv -f "$tmp" "$notices"
trap - EXIT
