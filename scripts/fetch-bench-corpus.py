#!/usr/bin/env python3
"""Fetch the large public benchmark corpus (read cache A4, issue #233).

Shallow-fetches a pinned list of public, permissively licensed repos into
--dest, a directory OUTSIDE this repo (refused otherwise). Nothing is vendored
or checked in. Each repo is `git init` + `git fetch --depth 1 <url> <sha>` +
checkout of exactly that commit, with line-ending conversion off so the files
are the git blobs. Submodules are not populated. Idempotent and resumable: a
repo at its pinned sha with a matching .git/mg-bench-ok marker is skipped;
anything else (a killed run, stale *.lock files, another sha) is wiped and
re-fetched. Visibility and licence are checked with `gh api` first (fails
closed, like vendor-corpus.py) unless --skip-verify. A failed repo is
reported, the others continue, and the exit status is non-zero at the end.

Usage:
  python3 scripts/fetch-bench-corpus.py --list
  python3 scripts/fetch-bench-corpus.py --dest D:/mg-bench [--only tokio,go] [--skip-verify]

The manifest below was checked with `gh api repos/<owner>/<repo>` (license,
size) and pinned with `gh api repos/<o>/<r>/commits/<branch> -q .sha` on
2026-10-05. `gh_license` is GitHub's SPDX id (NOASSERTION where GitHub cannot
classify the file); `license` is the licence read from the repo itself.
`approx_mb` is a rough checkout size (working tree, not the GitHub pack size).
"""
import argparse, os, shutil, stat, subprocess, sys, time
from collections import defaultdict

REPO_ROOT = os.path.realpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
# Wider than vendor-corpus.py's allow-list (adds BSD, PostgreSQL and the LLVM
# exception) because nothing here is redistributed: repos are fetched to the
# user's own disk, never checked in.
LICENSES = {"MIT", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause",
            "BSD-3-Clause", "PostgreSQL"}
BANNED_OWNERS = {"p47phoenix", "michaelconne"}
MARKER = "mg-bench-ok"  # .git/mg-bench-ok holds the sha once a checkout completed
FETCH_TRIES, FETCH_TIMEOUT = 3, 3600

