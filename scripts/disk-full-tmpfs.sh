#!/usr/bin/env bash
# Real "No space left on device" test for `memory-graph index` (Linux, needs
# sudo for tmpfs). Indexes a 20x copy of testdata/corpus into a 48 MB tmpfs:
#  1. with the disk check off, the run must hit a real ENOSPC and report it
#     as "disk full", leaving a consistent database;
#  2. with the check on, the run must stop cleanly before the disk fills;
#  3. after enlarging the volume, a rerun must resume (stored files skipped)
#     and finish.
#
#   scripts/disk-full-tmpfs.sh [path/to/memory-graph]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EXE="${1:-$ROOT/target/release/memory-graph}"
[ -x "$EXE" ] || cargo build --release -p graph-cli --manifest-path "$ROOT/Cargo.toml"

WORK="$(mktemp -d)"
MNT="$WORK/vol"
mkdir -p "$MNT" "$WORK/src"
for i in $(seq 1 20); do cp -r "$ROOT/testdata/corpus" "$WORK/src/c$i"; done
sudo mount -t tmpfs -o size=48m tmpfs "$MNT"
sudo chown "$(id -u):$(id -g)" "$MNT"
trap 'sudo umount "$MNT" 2>/dev/null || true; rm -rf "$WORK"' EXIT

stored_from_stderr() { grep -o '[0-9]* files stored' "$1" | head -1 | grep -o '^[0-9]*' || echo 0; }
consistent() {
  "$EXE" --db "$1" describe --json | grep -q '"open_batch":false' || { echo "database has an open batch"; exit 1; }
  "$EXE" --db "$1" search foo --json >/dev/null || { echo "search failed on the database"; exit 1; }
}

echo "== 1. disk check off: a real ENOSPC must be reported as disk full"
set +e
"$EXE" --db "$MNT/a.redb" index --org o --repo r --no-disk-check "$WORK/src" >/dev/null 2>"$WORK/a.err"
status=$?
set -e
tail -3 "$WORK/a.err"
[ "$status" -ne 0 ] || { echo "expected a non-zero exit"; exit 1; }
grep -q "disk full" "$WORK/a.err" || { echo "expected the disk-full message"; exit 1; }
consistent "$MNT/a.redb"
rm -f "$MNT/a.redb"

echo "== 2. disk check on (4 MB reserve): must stop cleanly before the disk fills"
set +e
"$EXE" --db "$MNT/g.redb" index --org o --repo r --min-free-disk 4M "$WORK/src" >/dev/null 2>"$WORK/g.err"
status=$?
set -e
tail -3 "$WORK/g.err"
[ "$status" -ne 0 ] || { echo "expected a non-zero exit"; exit 1; }
grep -q "stopped before the disk filled" "$WORK/g.err" || { echo "expected the disk-guard message"; exit 1; }
stored=$(stored_from_stderr "$WORK/g.err")
echo "stored $stored files before stopping"
[ "$stored" -gt 0 ] || { echo "expected some files stored before the stop"; exit 1; }
consistent "$MNT/g.redb"

echo "== 3. enlarge the volume and rerun (must resume and finish)"
sudo mount -o remount,size=512m "$MNT"
"$EXE" --db "$MNT/g.redb" index --org o --repo r --json --min-free-disk 4M "$WORK/src" >"$WORK/second.json"
python3 - "$WORK/second.json" "$stored" <<'EOF'
import json, sys
s = json.load(open(sys.argv[1])); before = int(sys.argv[2])
assert s["files"] > before, (s["files"], before)
assert s["unchanged"] >= before, (s["unchanged"], before)
print(f"resumed: {s['files']} files, {s['unchanged']} unchanged")
EOF
echo "OK"
