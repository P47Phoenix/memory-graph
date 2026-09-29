#!/usr/bin/env python3
"""Cluster kill test (ADR 0004 test plan D, epic story 23).

Starts three real `memory-graph serve --data-dir` processes (node 1
`--bootstrap`, nodes 2-3 `--join <node 1> --auto-promote`), waits for three
voters, and runs a writer thread that indexes numbered repos through
`--server a,b,c` (the client moves on from a dead or leaderless node),
alternating one-file `index-file` batches and multi-file `index <dir>`
batches (--batch-files, default 20), recording every batch whose command
exited 0 (acknowledged). Meanwhile it
kills the current leader (SIGKILL; TerminateProcess on Windows) ROUNDS times:
kill, let the writer keep going on the survivors, restart the killed node
with no cluster flags, wait until all three report the same applied index.
At the end every acknowledged batch must be present on every node, all of
its files (`describe --json --read local`, per node, after convergence: the
repo's file count).

A node's first start retries on a fresh port when its port was taken
between `free_port()` and the bind (another process on the machine); a
restart keeps its port (the cluster knows the node by it) and retries it.

Only processes this script started are ever killed. Every wait has a hard
timeout and fails with what it waited for. Pure Python stdlib.

    python3 scripts/cluster_kill_test.py                 # builds release first
    python3 scripts/cluster_kill_test.py --bin target/release/memory-graph --rounds 3
"""
import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time

EXE = ".exe" if os.name == "nt" else ""


def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_for(what, probe, timeout):
    """Poll `probe` until it returns a truthy value; fail after `timeout` s."""
    deadline = time.monotonic() + timeout
    while True:
        v = probe()
        if v:
            return v
        if time.monotonic() > deadline:
            raise SystemExit(f"FAIL: timed out after {timeout}s waiting for {what}")
        time.sleep(0.2)


class Node:
    # Popen creationflags (the soak runs each node in its own process
    # group on Windows, so a Ctrl-Break stops only that node).
    popen_flags = 0

    def __init__(self, bin_path, root, node_id, port):
        self.bin = bin_path
        self.id = node_id
        self.port = port
        self.addr = f"127.0.0.1:{port}"
        self.dir = os.path.join(root, f"n{node_id}")
        self.log_path = os.path.join(root, f"n{node_id}.log")
        self.proc = None
        self.ready = threading.Event()

    def set_port(self, port):
        self.port = port
        self.addr = f"127.0.0.1:{port}"

    def start(self, extra, first=False):
        """Start; retry when the process exits before it listens because
        its port is in use (a fresh port on a first start, the same one on
        a restart)."""
        for attempt in range(1, 6):
            if self._start_once(extra):
                return
            with open(self.log_path, encoding="utf-8", errors="replace") as f:
                tail = f.read()[-4000:]
            in_use = any(m in tail.lower() for m in ("address already in use", "addrinuse",
                                                     "os error 98", "os error 10048",
                                                     "only one usage of each socket address"))
            if not in_use:
                raise SystemExit(f"FAIL: node {self.id} exited before listening (log {self.log_path})")
            if first:
                # A first start owns nothing yet: start over on an empty
                # directory (the address is recorded in node.json).
                shutil.rmtree(self.dir, ignore_errors=True)
                old = self.port
                self.set_port(free_port())
                log(f"node {self.id}: port {old} was taken, retrying on {self.port} (attempt {attempt})")
            else:
                log(f"node {self.id}: port {self.port} still in use, retrying (attempt {attempt})")
                time.sleep(1)
        raise SystemExit(f"FAIL: node {self.id} could not bind a port after 5 attempts (log {self.log_path})")

    def _start_once(self, extra):
        """True once it printed `listening on`; False if it exited first."""
        cmd = [self.bin, "serve", "--data-dir", self.dir, "--listen", self.addr] + extra
        log(f"start node {self.id}: {' '.join(cmd[1:])}")
        self.ready.clear()
        logf = open(self.log_path, "a", encoding="utf-8")
        self.proc = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=logf,
            stdin=subprocess.DEVNULL,
            creationflags=self.popen_flags,
            text=True,
            encoding="utf-8",
            errors="replace",
        )
        proc = self.proc

        def pump():
            for line in proc.stdout:
                logf.write(line)
                logf.flush()
                if "listening on" in line:
                    self.ready.set()

        threading.Thread(target=pump, daemon=True).start()
        deadline = time.monotonic() + 120
        while not self.ready.wait(0.2):
            if proc.poll() is not None and not self.ready.is_set():
                proc.wait()
                logf.flush()
                return False
            if time.monotonic() > deadline:
                raise SystemExit(f"FAIL: node {self.id} did not print `listening on` in 120s (log {self.log_path})")
        return True

    def kill(self):
        """SIGKILL (POSIX) / TerminateProcess (Windows): no clean shutdown."""
        log(f"KILL node {self.id} (pid {self.proc.pid})")
        self.proc.kill()
        self.proc.wait(30)

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.kill()
            try:
                self.proc.wait(30)
            except subprocess.TimeoutExpired:
                pass


