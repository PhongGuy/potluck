#!/usr/bin/env bash
# Fails when a public item of the potluck library names an axum, tower, http or hyper type.
# Usage: scripts/check-public-api.sh <potluck.json>, the rustdoc JSON of the library, built by
#   RUSTC_BOOTSTRAP=1 cargo rustdoc -p potluck --lib -- -Z unstable-options --output-format json
set -euo pipefail
json=${1:?usage: check-public-api.sh <rustdoc JSON of the potluck library>}

# A shape this filter does not walk would pass silently, so the keys it reads are checked first.
shape='(.format_version | type == "number") and (.index | type == "object")
  and (.paths | type == "object") and (.external_crates | type == "object")
  and ([.index[] | select(.crate_id == 0)] | length > 0)'
if ! jq -e "$shape" "$json" > /dev/null; then
  echo "check-public-api: $json is not rustdoc JSON this check can read" >&2
  exit 2
fi

# Every public item of the crate, and every impl on one except blanket and auto-trait impls
# (those belong to the trait's crate), is searched for an id that resolves to a banned crate.
# Doc text and intra-doc links are not signatures, so they are left out.
filter='
(.external_crates | with_entries(.value |= .name)) as $crates
| .paths as $paths
| ["axum", "axum_core", "http", "http_body", "http_body_util", "hyper", "hyper_util",
   "tower", "tower_service", "tower_layer", "tower_http"] as $banned
| .index[]
| select(.crate_id == 0)
| select((.inner.impl.blanket_impl // null) == null and (.inner.impl.is_synthetic // false) == false)
| . as $item
| [ del(.links, .docs) | .. | objects | select(has("id") and (.id | type) == "number") | .id
    | ($paths[tostring] // empty) | select(.crate_id != 0)
    | select($crates[.crate_id | tostring] as $c | $banned | index($c)) | .path | join("::") ]
| unique | select(length > 0)
| "\($item.name // ("impl for " + ($item.inner.impl.for.resolved_path.path // "?")))"
  + " (\($item.span.filename // "?"):\($item.span.begin[0] // 0)): \(join(", "))"'
leaks=$(jq -r "$filter" "$json" | sort)
if [[ -n $leaks ]]; then
  echo "check-public-api: public items name a web-stack type:"
  printf '%s\n' "$leaks"
  exit 1
fi
echo "check-public-api: no axum, tower, http or hyper type in the public API"
