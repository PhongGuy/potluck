#!/usr/bin/env bash
# Builds the Potluck image and runs the spec's smoke test on 127.0.0.1: health over TCP,
# `potluck health-check` inside the container, the non-root uid, git present, and the
# HEALTHCHECK stored in the image. Usage: scripts/container-smoke.sh [build|smoke|all]
# with ENGINE=podman|docker (auto-detected) and IMAGE (required for smoke).
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
mode=${1:-all}
engine=${ENGINE:-}
if [[ -z $engine ]]; then
  for candidate in podman docker; do
    if command -v "$candidate" > /dev/null 2>&1; then engine=$candidate; break; fi
  done
fi
if [[ -z $engine ]]; then
  echo "skipped: neither podman nor docker is installed"
  exit 0
fi
run() { timeout 300 "$engine" "$@"; }

stamp=$$-$RANDOM
built=""
name=potluck-smoke-$stamp
volume=$name-data
cleanup() {
  run rm -f "$name" > /dev/null 2>&1 || true
  run volume rm -f "$volume" > /dev/null 2>&1 || true
  [[ -z $built ]] || run rmi -f "$built" > /dev/null 2>&1 || true
}
trap cleanup EXIT

build() {
  local format=()
  # Podman's default OCI format drops HEALTHCHECK; docker has no such flag.
  [[ $engine != podman ]] || format=(--format docker)
  built=localhost/potluck:smoke-$stamp
  # No --pull flag: both engines pull a missing base by default, and docker's --pull is a bare boolean.
  # A release build of the crate and its dependencies takes minutes; the 300 s step budget is for runtime calls.
  timeout 1800 "$engine" build "${format[@]}" -f "$root/Containerfile" \
    --build-arg VERSION=smoke --build-arg REVISION=local -t "$built" "$root"
  IMAGE=$built
}

fail() { echo "smoke: $*" >&2; exit 1; }

smoke() {
  [[ -n ${IMAGE:-} ]] || fail "IMAGE is not set"
  local stored
  stored=$(run image inspect --format '{{.Config.Healthcheck.Test}}' "$IMAGE")
  [[ $stored == *health-check* ]] || fail "HEALTHCHECK missing from the image: $stored"

  run run -d --rm --pull=never --name "$name" -v "$volume:/data" -p 127.0.0.1::8080 \
    -e POTLUCK_PUBLIC_URL=http://127.0.0.1:8080 "$IMAGE" > /dev/null
  local addr port
  addr=$(run port "$name" 8080/tcp | head -n1 | sed 's/.*-> *//')
  port=${addr##*:}
  [[ $port =~ ^[0-9]+$ ]] || fail "no host port in '$addr'"

  local deadline=$((SECONDS + 30)) body=""
  until body=$(curl -fsS "http://127.0.0.1:$port/api/v1/health" 2> /dev/null); do
    ((SECONDS < deadline)) || { run logs "$name" >&2 || true; fail "health did not answer in 30 s"; }
    sleep 0.5
  done
  [[ $body == *'"status":"ok"'* ]] || fail "unexpected health body: $body"

  [[ $(run exec "$name" potluck health-check) == ok ]] || fail "health-check inside the container"
  [[ $(run exec "$name" id -u) == 65532 ]] || fail "not running as uid 65532"
  run exec "$name" git --version > /dev/null || fail "git missing"
  if [[ $engine == podman ]]; then
    run healthcheck run "$name" || fail "podman healthcheck run"
  fi
  run exec "$name" test -S /data/admin.sock || fail "admin.sock not under /data"
  echo "smoke: ok ($engine, $IMAGE on 127.0.0.1:$port)"
}

case $mode in
  build) build; echo "built $IMAGE" ;;
  smoke) smoke ;;
  all) build; smoke ;;
  *) echo "usage: $0 [build|smoke|all]" >&2; exit 2 ;;
esac
