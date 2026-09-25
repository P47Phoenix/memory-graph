#!/usr/bin/env python3
"""Check `memory-graph sysinfo --json` output: the memory probe worked on
this platform, and (with --max-total BYTES) a container's cgroup limit was
honoured. CI runs it on ubuntu, macOS and inside `docker run --memory`.

  memory-graph sysinfo --json | python3 scripts/check-probes.py [--max-total BYTES] [--source-contains TEXT]
"""
import argparse, json, sys

ap = argparse.ArgumentParser()
ap.add_argument("--max-total", type=int, help="memory.total must be at most this (a cgroup limit)")
ap.add_argument("--source-contains", help="memory.source must contain this text")
a = ap.parse_args()

v = json.load(sys.stdin)
m = v["memory"]
print("memory:", json.dumps(m))
print("disk:", json.dumps(v["disk"]))
bad = []
if m.get("error") is not None:
    bad.append(f"memory probe failed: {m['error']}")
if not m.get("source"):
    bad.append("memory.source is empty")
if not (m.get("total") or 0) > 0:
    bad.append("memory.total is not positive")
if not (m.get("available") or 0) > 0:
    bad.append("memory.available is not positive")
if not (m.get("rss") or 0) > 0:
    bad.append("memory.rss is not positive")
if a.max_total is not None and (m.get("total") or 0) > a.max_total:
    bad.append(f"memory.total {m.get('total')} exceeds the limit {a.max_total}")
if a.source_contains and a.source_contains not in (m.get("source") or ""):
    bad.append(f"memory.source {m.get('source')!r} does not mention {a.source_contains!r}")
if not (v["disk"].get("total") or 0) > 0:
    bad.append("disk.total is not positive")
if v["budget"]["bytes"] <= 0:
    bad.append("budget.bytes is not positive")
for b in bad:
    print("FAIL:", b)
if bad:
    sys.exit(1)
print("probes ok")
