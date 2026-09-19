#!/usr/bin/env python3
"""Vendor the public test corpus described by testdata/corpus/corpus.json.

For each repo: shallow-clone the upstream at the pinned commit, copy the
`include` paths (skipping bin/obj/.git, binaries and files over 1 MiB), keep
the license files, and write UPSTREAM.md. Refuses a repo that is not public
(checked with `gh api` when available). Usage: python3 scripts/vendor-corpus.py [repo-dir ...]
"""
import json, os, shutil, subprocess, sys, tempfile

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "testdata", "corpus")
SKIP_DIRS = {"bin", "obj", ".git", "node_modules", "target"}
MAX_BYTES = 1 << 20

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
                continue
            rel = os.path.relpath(p, os.path.dirname(src) if os.path.isfile(src) else src)
            out = os.path.join(dst, rel)
            os.makedirs(os.path.dirname(out), exist_ok=True)
            shutil.copyfile(p, out)

def check_public(url):
    slug = url.removeprefix("https://github.com/")
    try:
        vis = subprocess.check_output(["gh", "api", f"repos/{slug}", "--jq", ".visibility"], text=True).strip()
    except (OSError, subprocess.CalledProcessError):
        print(f"warning: could not verify visibility of {slug}", file=sys.stderr)
        return
    if vis != "public":
        sys.exit(f"refusing to vendor non-public repo {slug} ({vis})")

def main():
    manifest = json.load(open(os.path.join(ROOT, "corpus.json")))
    only = set(sys.argv[1:])
    for repo in manifest["repos"]:
        if only and repo["dir"] not in only:
            continue
        check_public(repo["upstream"])
        dest = os.path.join(ROOT, repo["dir"])
        shutil.rmtree(dest, ignore_errors=True)
        with tempfile.TemporaryDirectory() as tmp:
            subprocess.check_call(["git", "clone", "-q", repo["upstream"], tmp + "/u"])
            subprocess.check_call(["git", "-C", tmp + "/u", "checkout", "-q", repo["commit"]])
            for inc in repo["include"]:
                src = os.path.join(tmp, "u", inc)
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

main()
