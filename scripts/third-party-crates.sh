#!/usr/bin/env bash
# Print the Rust crates linked into the potluck release binary, one Markdown line per crate
# with its licence and, beneath it, its copyright lines, or with --write replace the marked
# block in THIRD_PARTY_NOTICES.md.
# A crate whose licence falls outside the allowlist fails the run instead of being listed.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
notices=${NOTICES:-$root/THIRD_PARTY_NOTICES.md}
readonly begin='<!-- crates:begin -->' end='<!-- crates:end -->'
readonly allowed=' MIT Apache-2.0 ISC BSD-3-Clause Unicode-3.0 Zlib Unlicense BSL-1.0 0BSD MIT-0 LLVM-exception '

# An SPDX expression passes when it holds with each allowed identifier true and every other
# false: OR needs one alternative, AND and WITH need both sides, and `/` is the old spelling of
# OR. An empty or malformed expression fails, since the arithmetic below cannot parse it.
licence_allowed() {
  local expr=${1//\// OR } out='' token
  expr=${expr//(/ ( }
  expr=${expr//)/ ) }
  local -a tokens
  read -ra tokens <<< "$expr"
  (( ${#tokens[@]} )) || return 1
  for token in "${tokens[@]}"; do
    case $token in
      OR) out+=' || ' ;;
      AND | WITH) out+=' && ' ;;
      '(' | ')') out+=" $token " ;;
      *) if [[ $allowed == *" $token "* ]]; then out+=" 1 "; else out+=" 0 "; fi ;;
    esac
  done
  ( (( $out )) ) 2> /dev/null
}

write=0
case ${1:-} in
  '') ;;
  --write) write=1 ;;
  *) echo "third-party-crates.sh: unknown argument '$1'" >&2; exit 2 ;;
esac

# Normal edges without proc-macros, across both image targets, is what the release build links.
tree=$(cd "$root" && "${CARGO:-cargo}" tree -p potluck -e normal,no-proc-macro \
  --target x86_64-unknown-linux-gnu --target aarch64-unknown-linux-gnu \
  --prefix none -f '{p}|{l}' --color never --frozen)
entries=()
bad=0
while IFS= read -r line; do
  [[ -z $line ]] && continue
  line=${line% (\*)}
  pkg=${line%%|*} licence=${line#*|}
  name=${pkg%% *} version=${pkg#* v}
  # This repo's own crates are first-party; any other path crate is checked and listed.
  [[ $name == potluck && $version == *' ('* ]] && continue
  [[ $version == *" ($root/crates/"* ]] && continue
  if ! licence_allowed "$licence"; then
    echo "third-party-crates.sh: $name $version has licence '$licence' outside the allowlist" >&2
    bad=1
  fi
  entries+=("- \`$name\` $version: $licence"$'\t'"$name ${version%% *}")
done <<< "$tree"
(( bad )) && exit 1
if (( ! ${#entries[@]} )); then
  echo "third-party-crates.sh: cargo tree listed no third-party crates" >&2
  exit 1
fi

# Metadata locates each crate's source, authors and repository; filtered to the two image
# targets it needs only the sources tree already required, so it runs offline.
meta=$(cd "$root" && "${CARGO:-cargo}" metadata --format-version 1 --frozen \
  --filter-platform x86_64-unknown-linux-gnu --filter-platform aarch64-unknown-linux-gnu)
rows=$(jq -r '.packages[] | [.name + " " + .version, (.manifest_path | rtrimstr("/Cargo.toml")),
  (.authors | join(", ")), (.repository // "")] | join("\u001f")' <<< "$meta")
# A unit separator, unlike a tab, keeps an empty authors or repository field in its place.
declare -A dirs authors repos
while IFS=$'\x1f' read -r key dir who repo; do
  [[ -z $key ]] && continue
  dirs[$key]=$dir authors[$key]=$who repos[$key]=$repo
done <<< "$rows"

# The copyright lines of a crate's top-level licence files, without comment prefixes and
# without the Apache appendix template, which names no real holder.
licence_files() {
  find "$1" -maxdepth 1 -xtype f \( -iname 'LICEN[CS]E*' -o -iname 'COPYING*' \
    -o -iname 'COPYRIGHT*' -o -iname 'NOTICE*' \) -print0
}
copyright_lines() {
  local files found rc=0
  mapfile -d '' files < <(licence_files "$1")
  (( ${#files[@]} )) || return 0
  found=$(grep -hE '^[[:space:]]*(//|#|\*)?[[:space:]]*(Copyright|©)' -- "${files[@]}") || rc=$?
  (( rc <= 1 )) || return "$rc"
  { grep -vE '\[yyyy\]|\{yyyy\}|name of copyright owner' <<< "$found" || true; } |
    sed -E 's/^[[:space:]]*(\/\/|#|\*)?[[:space:]]*//; s/[[:space:]]+$//; /^$/d' | LC_ALL=C sort -u
}

# A statement wrapped so no line starts with Copyright: each paragraph above an MIT grant
# that names a copyright, joined into one line.
copyright_headers() {
  local files
  mapfile -d '' files < <(licence_files "$1")
  (( ${#files[@]} )) || return 0
  awk '
    function keep() { if (para ~ /Copyright/) held = held para "\n"; para = "" }
    FNR == 1 { para = ""; held = ""; done = 0 }
    done { next }
    /Permission is hereby granted/ { keep(); printf "%s", held; done = 1; next }
    /^[[:space:]]*$/ { keep(); next }
    {
      line = $0; gsub(/[[:space:]]+/, " ", line); sub(/^ /, "", line); sub(/ $/, "", line)
      para = para == "" ? line : para " " line
    }
  ' "${files[@]}" | LC_ALL=C sort -u
}

list=''
while IFS=$'\t' read -r header key; do
  if [[ ! -v dirs[$key] ]]; then
    echo "third-party-crates.sh: cargo metadata lacks $key" >&2
    exit 1
  fi
  list+=$header$'\n'
  notes=$(copyright_lines "${dirs[$key]}")
  [[ -n $notes ]] || notes=$(copyright_headers "${dirs[$key]}")
  if [[ -n $notes ]]; then
    list+=$(sed 's/^/  - /' <<< "$notes")$'\n'
  elif [[ -n ${authors[$key]} ]]; then
    list+="  - Authors: ${authors[$key]}"$'\n'
  elif [[ -n ${repos[$key]} ]]; then
    list+="  - No copyright line or authors published; see ${repos[$key]}"$'\n'
  else
    list+="  - No copyright line, authors or repository published"$'\n'
  fi
done < <(printf '%s\n' "${entries[@]}" | LC_ALL=C sort -u)
list=${list%$'\n'}

if (( ! write )); then
  printf '%s\n' "$list"
  exit 0
fi

# Exactly one begin marker must precede exactly one end marker, or the block is ambiguous.
begins=$(grep -nxF "$begin" "$notices" | cut -d: -f1 || true)
ends=$(grep -nxF "$end" "$notices" | cut -d: -f1 || true)
if [[ ! $begins =~ ^[0-9]+$ || ! $ends =~ ^[0-9]+$ ]] || (( begins >= ends )); then
  echo "third-party-crates.sh: $notices needs one $begin line before one $end line" >&2
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