# name, owner/repo, sha, license, gh_license, approx_mb, main languages
MANIFEST = [
    ("rust", "rust-lang/rust", "db23a2d392783030c008a5fafbe6cb139d1f7707", "Apache-2.0", "Apache-2.0", 300, "Rust"),
    ("tokio", "tokio-rs/tokio", "b2636752450484955e7ad334bac678424d51bc4a", "MIT", "MIT", 10, "Rust"),
    ("llvm-project", "llvm/llvm-project", "75cc30c7b35ce8d5ddd311b67872ed498997d58f", "Apache-2.0 WITH LLVM-exception", "NOASSERTION", 2300, "C++, C, assembly, Python"),
    ("libuv", "libuv/libuv", "954c1d1887bfdb223b2b0dc2201199f938902e68", "MIT", "MIT", 10, "C"),
    ("postgres", "postgres/postgres", "80707c10456f3b29202899c62619b4a8d81fe8f9", "PostgreSQL", "NOASSERTION", 200, "C, SQL, Perl"),
    ("go", "golang/go", "93cc6ae5ec896b3e0d0b6c5cf01361eb5d5cb437", "BSD-3-Clause", "BSD-3-Clause", 350, "Go, assembly"),
    ("kubernetes", "kubernetes/kubernetes", "e234a6f2036b550893eba6d4556b83ac5e7d2a3f", "Apache-2.0", "Apache-2.0", 400, "Go, shell"),
    ("kafka", "apache/kafka", "c2fbf6f52f78ecc1c11c6511242aade6a2c50b71", "Apache-2.0", "Apache-2.0", 90, "Java, Scala"),
    ("spring-framework", "spring-projects/spring-framework", "3a91d153165490b0736c9907fda1d8669c6ef3f4", "Apache-2.0", "Apache-2.0", 70, "Java, Kotlin"),
    ("spark", "apache/spark", "5926a10389e8eace9f655dc138f4bb30254f7bed", "Apache-2.0", "Apache-2.0", 350, "Scala, Python, SQL, R, Java"),
    ("scala3", "scala/scala3", "39f42801b17af4a49df3ae37f35ce65ab874e2a9", "Apache-2.0", "Apache-2.0", 80, "Scala"),
    ("dotnet-runtime", "dotnet/runtime", "c95e736db2bf873e681af9f0c7d262acc751ed9f", "MIT", "MIT", 1000, "C#, C++, C, assembly"),
    ("aspnetcore", "dotnet/aspnetcore", "1c9384556239e201511cadf5ced0bf3f753a98ab", "MIT", "MIT", 300, "C#, TypeScript, Razor, HTML"),
    ("fsharp", "dotnet/fsharp", "d16ac1bc6d29afc0652787af2814ac4d6863a9d9", "MIT", "MIT", 250, "F#"),
    ("react", "react/react", "278794d7dee9cd2a3a2aaf9f0b2a4b8b747d74ee", "MIT", "MIT", 60, "JavaScript"),
    ("typescript", "microsoft/TypeScript", "a1ef42b9ea7032fa60df127d42b4c86fd2a110ee", "Apache-2.0", "Apache-2.0", 600, "TypeScript, JavaScript"),
    ("vscode", "microsoft/vscode", "f8ef5acd033ce44f9e8218e56c8df89f2e6d519e", "MIT", "MIT", 300, "TypeScript"),
    ("django", "django/django", "08e4c0d8e7db6343567e7b02e25da8ea2226a7e0", "BSD-3-Clause", "BSD-3-Clause", 70, "Python, HTML"),
    ("ohmyzsh", "ohmyzsh/ohmyzsh", "d745fbf3bd49a5038e089d7d343ca89db0cbaaff", "MIT", "MIT", 10, "shell"),
    ("nvm", "nvm-sh/nvm", "52a1e55574111d9e66dc2416a09d76bd6c05c33d", "MIT", "MIT", 2, "shell"),
    ("ggplot2", "tidyverse/ggplot2", "737be17f43bebac02ccb119753c1fee267572443", "MIT", "NOASSERTION", 50, "R"),
    ("cabal", "haskell/cabal", "cdb488e5ef41005cb1fa9a534708a459a6e98b2e", "BSD-3-Clause", "NOASSERTION", 30, "Haskell"),
    ("postgrest", "PostgREST/postgrest", "cc508891ccf604e1a0372adfa4f3b3c6a3136dc4", "MIT", "MIT", 10, "Haskell, SQL"),
    ("elixir", "elixir-lang/elixir", "23423047325d0c9fef23e81955ee4295957f8705", "Apache-2.0", "Apache-2.0", 30, "Elixir, Erlang"),
    ("phoenix", "phoenixframework/phoenix", "2ca60ffe811c0e585835cfc309b645c3a4190df1", "MIT", "MIT", 10, "Elixir, JavaScript"),
    ("dav1d", "videolan/dav1d", "7f12cf23560430c02a83e67bb68eec74d93ce5fd", "BSD-2-Clause", "BSD-2-Clause", 15, "assembly, C"),
    ("godot-demo-projects", "godotengine/godot-demo-projects", "3e08537616661a5883831628decab4c526260289", "MIT", "MIT", 400, "GDScript, C#"),
    ("bootstrap", "twbs/bootstrap", "d6d3a990089b96da194031cf3c0206fcc8f59a6c", "MIT", "MIT", 30, "HTML, JavaScript, SCSS"),
    ("ossile", "OSSILE/OSSILE", "7201a67f7e704d73b7da82abd8c520826dd05741", "MIT", "MIT", 10, "RPG, C"),
    ("noxdb", "sitemule/noxDB", "e1075364f5273d579b7e79112bfa665adbbc6a7f", "MIT", "MIT", 40, "C, RPG, SQL"),
    ("carddemo", "aws-samples/aws-mainframe-modernization-carddemo", "59cc6c2fd7ebd7ef7925cad552a01a4b8b6e4d5e", "Apache-2.0", "Apache-2.0", 30, "COBOL, JCL, assembly"),
]

