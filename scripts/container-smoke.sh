#!/usr/bin/env bash
# Builds the Potluck image and runs the spec's smoke test on 127.0.0.1: health over TCP,
# `potluck health-check` inside the container, the non-root uid, git present, the HEALTHCHECK
# stored in the image, tini as PID 1 reaping orphans, an online backup restored into a fresh
# volume and served by a second container with the same refs, and a graceful stop on SIGTERM.
# Usage: scripts/container-smoke.sh [build|smoke|all] with ENGINE=podman|docker
# (auto-detected) and IMAGE (required for smoke; for build, the tag to build, which is kept).
# Only `all` without IMAGE removes the image it built.
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
# The image the EXIT trap removes: set only when `all` builds under a throwaway tag of its own.
built=""
name=potluck-smoke-$stamp
volume=$name-data
restored=$name-restore
restore_volume=$restored-data
tmp=""
cleanup() {
  run rm -f "$name" > /dev/null 2>&1 || true
  run rm -f "$restored" > /dev/null 2>&1 || true
  run volume rm -f "$volume" > /dev/null 2>&1 || true
  run volume rm -f "$restore_volume" > /dev/null 2>&1 || true
  [[ -z $tmp ]] || rm -rf "$tmp"
  [[ -z $built ]] || run rmi -f "$built" > /dev/null 2>&1 || true
}
trap cleanup EXIT

build() {
  local format=()
  # Podman's default OCI format drops HEALTHCHECK; docker has no such flag.
  [[ $engine != podman ]] || format=(--format docker)
  local tag=${IMAGE:-localhost/potluck:smoke-$stamp}
  [[ $mode != all || -n ${IMAGE:-} ]] || built=$tag
  # No --pull flag: both engines pull a missing base by default, and docker's --pull is a bare boolean.
  # A release build of the crate and its dependencies takes minutes; the 300 s step budget is for runtime calls.
  timeout 1800 "$engine" build "${format[@]}" -f "$root/Containerfile" \
    --build-arg VERSION=smoke --build-arg REVISION=local -t "$tag" "$root"
  IMAGE=$tag
}

fail() { echo "smoke: $*" >&2; exit 1; }

