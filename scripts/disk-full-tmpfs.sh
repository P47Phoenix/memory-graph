#!/usr/bin/env bash
# Real "No space left on device" test for `memory-graph index` (Linux, needs
# sudo for tmpfs). Indexes a 20x copy of testdata/corpus into a 48 MB tmpfs
# with a 4 MB reserve: the run must stop cleanly before the disk fills, the
# database must stay consistent, and a rerun after enlarging the volume must
# resume (stored files skipped as unchanged) and finish.
#
#   scripts/disk-full-tmpfs.sh [path/to/memory-graph]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EXE="${1:-$ROOT/target/release/memory-graph}"
[ -x "$EXE" ] || { cargo build --release -p graph-cli --manifest-path "$ROOT/Cargo.toml"; }

WORK="$(mktemp -d)"
MNT="$WORK/vol"
mkdir -p "$MNT" "$WORK/src"
for i in $(seq 1 20); do cp -r "$ROOT/testdata/corpus" "$WORK/src/c$i"; done
sudo mount -t tmpfs -o size=48m tmpfs "$MNT"
sudo chown "$(id -u):$(id -g)" "$MNT"
trap 'sudo umount "$MNT" 2>/dev/null || true; rm -rf "$WORK"' EXIT

echo "== index into a 48 MB volume with a 4 MB reserve (must stop cleanly)"
set +e
"$EXE" --db "$MNT/g.redb" index --org o --repo r --json --min-free-disk 4M "$WORK/src" >"$WORK/first.json" 2>"$WORK/first.err"
status=$?
set -e
cat "$WORK/first.err"
[ "$status" -ne 0 ] || { echo "expected a non-zero exit"; exit 1; }
grep -q "stopped before the disk filled\|disk full" "$WORK/first.err" || { echo "expected the disk-guard message"; exit 1; }
stored=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['files'])" "$WORK/first.json" 2>/dev/null || echo 0)
echo "stored $stored files before stopping"
"$EXE" --db "$MNT/g.redb" describe --json | grep -q '"open_batch":false' && echo "database consistent (no open batch)"

echo "== enlarge the volume and rerun (must resume and finish)"
sudo mount -o remount,size=512m "$MNT"
"$EXE" --db "$MNT/g.redb" index --org o --repo r --json "$WORK/src" >"$WORK/second.json"
python3 - "$WORK/second.json" "$stored" <<'EOF'
import json, sys
s = json.load(open(sys.argv[1])); before = int(sys.argv[2])
assert s["files"] > before, (s["files"], before)
assert s["unchanged"] >= before, (s["unchanged"], before)
print(f"resumed: {s['files']} files, {s['unchanged']} unchanged")
EOF
echo "OK"
