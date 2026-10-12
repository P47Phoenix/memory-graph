#!/usr/bin/env bash
# End-to-end check of the `otel` profile of deploy/compose/cluster.yml
# (ADR 0009, epic story 54): the Compose CI job runs it after check.sh, on
# the same image (.github/workflows/compose.yml), and it runs the same locally:
#
#   cargo build --release -p graph-cli
#   deploy/compose/check-otel.sh       # MG=path/to/memory-graph to override
#
# Steps: start the cluster with the `otel` profile and
# MG_OTLP_ENDPOINT=http://otel-collector:4317 (up -d --wait); index a repo
# of testdata/corpus through node2 (forwarded to the leader) and search on
# every node, repeating until the collector's debug log shows traces,
# metrics and logs from all three nodes (service.instance.id 1, 2 and 3) and
# Jaeger lists the memory-graph service; `down -v`. Every wait is bounded
# (WAIT_SECS); nothing sleeps for a fixed time to let telemetry arrive. Run
# from the repository root.
#
# Like check.sh it runs under its own Compose project name (CHECK_PROJECT,
# default memory-graph-otel-check-<pid>) and its own host ports
# (CHECK_PORT_BASE, default 27000: gRPC on 27001-27003, metrics on
# 29101-29103, Jaeger's UI on 26686), so it never touches another cluster.
set -euo pipefail

MG=${MG:-target/release/memory-graph}
PY=${PY:-python3}
PROJECT=${CHECK_PROJECT:-memory-graph-otel-check-$$}
BASE=${CHECK_PORT_BASE:-27000}
MBASE=${CHECK_METRICS_PORT_BASE:-29100}
P1=$((BASE + 1)) P2=$((BASE + 2)) P3=$((BASE + 3))
export MG_GRPC_PORT_1=$P1 MG_GRPC_PORT_2=$P2 MG_GRPC_PORT_3=$P3
export MG_METRICS_PORT_1=$((MBASE + 1)) MG_METRICS_PORT_2=$((MBASE + 2)) MG_METRICS_PORT_3=$((MBASE + 3))
export MG_JAEGER_UI_PORT=${CHECK_JAEGER_PORT:-26686}
export MG_OTLP_ENDPOINT=http://otel-collector:4317
export MG_OTLP_METRICS_INTERVAL=5s
COMPOSE=(docker compose -p "$PROJECT" -f deploy/compose/cluster.yml --profile otel)
WAIT_SECS=${WAIT_SECS:-180}
WORK=$(mktemp -d)

log() { printf '== %s\n' "$*" >&2; }

cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then
    log "failed; container logs follow"
    "${COMPOSE[@]}" ps -a || true
    "${COMPOSE[@]}" logs --no-color --tail 300 || true
  fi
  if [ "${KEEP:-0}" != 1 ]; then
    "${COMPOSE[@]}" down -v --remove-orphans || true
  fi
  rm -rf "$WORK"
  exit "$status"
}
trap cleanup EXIT

"$MG" --help >/dev/null || { log "no memory-graph binary at $MG"; exit 1; }

log "starting the cluster with the otel profile"
"${COMPOSE[@]}" up -d --wait --wait-timeout "$WAIT_SECS" ${COMPOSE_UP_ARGS:---build}

# Which nodes (service.instance.id) the collector's debug log shows, per
# signal. The debug exporter (verbosity detailed) prints a
# `ResourceSpans #n` / `ResourceMetrics #n` / `ResourceLog #n` header (the
# first one of an export on the exporter's own `info` log line), then
# that resource's attributes, one `-> key: Type(value)` line each.
seen() {
  "${COMPOSE[@]}" logs --no-color --no-log-prefix otel-collector 2>/dev/null >"$WORK/collector.log"
  "$PY" - "$WORK/collector.log" <<'EOF'
import re, sys
signal, seen = None, {"traces": set(), "metrics": set(), "logs": set()}
# A header starts a line, or follows the zap prefix of the exporter's own
# log line (`<time>\tinfo\tResourceLog #0`), so it is searched for. Any
# other zap line (a new export) ends the current resource, so an id is only
# ever credited to the signal of its own enclosing header.
head = re.compile(r"\b(ResourceSpans|ResourceMetrics|ResourceLog) #\d+")
zap = re.compile(r"\t(debug|info|warn|error)\t")
names = {"ResourceSpans": "traces", "ResourceMetrics": "metrics", "ResourceLog": "logs"}
for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    h = head.search(line)
    if h:
        signal = names[h.group(1)]
    elif zap.search(line):
        signal = None
    m = re.search(r"-> service\.instance\.id: \w+\((\w+)\)", line)
    if m and signal:
        seen[signal].add(m.group(1))
print(" ".join(f"{k}={','.join(sorted(v)) or '-'}" for k, v in seen.items()))
sys.exit(0 if all(v >= {"1", "2", "3"} for v in seen.values()) else 1)
EOF
}

# Some traffic on every node: a write through node2 (a follower forwards it
# to the leader), a search on each node.
traffic() {
  "$MG" --server 127.0.0.1:$P2 index --org corpus --repo "$REPO" --no-progress "$REPO_DIR" >/dev/null
  for p in $P1 $P2 $P3; do
    "$MG" --server 127.0.0.1:$p search Subscribe --limit 5 >/dev/null
  done
}

REPO_DIR=$(ls -d testdata/corpus/*/ | head -n 1)
REPO=$(basename "$REPO_DIR")

log "waiting for traces, metrics and logs from all three nodes"
deadline=$((SECONDS + WAIT_SECS))
until traffic && seen >"$WORK/seen.txt"; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    log "timed out after ${WAIT_SECS}s; the collector saw: $(cat "$WORK/seen.txt")"
    exit 1
  fi
  sleep 2
done
log "collector: $(cat "$WORK/seen.txt")"

log "waiting for Jaeger to list the memory-graph service"
deadline=$((SECONDS + WAIT_SECS))
until curl -fsS "http://127.0.0.1:$MG_JAEGER_UI_PORT/api/services" 2>/dev/null | grep -q '"memory-graph"'; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    log "timed out after ${WAIT_SECS}s waiting for Jaeger"
    curl -sS "http://127.0.0.1:$MG_JAEGER_UI_PORT/api/services" || true
    exit 1
  fi
  sleep 2
done

log "ok"