# Starts a container hardened as compose.yaml and the Quadlet unit run it, then prints its host
# port once health answers.
start() {
  local container=$1 data=$2
  run run -d --rm --pull=never --name "$container" -v "$data:/data" -p 127.0.0.1::8080 \
    --read-only --tmpfs /tmp --cap-drop ALL --security-opt no-new-privileges \
    -e POTLUCK_PUBLIC_URL=http://127.0.0.1:8080 "$IMAGE" > /dev/null
  local addr port
  addr=$(run port "$container" 8080/tcp | head -n1 | sed 's/.*-> *//')
  port=${addr##*:}
  [[ $port =~ ^[0-9]+$ ]] || fail "no host port in '$addr'"

  local deadline=$((SECONDS + 30)) body=""
  until body=$(curl -fsS "http://127.0.0.1:$port/api/v1/health" 2> /dev/null); do
    ((SECONDS < deadline)) || { run logs "$container" >&2 || true; fail "health did not answer in 30 s"; }
    sleep 0.5
  done
  [[ $body == *'"status":"ok"'* ]] || fail "unexpected health body: $body"
  echo "$port"
}

# git with the token as an extra header from the environment, so it never reaches argv or a
# config file, and with the host's own git config ignored.
git_as() {
  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=http.extraHeader \
    GIT_CONFIG_VALUE_0="Authorization: Bearer $token" git "$@"
}

# Seeds a workspace, takes an online backup, restores it into a fresh volume with the image's
# own tar and checks a second container serves the same refs and clones the same HEAD.
backup_round_trip() {
  local port=$1 password token url original
  command -v git > /dev/null 2>&1 || fail "git is not installed on the host"
  tmp=$(mktemp -d)
  run exec "$name" potluck admin team create smoke > /dev/null || fail "team create"
  run exec "$name" potluck admin workspace create smoke ws --kind apinata > /dev/null ||
    fail "workspace create"
  password=$(od -An -tx1 -N24 /dev/urandom | tr -d ' \n')
  printf '%s' "$password" |
    run exec -i "$name" potluck admin user create smoke --server-admin --password-stdin \
      > /dev/null || fail "user create"
  # The body travels on stdin and the token stays in a variable, so neither reaches argv.
  local session
  session=$(printf '{"username":"smoke","password":"%s","token_name":"smoke"}' "$password" |
    curl -fsS -H 'Content-Type: application/json' --data-binary @- \
      "http://127.0.0.1:$port/api/v1/session") || fail "sign-in failed"
  token=$(sed -n 's/.*"token":"\([^"]*\)".*/\1/p' <<< "$session")
  [[ -n $token ]] || fail "no token in the session response"

  url=/git/smoke/ws.git
  original=$(git_as ls-remote "http://127.0.0.1:$port$url") || fail "ls-remote of the original"
  [[ -n $original ]] || fail "the seeded workspace has no refs"
  run exec "$name" potluck admin backup > "$tmp/potluck-data.tar" || fail "admin backup"

  run run --rm -i --pull=never -v "$restore_volume:/data" --entrypoint tar "$IMAGE" \
    -C /data -xf - < "$tmp/potluck-data.tar" || fail "restoring the backup"
  local restored_port copy want
  restored_port=$(start "$restored" "$restore_volume")
  copy=$(git_as ls-remote "http://127.0.0.1:$restored_port$url") || fail "ls-remote of the restore"
  [[ $copy == "$original" ]] || fail "restored refs differ: $copy, expected $original"
  git_as clone -q "http://127.0.0.1:$restored_port$url" "$tmp/clone" || fail "clone of the restore"
  want=$(awk '$2 == "HEAD" { print $1 }' <<< "$original")
  [[ -n $want && $(git -C "$tmp/clone" rev-parse HEAD) == "$want" ]] ||
    fail "the clone's HEAD is not the original's $want"
  run rm -f "$restored" > /dev/null
}

smoke() {
  [[ -n ${IMAGE:-} ]] || fail "IMAGE is not set"
  local stored
  stored=$(run image inspect --format '{{.Config.Healthcheck.Test}}' "$IMAGE")
  [[ $stored == *health-check* ]] || fail "HEALTHCHECK missing from the image: $stored"

  local port
  port=$(start "$name" "$volume")

  [[ $(run exec "$name" potluck health-check) == ok ]] || fail "health-check inside the container"
  [[ $(run exec "$name" id -u) == 65532 ]] || fail "not running as uid 65532"
  run exec "$name" git --version > /dev/null || fail "git missing"
  if [[ $engine == podman ]]; then
    run healthcheck run "$name" || fail "podman healthcheck run"
  fi
  run exec "$name" test -S /data/admin.sock || fail "admin.sock not under /data"
  # tini is PID 1: a backgrounded child of an exec'd shell is re-parented to it and must not
  # stay a zombie once it exits.
  [[ $(run exec "$name" cat /proc/1/comm) == tini ]] || fail "PID 1 is not tini"
  run exec "$name" sh -c 'sleep 0.2 & exit 0'
  sleep 1
  local zombies
  zombies=$(run exec "$name" sh -c 'n=0; for f in /proc/[0-9]*/stat; do [ "$(cut -d" " -f3 "$f" 2> /dev/null)" = Z ] && n=$((n + 1)); done; echo $n')
  [[ $zombies == 0 ]] || fail "$zombies zombie processes in the container"
  backup_round_trip "$port"
  # stop sends SIGTERM to PID 1; tini forwards it, so potluck exits well before the 20 s kill.
  local stopped=$SECONDS
  run stop -t 20 "$name" > /dev/null
  ((SECONDS - stopped < 15)) || fail "stop took $((SECONDS - stopped)) s: SIGTERM did not reach potluck"
  echo "smoke: ok ($engine, $IMAGE on 127.0.0.1:$port)"
}

case $mode in
  build) build; echo "built $IMAGE" ;;
  smoke) smoke ;;
  all) build; smoke ;;
  *) echo "usage: $0 [build|smoke|all]" >&2; exit 2 ;;
esac
