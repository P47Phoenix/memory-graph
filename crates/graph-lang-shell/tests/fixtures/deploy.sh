#!/usr/bin/env bash
# Hand-written fixture: a small deploy script in common bash style.
set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export DEPLOY_ENV="${DEPLOY_ENV:-staging}" LOG_LEVEL=info
declare -r MAX_RETRIES=3
declare -a TARGETS=(web api worker)

log() {
  local level=$1; shift
  printf '[%s] %s\n' "$level" "$*" >&2
}

die() { log error "$@"; exit 1; }

function retry {
  local n=0
  until "$@"; do
    n=$((n + 1))
    if (( n >= MAX_RETRIES )); then
      die "gave up after $n tries: $*"
    fi
    sleep $((n * 2))
  done
}

function deploy_target() {
  local target=$1
  case "$target" in
    web|api)
      retry kubectl rollout restart "deploy/$target"
      ;;
    worker)
      retry kubectl scale "deploy/$target" --replicas=0
      ;;
    *) die "unknown target $target" ;;
  esac
  cat <<EOF
deployed() { not a function }
EOF
}

cleanup() (
  cd "$SCRIPT_DIR"
  rm -rf ./tmp/*
)

main() {
  trap cleanup EXIT
  on_error() { log error "failed at line $1"; }
  trap 'on_error $LINENO' ERR
  for t in "${TARGETS[@]}"; do
    deploy_target "$t"
  done
}

main "$@"
