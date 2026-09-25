#!/usr/bin/env python3
"""Measure how big the database gets per byte of source.

Copies testdata/corpus N times into a temp tree, indexes it in several
commit modes, re-indexes, vacuums/compacts, and prints a Markdown table of
file bytes, ratio to the source, bytes per token, transactions and time.
The numbers are the provenance for the disk projection ratio and the CI
size gate.

    python3 scripts/measure-size.py            # 20x corpus, release build
    python3 scripts/measure-size.py --copies 5 --keep
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CORPUS = ROOT / "testdata" / "corpus"


def sh(args, **kw):
    return subprocess.run(args, check=True, capture_output=True, text=True, **kw)


def tree_bytes(path: Path) -> int:
    return sum(p.stat().st_size for p in path.rglob("*") if p.is_file())


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--copies", type=int, default=20)
    ap.add_argument("--keep", action="store_true", help="keep the temp dir")
    ap.add_argument("--exe", help="memory-graph binary (default: build release)")
    ap.add_argument("--backend", nargs="*", default=None,
                    help="backends to try (default: whatever the binary offers)")
    args = ap.parse_args()

    exe = args.exe
    if not exe:
        sh(["cargo", "build", "--release", "-p", "graph-cli"], cwd=ROOT)
        exe = str(ROOT / "target" / "release" / ("memory-graph.exe" if os.name == "nt" else "memory-graph"))

    tmp = Path(tempfile.mkdtemp(prefix="mg-size-"))
    tree = tmp / "src"
    for i in range(args.copies):
        shutil.copytree(CORPUS, tree / f"c{i}")
    source = tree_bytes(tree)
    print(f"source: {source / 2**20:.1f} MB in {sum(1 for _ in tree.rglob('*') if _.is_file())} files "
          f"({args.copies}x testdata/corpus)\n")

    help_text = sh([exe, "--help"]).stdout
    backends = args.backend
    if backends is None:
        backends = ["v2", "v1"] if "--backend" in help_text else [None]

    rows = []

    def run(db: Path, backend, mode_flags, label, extra=()):
        cmd = [exe, "--db", str(db)]
        if backend:
            cmd += ["--backend", backend]
        cmd += ["index", "--org", "o", "--repo", "r", "--json", "--stats", *mode_flags, *extra, str(tree)]
        t0 = time.time()
        out = sh(cmd).stdout
        secs = time.time() - t0
        summary = json.loads(out)
        size = db.stat().st_size
        tokens = summary.get("tokens", 0)
        txns = summary.get("stats", {}).get("transactions")
        rows.append((backend or "-", label, size, tokens, txns, secs))
        return summary

    def vacuum(db: Path, backend, compact: bool, label):
        cmd = [exe, "--db", str(db)]
        if backend:
            cmd += ["--backend", backend]
        cmd += ["vacuum"] + (["--compact"] if compact else [])
        t0 = time.time()
        subprocess.run(cmd, check=False, capture_output=True, text=True)
        rows.append((backend or "-", label, db.stat().st_size, None, None, time.time() - t0))

    total_tokens = None
    for b in backends:
        for mode, flags in [("default", []), ("--deterministic", ["--deterministic"]),
                            ("--memory 64M", ["--memory", "64M"])]:
            db = tmp / f"{b or 'db'}-{mode.strip('-').replace(' ', '')}.redb"
            s = run(db, b, flags, f"{mode} fresh")
            total_tokens = total_tokens or s.get("tokens")
            if mode == "default":
                run(db, b, flags, "unchanged rerun")
                run(db, b, flags, "--reindex #1", ["--reindex"])
                run(db, b, flags, "--reindex #2", ["--reindex"])
                vacuum(db, b, False, "vacuum")
                vacuum(db, b, True, "vacuum --compact")

    print("| backend | run | db MB | db/source | B/token | txns | s |")
    print("|---|---|---:|---:|---:|---:|---:|")
    for b, label, size, tokens, txns, secs in rows:
        toks = tokens or total_tokens or 0
        bpt = f"{size / toks:.0f}" if toks else "-"
        print(f"| {b} | {label} | {size / 2**20:.1f} | {size / source:.1f}x | {bpt} | "
              f"{txns if txns is not None else '-'} | {secs:.1f} |")

    if args.keep:
        print(f"\nkept {tmp}")
    else:
        shutil.rmtree(tmp, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
