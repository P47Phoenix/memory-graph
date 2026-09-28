#!/usr/bin/env python3
"""Pure-Rust gate: fail on any crate that links a native library or has a build script that compiles C/C++.

Run locally exactly as CI does:  python3 scripts/check-no-c-deps.py
Options: --manifest-path <Cargo.toml> (default: the current directory's, as `cargo metadata` resolves it).

Target-aware (ADR 0004): the dependency graph is resolved once per shipped target
(`cargo metadata --filter-platform <triple>` for each of TARGETS) and the union of those
package sets is checked, so a crate that only exists on a platform we never build for
(e.g. a Haiku-only cc build script, or a wasm-only `links` crate pulled in transitively
by chrono/openraft) does not fail the gate, while anything reachable on any shipped
target does. The targets are printed on every run.

Deny-list: `ring`, `aws-lc-sys`, `openssl-sys`, `libz-sys` fail by name, whatever their
build mechanism (a pure-Rust rewrite would need the name removed here first).

Exceptions: one crate name per line in scripts/c-deps-exceptions.txt (initially empty), or
the file named by C_DEPS_EXCEPTIONS. An excepted crate is printed and allowed, for both
the mechanism check and the deny-list.

Not gated: `xtask/` (dev-only proto codegen, excluded from the workspace, with its own
lockfile; never shipped). Only the workspace's own dependency graph is checked.
"""
import argparse, json, os, subprocess, sys

C_BUILD_DEPS = {"cc", "cmake", "bindgen", "cxx-build", "pkg-config", "vcpkg"}
DENY = {"ring", "aws-lc-sys", "openssl-sys", "libz-sys"}
# Every target a release or CI build of this workspace is made for.
TARGETS = [
    "x86_64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-gnu",
    "aarch64-unknown-linux-musl",
    "aarch64-pc-windows-msvc",
    "x86_64-pc-windows-msvc",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
]

ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
ap.add_argument("--manifest-path", help="Cargo.toml to resolve (default: current directory)")
args = ap.parse_args()

here = os.path.dirname(os.path.abspath(__file__))
exc_file = os.environ.get("C_DEPS_EXCEPTIONS", os.path.join(here, "c-deps-exceptions.txt"))
exceptions = set()
if os.path.exists(exc_file):
    exceptions = {l.strip() for l in open(exc_file) if l.strip() and not l.startswith("#")}


def metadata(target):
    cmd = ["cargo", "metadata", "--format-version", "1", "--filter-platform", target]
    if args.manifest_path:
        cmd += ["--manifest-path", args.manifest_path]
    try:
        return json.loads(subprocess.check_output(cmd, stderr=subprocess.PIPE))
    except subprocess.CalledProcessError as e:
        sys.exit(f"cargo metadata failed for {target} (is Cargo.lock up to date?):\n" + e.stderr.decode())


print("targets: " + ", ".join(TARGETS))
packages, workspace, per_target = {}, set(), {}
for target in TARGETS:
    meta = metadata(target)
    workspace |= set(meta["workspace_members"])
    ids = set()
    for p in meta["packages"]:
        # `packages` lists every crate in the lockfile; `resolve` is the
        # platform-filtered graph, so only the resolved ids count.
        ids.add(p["id"])
        packages.setdefault(p["id"], p)
    resolved = {n["id"] for n in meta["resolve"]["nodes"]} if meta.get("resolve") else ids
    per_target[target] = ids & resolved

union = set().union(*per_target.values())
checked, bad, used = 0, [], []
for pid in sorted(union):
    p = packages[pid]
    if pid in workspace:
        continue
    checked += 1
    reasons = []
    if p["name"] in DENY:
        reasons.append("deny-listed crate name")
    # A `links` key means the crate binds a native (C) library. Pure-Rust
    # `-sys` crates such as windows-sys / linux-raw-sys have neither `links`
    # nor a C build script and are allowed.
    if p.get("links"):
        reasons.append(f"links native library `{p['links']}`")
    if p["name"].endswith("-sys") and not reasons:
        print(f"note: {p['name']} is a -sys crate but has no native link or C build script (pure Rust)")
    build_deps = {d["name"] for d in p["dependencies"] if d["kind"] == "build"}
    has_build_rs = any("custom-build" in t["kind"] for t in p["targets"])
    if has_build_rs and build_deps & C_BUILD_DEPS:
        reasons.append("build.rs uses " + ", ".join(sorted(build_deps & C_BUILD_DEPS)))
    if reasons:
        on = [t for t in TARGETS if pid in per_target[t]]
        where = "all targets" if len(on) == len(TARGETS) else "targets " + ", ".join(on)
        if p["name"] in exceptions:
            used.append(p["name"])
            print(f"EXCEPTION: {p['name']} {p['version']} ({'; '.join(reasons)}; {where})")
        else:
            bad.append(f"{p['name']} {p['version']}: {'; '.join(reasons)} ({where})")
print(f"checked {checked} dependencies")
if bad:
    for b in bad:
        print("FORBIDDEN C/C++ dependency:", b)
    sys.exit(1)
print("pure-Rust gate passed")