# Extension -> language, for the summary only (indexing uses graph-core's detection).
EXT_LANG = {
    ".rs": "Rust", ".c": "C", ".h": "C/C++ header", ".cc": "C++", ".cpp": "C++", ".cxx": "C++",
    ".hpp": "C++", ".hh": "C++", ".go": "Go", ".java": "Java", ".kt": "Kotlin", ".scala": "Scala",
    ".cs": "C#", ".js": "JavaScript", ".mjs": "JavaScript", ".jsx": "JavaScript", ".ts": "TypeScript",
    ".tsx": "TypeScript", ".py": "Python", ".sql": "SQL", ".sh": "shell", ".bash": "shell", ".zsh": "shell",
    ".r": "R", ".fs": "F#", ".fsi": "F#", ".fsx": "F#", ".hs": "Haskell", ".ex": "Elixir", ".exs": "Elixir",
    ".gd": "GDScript", ".cbl": "COBOL", ".cob": "COBOL", ".cpy": "COBOL", ".s": "assembly", ".asm": "assembly",
    ".html": "HTML", ".htm": "HTML", ".cshtml": "Razor", ".razor": "Razor", ".aspx": "ASP.NET",
    ".rpgle": "RPG", ".sqlrpgle": "RPG", ".rpg": "RPG", ".pl": "Perl", ".pm": "Perl",
}

def entry(t):
    return dict(zip(("name", "slug", "sha", "license", "gh_license", "approx_mb", "langs"), t))

def print_list(repos):
    for r in repos:
        print(f"{r['name']:<22} {r['sha'][:12]}  {r['license']:<31} ~{r['approx_mb']:>5} MB  {r['langs']}")
    print(f"{len(repos)} repos, ~{sum(r['approx_mb'] for r in repos) / 1024:.1f} GB checked out (estimate)")

def check(repo, verify):
    owner = repo["slug"].split("/")[0].lower()
    if owner in BANNED_OWNERS or repo["license"] not in LICENSES:
        sys.exit(f"{repo['name']}: owner or licence {repo['license']} not allowed")
    if not verify:
        return
    try:
        out = subprocess.check_output(["gh", "api", f"repos/{repo['slug']}", "--jq",
                                       '[.visibility, .license.spdx_id] | @tsv'], text=True).split()
    except (OSError, subprocess.CalledProcessError):
        sys.exit(f"cannot verify {repo['slug']} is public (need `gh`); pass --skip-verify to override")
    if out[0] != "public" or out[1] != repo["gh_license"]:
        sys.exit(f"{repo['slug']}: GitHub says {out}, manifest says public/{repo['gh_license']}")

def head(path):
    try:
        return subprocess.check_output(["git", "-C", path, "rev-parse", "HEAD"], text=True,
                                       stderr=subprocess.DEVNULL).strip()
    except (OSError, subprocess.CalledProcessError):
        return None

def marker(path):
    try:
        with open(os.path.join(path, ".git", MARKER)) as f:
            return f.read().strip()
    except OSError:
        return None

def _force_remove(func, p, _exc):
    os.chmod(p, stat.S_IWRITE)  # git objects are read-only on Windows
    func(p)

def fetch(repo, dest):
    path = os.path.join(dest, repo["name"])
    # No dirty-tree check: on a case-insensitive filesystem a repo with
    # case-colliding paths never looks clean. HEAD plus the marker is enough.
    if head(path) == repo["sha"] and marker(path) == repo["sha"]:
        print(f"{repo['name']}: already at {repo['sha'][:12]}, skipped")
        return path
    if os.path.exists(path):  # incomplete: start over (also clears stale *.lock files)
        shutil.rmtree(path, onerror=_force_remove)
    print(f"{repo['name']}: fetching {repo['slug']} @ {repo['sha'][:12]} (~{repo['approx_mb']} MB)", flush=True)
    git = ["git", "-C", path]
    subprocess.check_call(["git", "init", "-q", path])
    subprocess.check_call(git + ["config", "core.autocrlf", "false"])
    subprocess.check_call(git + ["config", "core.longpaths", "true"])
    with open(os.path.join(path, ".git", "info", "attributes"), "w", newline="\n") as f:
        f.write("* -text -filter -ident -working-tree-encoding\n")
    url = f"https://github.com/{repo['slug']}.git"
    for attempt in range(1, FETCH_TRIES + 1):
        try:
            subprocess.run(git + ["fetch", "-q", "--depth", "1", url, repo["sha"]], check=True,
                           timeout=FETCH_TIMEOUT)
            break
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as e:
            if attempt == FETCH_TRIES:
                raise
            print(f"{repo['name']}: fetch attempt {attempt} failed ({e}), retrying", flush=True)
            time.sleep(5 * attempt)
    subprocess.check_call(git + ["checkout", "-q", "--force", "FETCH_HEAD"])
    if head(path) != repo["sha"]:
        raise RuntimeError(f"checkout is not at {repo['sha']}")
    with open(os.path.join(path, ".git", MARKER), "w") as f:
        f.write(repo["sha"] + "\n")
    return path