def cli(bin_path, args, server, timeout=120):
    r = subprocess.run(
        [bin_path] + args + ["--server", server],
        capture_output=True,
        text=True,
        timeout=timeout,
        stdin=subprocess.DEVNULL,
    )
    return r


def status(bin_path, node):
    try:
        r = cli(bin_path, ["cluster", "status", "--json"], node.addr, timeout=20)
    except subprocess.TimeoutExpired:
        return None
    if r.returncode != 0:
        return None
    try:
        return json.loads(r.stdout)
    except json.JSONDecodeError:
        return None


def converged(bin_path, nodes):
    """The common applied index when every node reports the same one and a
    leader is known, else False."""
    sts = [status(bin_path, n) for n in nodes.values()]
    if any(s is None for s in sts):
        return False
    idx = {s["applied_index"] for s in sts}
    if len(idx) != 1 or any(s["leader_id"] is None for s in sts):
        return False
    return idx.pop() or False


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", help="memory-graph binary (default: build release and use it)")
    ap.add_argument("--rounds", type=int, default=3, help="leader kills (default 3)")
    ap.add_argument("--writes-between", type=int, default=8,
                    help="acknowledged writes to wait for before each kill and while the node is down")
    ap.add_argument("--batch-files", type=int, default=20,
                    help="files per multi-file `index <dir>` batch (every other batch; default 20)")
    ap.add_argument("--keep", action="store_true", help="keep the temp directory")
    a = ap.parse_args()

    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    bin_path = a.bin
    if not bin_path:
        log("cargo build --release -p graph-cli")
        subprocess.run(["cargo", "build", "--release", "-p", "graph-cli"], cwd=repo_root, check=True)
        bin_path = os.path.join(repo_root, "target", "release", "memory-graph" + EXE)
    bin_path = os.path.abspath(bin_path)

    root = tempfile.mkdtemp(prefix="mg-cluster-kill-")
    src = os.path.join(root, "src")
    os.makedirs(src)
    nodes = {i: Node(bin_path, root, i, free_port()) for i in (1, 2, 3)}
    all_eps = ""  # set once every node listens (a first start may move a port)
    env_ok = True
    stop = threading.Event()
    pause = threading.Event()  # set: the writer idles (idle is set once it does)
    idle = threading.Event()
    acked = {}  # repo -> file count, for batches whose command exited 0
    attempted = [0]
    failures = []
    lock = threading.Lock()

    def writer():
        i = 0
        while not stop.is_set():
            if pause.is_set():
                idle.set()
                time.sleep(0.05)
                continue
            idle.clear()
            repo = f"r{i:05d}"
            if i % 2 == 1 and a.batch_files > 1:
                # A multi-file batch: one `index <dir>` of batch_files files.
                path = os.path.join(src, f"b{i}")
                os.makedirs(path)
                for k in range(a.batch_files):
                    with open(os.path.join(path, f"m{k}.rs"), "w", encoding="utf-8") as f:
                        f.write(f"fn b{i}_{k}() -> u32 {{ {k} }}\n")
                args, nfiles = ["index", "--org", "kill", "--repo", repo, path], a.batch_files
            else:
                path = os.path.join(src, f"f{i}.rs")
                with open(path, "w", encoding="utf-8") as f:
                    f.write(f"fn f{i}() -> u32 {{ {i} }}\n")
                args, nfiles = ["index-file", "--org", "kill", "--repo", repo, path], 1
            try:
                r = cli(bin_path, args + ["--write-deadline", "30s"], all_eps, timeout=120)
                rc = r.returncode
            except subprocess.TimeoutExpired:
                rc = None
            with lock:
                attempted[0] += 1
                if rc == 0:
                    acked[repo] = nfiles
                else:
                    # Not acknowledged: may or may not be applied; never checked.
                    failures.append((repo, rc))
            i += 1

    def acked_count():
        with lock:
            return len(acked)

    try:
        nodes[1].start(["--node-id", "1", "--bootstrap"], first=True)
        for i in (2, 3):
            nodes[i].start(["--node-id", str(i), "--join", nodes[1].addr, "--auto-promote"], first=True)
        all_eps = ",".join(n.addr for n in nodes.values())

        def three_voters():
            r = cli(bin_path, ["cluster", "members", "--json"], nodes[1].addr, timeout=20)
            if r.returncode != 0:
                return False
            m = json.loads(r.stdout)
            return sum(1 for x in m["members"] if x["role"] == "voter") == 3

        wait_for("three voters", three_voters, 180)
        log("three voters")

        t = threading.Thread(target=writer, daemon=True)
        t.start()
        for rnd in range(1, a.rounds + 1):
            base = acked_count()
            wait_for(f"{a.writes_between} acked writes before kill {rnd}",
                     lambda: acked_count() >= base + a.writes_between, 300)

            def leader_id():
                r = cli(bin_path, ["cluster", "leader", "--json"], all_eps, timeout=20)
                if r.returncode != 0:
                    return None
                return json.loads(r.stdout).get("leader_id")

            lid = wait_for("a leader", leader_id, 120)
            victim = nodes[lid]
            victim.kill()
            base = acked_count()
            wait_for(f"{a.writes_between} acked writes with node {lid} down",
                     lambda: acked_count() >= base + a.writes_between, 300)
            victim.start([])  # a plain restart: no --bootstrap / --join

            # Convergence is checked with the writer paused (its write in
            # flight finished), then it resumes.
            pause.set()
            wait_for("the writer to pause", idle.is_set, 180)
            idx = wait_for(f"all three nodes at one applied index after round {rnd}",
                           lambda: converged(bin_path, nodes), 180)
            pause.clear()
            log(f"round {rnd}: node {lid} killed and restarted, converged at {idx}; "
                f"{acked_count()} acked so far")

        stop.set()
        t.join(180)
        if t.is_alive():
            raise SystemExit("FAIL: the writer did not stop within 180s")

        idx = wait_for("all three nodes at the same applied index",
                       lambda: converged(bin_path, nodes), 180)
        log(f"converged at applied index {idx}")

        with lock:
            want = dict(acked)
            n_attempted, n_failed = attempted[0], len(failures)
        multi = sum(1 for n in want.values() if n > 1)
        for n in nodes.values():
            r = cli(bin_path, ["describe", "--json", "--read", "local"], n.addr, timeout=120)
            if r.returncode != 0:
                raise SystemExit(f"FAIL: describe on node {n.id}: {r.stderr}")
            doc = json.loads(r.stdout)
            have = {x["repo"]: x["files"] for x in doc["repos"] if x["org"] == "kill"}
            missing = sorted(r for r in want if r not in have)
            short = sorted(f"{r} ({have[r]}/{want[r]} files)" for r in want
                           if r in have and have[r] < want[r])
            if missing or short:
                env_ok = False
                if missing:
                    log(f"FAIL: node {n.id} misses {len(missing)} acknowledged batches: {missing[:20]}")
                if short:
                    log(f"FAIL: node {n.id} misses files of {len(short)} acknowledged batches: {short[:20]}")
            else:
                log(f"node {n.id}: all {len(want)} acknowledged batches present with every file "
                    f"({len(have)} repos; stale_possible={doc.get('stale_possible')})")
        log(f"{n_attempted} batches attempted, {len(want)} acknowledged ({multi} of "
            f"{a.batch_files} files), {n_failed} not acknowledged")
        if not env_ok:
            raise SystemExit("FAIL: acknowledged writes lost")
        if len(want) == 0:
            raise SystemExit("FAIL: nothing was acknowledged")
        if a.batch_files > 1 and multi == 0:
            raise SystemExit("FAIL: no multi-file batch was acknowledged")
        log(f"PASS: {a.rounds} leader kills, every acknowledged batch on every node")
    finally:
        stop.set()
        for n in nodes.values():
            n.stop()
        if a.keep or not env_ok:
            log(f"kept {root}")
        else:
            shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
