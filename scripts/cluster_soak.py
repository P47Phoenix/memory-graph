#!/usr/bin/env python3
"""Cluster soak test (ADR 0004 revisit triggers, epic story 25 AC 2).

Three real `memory-graph serve --data-dir` processes (node 1 `--bootstrap`,
nodes 2-3 `--join <node 1> --auto-promote`) under a continuous indexing
loop for --minutes (default 60). Every --restart-every seconds (default
300) one node is restarted, round robin 1, 2, 3, ..., alternating a
graceful stop (SIGTERM; Ctrl-Break on Windows, where each node runs in its
own process group) and a kill (SIGKILL / TerminateProcess); the loop keeps
writing while it is down, and the next restart waits until it caught up.
Every --sample-every seconds each node's `cluster status` is recorded:
`log_bytes` (the size of `raft.redb`), `purged_index`, `snapshot_index`,
`applied_index`.

At the end (writer stopped, all nodes at one applied index):
  * every acknowledged batch (its command exited 0) is present on every
    node with all of its files (`describe --json --read local`);
  * every graceful stop exited 0;
  * the log stays within the purge policy: on every node the purged index
    advanced during the run (snapshots purge the log), and `log_bytes`
    never exceeded --max-log-bytes (default 64 MiB);
  * a table of log bytes over time is printed (and written as CSV with
    --csv).

The process, port, wait and convergence machinery is the kill test's
(`scripts/cluster_kill_test.py`, imported). Only processes this script
started are stopped or killed. Every wait has a hard timeout. Pure Python
stdlib.

    python3 scripts/cluster_soak.py                          # builds release first, 60 min
    python3 scripts/cluster_soak.py --bin target/release/memory-graph --minutes 5 --restart-every 60
"""
import argparse
import csv
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cluster_kill_test as ckt  # noqa: E402  (the shared machinery)

log = ckt.log
wait_for = ckt.wait_for
cli = ckt.cli
status = ckt.status


