#!/usr/bin/env bash
# End-to-end check of deploy/compose/cluster.yml: the `compose` CI job
# (.github/workflows/compose.yml) runs it, and it runs the same locally:
#
#   cargo build --release -p graph-cli
#   deploy/compose/check.sh            # MG=path/to/memory-graph to override
#
# Steps: build and start the cluster (up -d --wait: every node passes
# `health --ready`); index testdata/corpus through node2 (a follower
# forwarding to the leader); query node3 and diff against an
# embedded run of the same indexing; `cluster members` on node1 shows three
# voters; stop node1: a write through node2 and a linearizable read through
# node3 still work; start node1 and wait until all three report the same
# applied index; `down -v`. Every wait is bounded. Run from the repository
# root.
#
# It never touches a cluster started from cluster.yml the documented way:
# it runs under its own Compose project name (CHECK_PROJECT, default
# memory-graph-check-<pid>), so `down -v` removes only its own containers
# and volumes, and publishes its own host ports (CHECK_PORT_BASE, default
# 17000: gRPC on 17001-17003, metrics on 19101-19103), not 7001-7003 /
# 9101-9103.
set -euo pipefail

MG=${MG:-target/release/memory-graph}
PY=${PY:-python3}
PROJECT=${CHECK_PROJECT:-memory-graph-check-$$}
BASE=${CHECK_PORT_BASE:-17000}
MBASE=${CHECK_METRICS_PORT_BASE:-19100}
P1=$((BASE + 1)) P2=$((BASE + 2)) P3=$((BASE + 3))
export MG_GRPC_PORT_1=$P1 MG_GRPC_PORT_2=$P2 MG_GRPC_PORT_3=$P3
export MG_METRICS_PORT_1=$((MBASE + 1)) MG_METRICS_PORT_2=$((MBASE + 2)) MG_METRICS_PORT_3=$((MBASE + 3))
COMPOSE=(docker compose -p "$PROJECT" -f deploy/compose/cluster.yml)
WAIT_SECS=${WAIT_SECS:-180}
WORK=$(mktemp -d)
QUERIES=(
  "describe"
  "describe --json"
  "search Subscribe --json"
  "search return --grain method --limit 50"
  "search string --grain file --json"
)

log() { printf '== %s\n' "$*" >&2; }

cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then
    log "failed; container logs follow"
    "${COMPOSE[@]}" ps -a || true
    "${COMPOSE[@]}" logs --no-color --tail 200 || true
  fi
  if [ "${KEEP:-0}" != 1 ]; then
    "${COMPOSE[@]}" down -v --remove-orphans || true
  fi
  if [ "${EMBEDDED_IN_IMAGE:-0}" = 1 ]; then
    docker volume rm -f "$PROJECT-embedded" >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
  exit "$status"
}
trap cleanup EXIT

# `elapsed=12ms` and `"elapsed_ms":12` differ between runs.
# A `--server` run's `--json` also says `"stale_possible"` (ADR 0004 D8),
# an embedded one does not; a linearizable read must say `false`, so only
# that exact field is dropped (a `true` still differs).
normalize() { sed -E 's/elapsed=[0-9]+/elapsed=N/g; s/"elapsed_ms":[0-9]+/"elapsed_ms":N/g; s/,"stale_possible":false//g; s/"stale_possible":false,//g'; }

# The embedded side of the comparison: this binary on a local file, or
# (EMBEDDED_IN_IMAGE=1, for a host whose platform differs from the
# containers', e.g. Windows) the cluster's own image on a scratch volume.
embedded() {
  if [ "${EMBEDDED_IN_IMAGE:-0}" = 1 ]; then
    MSYS_NO_PATHCONV=1 docker run --rm -v "$(pwd -W 2>/dev/null || pwd)/testdata:/src/testdata:ro" \
      -v "$PROJECT-embedded":/data -w /src "${MG_IMAGE:-memory-graph:local}" \
      --db /data/embedded.redb "$@"
  else
    "$MG" --db "$WORK/embedded.redb" "$@"
  fi
}

# wait_for <what> <command...>: retry until the command succeeds.
wait_for() {
  what=$1
  shift
  deadline=$((SECONDS + WAIT_SECS))
  until "$@" >/dev/null 2>&1; do
    if [ "$SECONDS" -ge "$deadline" ]; then
      log "timed out after ${WAIT_SECS}s waiting for $what"
      "$@" || true
      return 1
    fi
    sleep 1
  done
}

"$MG" --help >/dev/null || { log "no memory-graph binary at $MG"; exit 1; }

log "starting the cluster"
"${COMPOSE[@]}" up -d --wait --wait-timeout "$WAIT_SECS" ${COMPOSE_UP_ARGS:---build}

log "cluster members through node1: three voters"
voters() {
  "$MG" --server 127.0.0.1:$P1 cluster members --json |
    "$PY" -c 'import json,sys; m=json.load(sys.stdin)["members"]; print(sorted(x["node_id"] for x in m if x["role"]=="voter"))'
}
three_voters() { [ "$(voters)" = "[1, 2, 3]" ]; }
wait_for "three voters" three_voters

log "indexing testdata/corpus through node2 and into an embedded store"
for dir in testdata/corpus/*/; do
  repo=$(basename "$dir")
  "$MG" --server 127.0.0.1:$P2 index --org corpus --repo "$repo" --no-progress "$dir" >/dev/null
  embedded index --org corpus --repo "$repo" --no-progress "$dir" >/dev/null
done

log "node3 answers what the embedded store answers"
for q in "${QUERIES[@]}"; do
  # shellcheck disable=SC2086
  "$MG" --server 127.0.0.1:$P3 --read linearizable $q | normalize >"$WORK/remote.txt"
  # shellcheck disable=SC2086
  embedded $q | normalize >"$WORK/embedded.txt"
  diff -u "$WORK/embedded.txt" "$WORK/remote.txt" || { log "differs: $q"; exit 1; }
done

log "/metrics on every node"
for port in "$MG_METRICS_PORT_1" "$MG_METRICS_PORT_2" "$MG_METRICS_PORT_3"; do
  curl -fsS "http://127.0.0.1:$port/metrics" | grep -q '^mg_raft_applied_index ' ||
    { log "no mg_raft_applied_index on :$port"; exit 1; }
done

log "stopping node1"
"${COMPOSE[@]}" stop node1
printf 'pub fn written_while_node1_was_down() {}\n' >"$WORK/after.rs"
wait_for "a write through node2 with node1 down" \
  "$MG" --server 127.0.0.1:$P2 index-file --org corpus --repo after "$WORK/after.rs"
"$MG" --server 127.0.0.1:$P3 --read linearizable search written_while_node1_was_down |
  grep -q written_while_node1_was_down || { log "node3 does not see the write"; exit 1; }

log "starting node1 again; waiting until every node applied the same index"
"${COMPOSE[@]}" start node1
applied() {
  "$MG" --server "127.0.0.1:$1" cluster status --json |
    "$PY" -c 'import json,sys; print(json.load(sys.stdin)["applied_index"])'
}
synced() {
  a=$(applied $P1) && b=$(applied $P2) && c=$(applied $P3) &&
    [ "$a" = "$b" ] && [ "$b" = "$c" ] && [ "$a" -gt 0 ]
}
wait_for "the three applied indexes to agree" synced
"$MG" --server 127.0.0.1:$P1 search written_while_node1_was_down |
  grep -q written_while_node1_was_down || { log "node1 did not catch up"; exit 1; }
wait_for "node1 to be ready" "$MG" --server 127.0.0.1:$P1 health --ready

log "ok"
