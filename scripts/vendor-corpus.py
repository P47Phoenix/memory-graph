#!/usr/bin/env python3
"""Vendor the public test corpus described by testdata/corpus/corpus.json.

For each repo: fetch the upstream (depth 1) at the pinned commit, copy the
`include` paths (skipping bin/obj/.git, binaries and files over 1 MiB), keep
the license files, and write UPSTREAM.md. Refuses a repo that is not public
(checked with `gh api`; fails closed unless --skip-visibility-check) and any
owner on the ban-list or licence outside the allow-list. Skipped files are reported.
Usage: python3 scripts/vendor-corpus.py [--skip-visibility-check] [repo-dir ...]
"""
import json, os, shutil, subprocess, sys, tempfile

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "testdata", "corpus")
SKIP_DIRS = {"bin", "obj", ".git", "node_modules", "target"}
MAX_BYTES = 1 << 20
BANNED_OWNERS = {"p47phoenix", "michaelconne"}
LICENSES = {"MIT", "Apache-2.0", "MIT OR Apache-2.0"}
SKIPPED = []

def is_text(path):
    with open(path, "rb") as f:
        data = f.read(MAX_BYTES + 1)
    if len(data) > MAX_BYTES or b"\0" in data:
        return False
    try:
        data.decode("utf-8")
    except UnicodeDecodeError:
        return False
    return True

def copy_tree(src, dst):
    for base, dirs, files in os.walk(src):
        dirs[:] = sorted(d for d in dirs if d not in SKIP_DIRS)
        for name in sorted(files):
            p = os.path.join(base, name)
            if os.path.islink(p) or not is_text(p):
                SKIPPED.append(os.path.relpath(p, src))
                continue
            rel = os.path.relpath(p, os.path.dirname(src) if os.path.isfile(src) else src)
            out = os.path.join(dst, rel)
            os.makedirs(os.path.dirname(out), exist_ok=True)
            shutil.copyfile(p, out)

def check_public(url, skip):
    slug = url.removeprefix("https://github.com/").removesuffix("/").removesuffix(".git")
    if not url.startswith("https://github.com/") or slug.split("/")[0].lower() in BANNED_OWNERS:
        sys.exit(f"refusing {url}: must be a public GitHub repo not owned by a banned owner")
    if skip:
        return
    try:
        vis = subprocess.check_output(["gh", "api", f"repos/{slug}", "--jq", ".visibility"], text=True).strip()
    except (OSError, subprocess.CalledProcessError):
        sys.exit(f"cannot verify {slug} is public (need `gh`); pass --skip-visibility-check to override")
    if vis != "public":
        sys.exit(f"refusing to vendor non-public repo {slug} ({vis})")

def main():
    manifest = json.load(open(os.path.join(ROOT, "corpus.json")))
    args = sys.argv[1:]
    skip = "--skip-visibility-check" in args
    only = {a for a in args if not a.startswith("--")}
    for repo in manifest["repos"]:
        if only and repo["dir"] not in only:
            continue
        if repo["license"] not in LICENSES:
            sys.exit(f"{repo['dir']}: licence {repo['license']} not allowed")
        check_public(repo["upstream"], skip)
        dest = os.path.join(ROOT, repo["dir"])
        with tempfile.TemporaryDirectory() as tmp:
            u = tmp + "/u"
            subprocess.check_call(["git", "init", "-q", u])
            subprocess.check_call(["git", "-C", u, "fetch", "-q", "--depth", "1", repo["upstream"], repo["commit"]])
            subprocess.check_call(["git", "-C", u, "checkout", "-q", "FETCH_HEAD"])
            shutil.rmtree(dest, ignore_errors=True)  # only after a successful fetch
            for inc in repo["include"]:
                src = os.path.realpath(os.path.join(u, inc))
                if not src.startswith(os.path.realpath(u) + os.sep):
                    sys.exit(f"{repo['dir']}: include {inc} escapes the repo")
                if os.path.isdir(src):
                    copy_tree(src, os.path.join(dest, inc))
                elif os.path.isfile(src):
                    os.makedirs(os.path.dirname(os.path.join(dest, inc)) or dest, exist_ok=True)
                    shutil.copyfile(src, os.path.join(dest, inc))
                else:
                    sys.exit(f"{repo['dir']}: {inc} not found upstream")
        with open(os.path.join(dest, "UPSTREAM.md"), "w") as f:
            f.write(f"# {repo['dir']}\n\nVendored subset of {repo['upstream']}\n\n"
                    f"- commit: `{repo['commit']}`\n- license: {repo['license']} (license file(s) included)\n"
                    f"- kind: {repo['kind']}\n")
            if repo.get("split"):
                f.write(f"- split: {repo['split']}\n")
            f.write("\nPublic code only; kept for parser/indexer tests. Do not edit; rerun scripts/vendor-corpus.py.\n")
        print("vendored", repo["dir"])
    if SKIPPED:
        print(f"skipped {len(SKIPPED)} non-text/oversize/symlink files, e.g. {SKIPPED[:5]}")

if __name__ == "__main__":
    main()