def summarize(paths):
    by_lang = defaultdict(lambda: [0, 0])
    for p in paths:
        if os.name == "nt":  # paths past MAX_PATH (aspnetcore has some) need the \\?\ prefix
            p = os.path.abspath(p)
            if p.startswith("\\\\?\\"):
                pass
            elif p.startswith("\\\\"):  # UNC \\server\share -> \\?\UNC\server\share
                p = "\\\\?\\UNC\\" + p[2:]
            else:
                p = "\\\\?\\" + p
        for base, dirs, files in os.walk(p):
            dirs[:] = [d for d in dirs if d != ".git"]
            for name in files:
                f = os.path.join(base, name)
                if os.path.islink(f):
                    continue
                lang = EXT_LANG.get(os.path.splitext(name)[1].lower(), "other")
                by_lang[lang][0] += os.path.getsize(f)
                by_lang[lang][1] += 1
    tb, tn = sum(v[0] for v in by_lang.values()), sum(v[1] for v in by_lang.values())
    print("\non-disk bytes by extension (not what is indexed)")
    print(f"{'language':<16} {'MB':>10} {'files':>10} {'share':>7}")
    for lang, (b, n) in sorted(by_lang.items(), key=lambda kv: -kv[1][0]):
        print(f"{lang:<16} {b / 2**20:>10.1f} {n:>10} {100 * b / (tb or 1):>6.1f}%")
    print(f"{'total':<16} {tb / 2**20:>10.1f} {tn:>10}")
    other = by_lang.get("other", [0, 0])[0]
    print(f"'other' (extensions not in the table above): {100 * other / (tb or 1):.1f}% of bytes")

def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--dest", help="directory outside this repo to fetch into")
    ap.add_argument("--list", action="store_true", help="print the manifest and exit")
    ap.add_argument("--only", help="comma-separated repo names")
    ap.add_argument("--skip-verify", action="store_true",
                    help="skip the `gh api` visibility and licence check (on by default)")
    a = ap.parse_args()
    repos = [entry(t) for t in MANIFEST]
    if a.only:
        want = set(a.only.split(","))
        unknown = want - {r["name"] for r in repos}
        if unknown:
            sys.exit(f"unknown repo(s): {', '.join(sorted(unknown))} (see --list)")
        repos = [r for r in repos if r["name"] in want]
    if a.list:
        print_list(repos)
        return
    if not a.dest:
        ap.error("--dest is required (or use --list)")
    dest = os.path.realpath(a.dest)
    try:
        inside = os.path.commonpath([dest, REPO_ROOT]) == REPO_ROOT
    except ValueError:  # different drives on Windows
        inside = False
    if inside:
        sys.exit(f"refusing --dest {dest}: it is inside the repo ({REPO_ROOT})")
    for r in repos:
        check(r, not a.skip_verify)
    os.makedirs(dest, exist_ok=True)
    done, failed = [], []
    for r in repos:
        try:
            done.append(fetch(r, dest))
        except Exception as e:  # keep going; report and exit non-zero at the end
            print(f"{r['name']}: fetch failed: {e}", flush=True)
            failed.append(r["name"])
    summarize(done)
    if failed:
        sys.exit(f"\n{len(failed)} repo(s) failed: {', '.join(failed)} (re-run to resume)")

if __name__ == "__main__":
    main()