class SoakNode(ckt.Node):
    """A kill-test node that can also be stopped gracefully."""

    # Its own process group on Windows, so Ctrl-Break reaches only it.
    popen_flags = subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0

    def graceful(self, timeout=120):
        """SIGTERM / Ctrl-Break, then wait for the exit; returns its code."""
        log(f"STOP node {self.id} gracefully (pid {self.proc.pid})")
        if os.name == "nt":
            self.proc.send_signal(signal.CTRL_BREAK_EVENT)
        else:
            self.proc.send_signal(signal.SIGTERM)
        try:
            return self.proc.wait(timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(30)
            raise SystemExit(f"FAIL: node {self.id} did not stop within {timeout}s of the signal")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", help="memory-graph binary (default: build release and use it)")
    ap.add_argument("--minutes", type=float, default=60.0, help="how long to soak (default 60)")
    ap.add_argument("--restart-every", type=float, default=300.0,
                    help="seconds between node restarts (default 300)")
    ap.add_argument("--sample-every", type=float, default=30.0,
                    help="seconds between status samples (default 30)")
    ap.add_argument("--batch-files", type=int, default=20,
                    help="files per multi-file `index <dir>` batch (every other batch; default 20)")
    ap.add_argument("--snapshot-log-entries", type=int, default=1000,
                    help="serve --snapshot-log-entries for the nodes (default 1000, so snapshots and "
                         "purges happen within a short run)")
    ap.add_argument("--log-keep-entries", type=int, default=100,
                    help="serve --log-keep-entries for the nodes (default 100)")
    ap.add_argument("--max-log-bytes", type=int, default=64 << 20,
                    help="fail if any node's log_bytes exceeds this (default 64 MiB)")
    ap.add_argument("--csv", help="write the status samples here")
    ap.add_argument("--keep", action="store_true", help="keep the temp directory")
    a = ap.parse_args()

    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    bin_path = a.bin
    if not bin_path:
        log("cargo build --release -p graph-cli")
        subprocess.run(["cargo", "build", "--release", "-p", "graph-cli"], cwd=repo_root, check=True)
        bin_path = os.path.join(repo_root, "target", "release", "memory-graph" + ckt.EXE)
    bin_path = os.path.abspath(bin_path)

    root = tempfile.mkdtemp(prefix="mg-cluster-soak-")
    src = os.path.join(root, "src")
    os.makedirs(src)
    nodes = {i: SoakNode(bin_path, root, i, ckt.free_port()) for i in (1, 2, 3)}
    tuning = ["--snapshot-log-entries", str(a.snapshot_log_entries),
              "--log-keep-entries", str(a.log_keep_entries),
              # The test machine's free space is not what is being tested.
              "--min-free-disk", "1"]
    eps = [""]  # every node's address, once all listen
    ok = True
    stop = threading.Event()
    pause = threading.Event()
    idle = threading.Event()
    acked = {}
    attempted = [0]
    failures = []
    lock = threading.Lock()
    samples = []  # (elapsed s, node, log_bytes, purged, snapshot, applied)
    restarts = []  # (elapsed s, node, how, exit code or None, catch-up s)

    def writer():
        i = 0
        while not stop.is_set():
            if pause.is_set():
                idle.set()
                time.sleep(0.05)
                continue
            idle.clear()
            repo = f"r{i:06d}"
            if i % 2 == 1 and a.batch_files > 1:
                path = os.path.join(src, f"b{i}")
                os.makedirs(path)
                for k in range(a.batch_files):
                    with open(os.path.join(path, f"m{k}.rs"), "w", encoding="utf-8") as f:
                        f.write(f"fn b{i}_{k}() -> u32 {{ {k} }}\n")
                args, nfiles = ["index", "--org", "soak", "--repo", repo, path], a.batch_files
            else:
                path = os.path.join(src, f"f{i}.rs")
                with open(path, "w", encoding="utf-8") as f:
                    f.write(f"fn f{i}() -> u32 {{ {i} }}\n")
                args, nfiles = ["index-file", "--org", "soak", "--repo", repo, path], 1
            try:
                rc = cli(bin_path, args + ["--write-deadline", "30s"], eps[0], timeout=120).returncode
            except subprocess.TimeoutExpired:
                rc = None
            with lock:
                attempted[0] += 1
                if rc == 0:
                    acked[repo] = nfiles
                else:
                    failures.append((repo, rc))
            # The source is not needed once indexed (a long run makes many).
            if os.path.isdir(path):
                shutil.rmtree(path, ignore_errors=True)
            elif os.path.exists(path):
                os.remove(path)
            i += 1

    t0 = time.monotonic()

    def sample():
        now = round(time.monotonic() - t0, 1)
        for n in nodes.values():
            if n.proc is None or n.proc.poll() is not None:
                continue
            st = status(bin_path, n)
            if st:
                samples.append((now, n.id, st.get("log_bytes", 0), st.get("purged_index", 0),
                                st.get("snapshot_index", 0), st.get("applied_index", 0)))

    def leader_committed():
        r = cli(bin_path, ["cluster", "status", "--json"], eps[0], timeout=20)
        if r.returncode != 0:
            return None
        st = json.loads(r.stdout)
        lid = st.get("leader_id")
        if lid is None:
            return None
        lst = status(bin_path, nodes[lid])
        return lst and lst.get("committed_index")

    try:
        nodes[1].start(["--node-id", "1", "--bootstrap"] + tuning, first=True)
        for i in (2, 3):
            nodes[i].start(["--node-id", str(i), "--join", nodes[1].addr, "--auto-promote"] + tuning,
                           first=True)
        eps[0] = ",".join(n.addr for n in nodes.values())

        def three_voters():
            r = cli(bin_path, ["cluster", "members", "--json"], nodes[1].addr, timeout=20)
            if r.returncode != 0:
                return False
            m = json.loads(r.stdout)
            return sum(1 for x in m["members"] if x["role"] == "voter") == 3

        wait_for("three voters", three_voters, 180)
        log(f"three voters; soaking for {a.minutes} min, a restart every {a.restart_every}s")
        t0 = time.monotonic()
        threading.Thread(target=writer, daemon=True).start()

        end = t0 + a.minutes * 60
        next_sample = t0
        next_restart = t0 + a.restart_every
        k = 0
        while time.monotonic() < end:
            now = time.monotonic()
            if now >= next_sample:
                sample()
                with lock:
                    n_acked = len(acked)
                last = {s[1]: s for s in samples[-3:]}
                log(f"t={now - t0:.0f}s acked={n_acked} " + " ".join(
                    f"n{i}:log={s[2]}B,purged={s[3]},applied={s[5]}" for i, s in sorted(last.items())))
                next_sample = now + a.sample_every
            if now >= next_restart:
                victim = nodes[k % 3 + 1]
                how = "graceful" if k % 2 == 0 else "kill"
                k += 1
                at = round(now - t0, 1)
                code = None
                if how == "graceful":
                    code = victim.graceful()
                    if code != 0:
                        ok = False
                        log(f"FAIL: node {victim.id} exited {code} on a graceful stop")
                else:
                    victim.kill()
                # Down for a moment while the writer keeps going.
                time.sleep(2)
                victim.start([])  # a plain restart
                target = wait_for("the leader's committed index", leader_committed, 180)
                t_up = time.monotonic()
                wait_for(f"node {victim.id} to catch up to {target}",
                         lambda: (status(bin_path, victim) or {}).get("applied_index", 0) >= target, 300)
                catch_up = round(time.monotonic() - t_up, 2)
                restarts.append((at, victim.id, how, code, catch_up))
                log(f"restart {k}: node {victim.id} ({how}, exit {code}) caught up to {target} "
                    f"in {catch_up}s")
                next_restart = time.monotonic() + a.restart_every
            time.sleep(0.5)

        # Final state: writer paused and stopped, all nodes converged.
        pause.set()
        wait_for("the writer to pause", idle.is_set, 180)
        stop.set()
        idx = wait_for("all three nodes at one applied index", lambda: ckt.converged(bin_path, nodes), 300)
        sample()
        log(f"converged at applied index {idx}")

        with lock:
            want = dict(acked)
            n_attempted, n_failed = attempted[0], len(failures)
        for n in nodes.values():
            r = cli(bin_path, ["describe", "--json", "--read", "local"], n.addr, timeout=300)
            if r.returncode != 0:
                raise SystemExit(f"FAIL: describe on node {n.id}: {r.stderr}")
            doc = json.loads(r.stdout)
            have = {x["repo"]: x["files"] for x in doc["repos"] if x["org"] == "soak"}
            missing = sorted(x for x in want if x not in have)
            short = sorted(x for x in want if x in have and have[x] < want[x])
            if missing or short:
                ok = False
                log(f"FAIL: node {n.id} misses {len(missing)} acknowledged batches {missing[:10]} "
                    f"and files of {len(short)} {short[:10]}")
            else:
                log(f"node {n.id}: all {len(want)} acknowledged batches present with every file")

        # The purge policy: purged index advancing, log bytes bounded.
        for i in nodes:
            mine = [s for s in samples if s[1] == i]
            purged = [s[3] for s in mine]
            peak = max(s[2] for s in mine)
            if not purged or max(purged) <= min(purged):
                ok = False
                log(f"FAIL: node {i}: the purged index never advanced ({purged[:1]}..{purged[-1:]})")
            if peak > a.max_log_bytes:
                ok = False
                log(f"FAIL: node {i}: log_bytes peaked at {peak} > --max-log-bytes {a.max_log_bytes}")
            log(f"node {i}: log_bytes min {min(s[2] for s in mine)} / peak {peak} / last {mine[-1][2]}; "
                f"purged index {purged[0]} -> {purged[-1]} ({len(mine)} samples)")

        log("log bytes over time (per node, every sample):")
        log("  t_s    node  log_bytes  purged  snapshot  applied")
        for s in samples:
            log(f"  {s[0]:>6} {s[1]:>4} {s[2]:>10} {s[3]:>7} {s[4]:>9} {s[5]:>8}")
        if a.csv:
            with open(a.csv, "w", newline="", encoding="utf-8") as f:
                w = csv.writer(f)
                w.writerow(["t_s", "node", "log_bytes", "purged_index", "snapshot_index", "applied_index"])
                w.writerows(samples)
            log(f"samples written to {a.csv}")
        for r in restarts:
            log(f"restart at t={r[0]}s node {r[1]} {r[2]} exit={r[3]} catch-up {r[4]}s")
        elapsed = time.monotonic() - t0
        log(f"{n_attempted} batches attempted, {len(want)} acknowledged, {n_failed} not acknowledged, "
            f"{len(restarts)} restarts, in {elapsed / 60:.1f} min")
        if not want:
            ok = False
            log("FAIL: nothing was acknowledged")
        if not ok:
            raise SystemExit("FAIL: see above")
        log(f"PASS: {elapsed / 60:.1f} min soak, {len(restarts)} restarts, every acknowledged batch on "
            f"every node, log within the purge policy")
    finally:
        stop.set()
        for n in nodes.values():
            n.stop()
        if a.keep or not ok:
            log(f"kept {root}")
        else:
            shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
