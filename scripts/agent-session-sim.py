#!/usr/bin/env python3
"""Scripted coding-agent session over MCP (read cache A5, issue #233).

Drives `memory-graph --server <addr> mcp` over stdio the way a coding agent
uses the tools, then reads the server's exact-repeat counters
(`mg_query_exact_repeats_total` / `mg_queries_total`, per RPC) from
`--metrics-listen`. The workload is SYNTHETIC; its assumptions are below and in
docs/spikes/read-cache.md ("Scripted-session repeats").

The session is a sequence of "tasks" (one bug or feature an agent works on).
Each task picks a repo and a seed symbol and then mixes, with a seeded RNG:
  - look up a symbol (find_symbols exact name, sometimes a prefix*),
  - open its file (file_outline, then file_tokens on a line window around it),
  - search for callers / uses (search at token or symbol grain, sometimes with
    a language or repo filter added to refine),
  - navigate to a related symbol seen in an outline (new lookup),
  - re-ask: repeat one of the task's own earlier calls verbatim (agents re-read
    a file or re-run a search after editing, or after losing it from context);
    most re-asks hit calls from the last ~60 s, some older ones,
  - occasionally describe / list_files for orientation.
Between calls the agent "thinks" for an exponential delay (default mean
0.3 s), so the 60 s repeat window sees a realistic spread of calls. Seeding
calls (finding repos, files and symbol names) run before the first metrics
scrape and are not counted.

Usage:
  python scripts/agent-session-sim.py --exe target/release/memory-graph \
      --server 127.0.0.1:7000 --metrics 127.0.0.1:9100 --calls 3000 --seed 7
"""
import argparse, json, random, re, subprocess, sys, time, urllib.request
from collections import Counter


class Mcp:
    def __init__(self, exe, server):
        self.p = subprocess.Popen([exe, "--server", server, "mcp"], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                  text=True, encoding="utf-8", bufsize=1)
        self.n = 0
        self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                "clientInfo": {"name": "agent-session-sim", "version": "1"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, msg):
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()

    def rpc(self, method, params):
        self.n += 1
        self.send({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params})
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError("mcp process exited")
            msg = json.loads(line)
            if msg.get("id") == self.n:
                return msg

    def tool(self, name, args):
        msg = self.rpc("tools/call", {"name": name, "arguments": args})
        res = msg.get("result") or {}
        if "structuredContent" in res:
            return res["structuredContent"]
        for c in res.get("content", []):
            if c.get("type") == "text":
                try:
                    return json.loads(c["text"])
                except ValueError:
                    return {}
        return {}

    def close(self):
        self.p.stdin.close()
        self.p.wait(timeout=30)


def scrape(metrics):
    text = urllib.request.urlopen(f"http://{metrics}/metrics", timeout=10).read().decode()
    out = {"q": Counter(), "r": Counter()}
    for line in text.splitlines():
        m = re.match(r'(mg_queries_total|mg_query_exact_repeats_total)\{rpc="([^"]+)"\} (\S+)', line)
        if m:
            out["q" if m.group(1) == "mg_queries_total" else "r"][m.group(2)] += float(m.group(3))
    return out


IDENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]{3,40}$")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--exe", required=True)
    ap.add_argument("--server", required=True)
    ap.add_argument("--metrics", required=True)
    ap.add_argument("--calls", type=int, default=3000)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--think-mean", type=float, default=0.3, help="mean think time between calls, s")
    ap.add_argument("--reask", type=float, default=0.22, help="share of steps that re-ask an earlier call verbatim")
    ap.add_argument("--json", help="write the result here as JSON")
    a = ap.parse_args()
    rng = random.Random(a.seed)
    mcp = Mcp(a.exe, a.server)

    # --- seeding (not counted) ---
    repos = [(r["org"], r["repo"]) for r in mcp.tool("list_repos", {"limit": 500}).get("items", [])]
    if not repos:
        sys.exit("no repos")
    seeds = {}  # repo -> [(symbol name, file)]
    for org, repo in repos:
        got = []
        for pre in rng.sample("abcdefghilmnoprstuw", 4):
            for it in mcp.tool("find_symbols", {"pattern": pre + "*", "org": org, "repo": repo,
                                                "limit": 50}).get("items", []):
                name, f = it.get("name") or it.get("symbol"), it.get("file")
                if name and f and IDENT.match(name):
                    got.append((name, f))
        if got:
            seeds[(org, repo)] = got
    repos = list(seeds)
    print(f"seeded {len(repos)} repos, {sum(len(v) for v in seeds.values())} symbols", file=sys.stderr)

    before = scrape(a.metrics)
    history = []  # (t, tool, args) of this session's calls
    tools = Counter()
    client_repeat = 0
    calls = 0

    def call(tool, args, record=True):
        nonlocal calls, client_repeat
        now = time.monotonic()
        key = (tool, json.dumps(args, sort_keys=True))
        if any(k == key and now - t <= 60 for t, k, _ in history[-2000:]):
            client_repeat += 1
        res = mcp.tool(tool, args)
        if record:
            history.append((now, key, (tool, args)))
        tools[tool] += 1
        calls += 1
        time.sleep(rng.expovariate(1 / a.think_mean) if a.think_mean > 0 else 0)
        return res

    while calls < a.calls:
        org, repo = rng.choice(repos)
        name, path = rng.choice(seeds[(org, repo)])
        task_start = len(history)
        steps = rng.randint(6, 25)
        for _ in range(steps):
            if calls >= a.calls:
                break
            r = rng.random()
            task_hist = history[task_start:]
            reask = r < a.reask and task_hist
            if r >= a.reask:
                # the other actions keep their relative mix whatever --reask
                # is (unchanged at the default 0.22)
                r = 0.22 + (r - a.reask) * 0.78 / (1 - a.reask)
            if reask:
                # re-ask: mostly a recent call of this task, sometimes any older one
                pool = task_hist[-8:] if rng.random() < 0.8 else history[-300:]
                tool, args = rng.choice(pool)[2]
                call(tool, args)
            elif r < 0.37:
                pat = name if rng.random() < 0.8 else name[:max(3, len(name) // 2)] + "*"
                args = {"pattern": pat, "limit": 50}
                if rng.random() < 0.5:
                    args.update(org=org, repo=repo)
                call("find_symbols", args)
            elif r < 0.52:
                res = call("file_outline", {"org": org, "repo": repo, "path": path, "limit": 200})
                items = [i for i in res.get("items", []) if IDENT.match(i.get("name") or "")]
                if items and rng.random() < 0.35:  # navigate to a related symbol
                    name = rng.choice(items)["name"]
            elif r < 0.67:
                start = rng.choice([1, 1, rng.randint(1, 400)])
                call("file_tokens", {"org": org, "repo": repo, "path": path, "start_line": start,
                                     "end_line": start + rng.choice([40, 80, 150]), "limit": 2000})
            elif r < 0.92:
                args = {"text": name, "grain": rng.choice(["token", "token", "symbol", "method", "file"]),
                        "limit": 50}
                if rng.random() < 0.4:  # refine
                    args.update(org=org, repo=repo)
                call("search", args)
            elif r < 0.96:
                call("list_files", {"org": org, "repo": repo, "limit": 100,
                                    "prefix": path.rsplit("/", 1)[0] + "/" if "/" in path else ""})
            else:
                call("describe", {"org": org, "repo": repo} if rng.random() < 0.7 else {})
            # occasionally the agent moves to another file of the same repo
            if rng.random() < 0.1:
                name, path = rng.choice(seeds[(org, repo)])

    after = scrape(a.metrics)
    mcp.close()
    rows = {}
    for rpc in sorted(set(after["q"]) | set(after["r"])):
        q = after["q"][rpc] - before["q"][rpc]
        r = after["r"][rpc] - before["r"][rpc]
        if q:
            rows[rpc] = {"queries": int(q), "repeats": int(r), "share": r / q}
    tq = sum(v["queries"] for v in rows.values())
    tr = sum(v["repeats"] for v in rows.values())
    result = {"mcp_calls": calls, "tools": dict(tools), "client_side_repeats_60s": client_repeat,
              "per_rpc": rows, "total": {"queries": tq, "repeats": tr, "share": tr / tq if tq else 0}}
    print(json.dumps(result, indent=2))
    if a.json:
        with open(a.json, "w") as f:
            json.dump(result, f, indent=2)


if __name__ == "__main__":
    main()
