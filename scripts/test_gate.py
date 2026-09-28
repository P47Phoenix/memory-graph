#!/usr/bin/env python3
"""Tests for check-no-c-deps.py using a fixture workspace, plus the workspace's own
deny-list check (`cargo tree -i ring` / `aws-lc-sys` must print nothing): python3 scripts/test_gate.py"""
import os, subprocess, sys, tempfile, textwrap

GATE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "check-no-c-deps.py")
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    open(path, "w").write(textwrap.dedent(text))

def run(cwd, env=None, args=()):
    e = dict(os.environ, **(env or {}))
    return subprocess.run([sys.executable, GATE, *args], cwd=cwd, env=e, capture_output=True, text=True)

with tempfile.TemporaryDirectory() as d:
    write(f"{d}/native-sys/Cargo.toml", '[package]\nname="native-sys"\nversion="0.1.0"\nedition="2021"\nlinks="native"\nbuild="build.rs"\n')
    write(f"{d}/native-sys/build.rs", "fn main(){}\n")
    write(f"{d}/native-sys/src/lib.rs", "")
    write(f"{d}/pure-sys/Cargo.toml", '[package]\nname="pure-sys"\nversion="0.1.0"\nedition="2021"\n')
    write(f"{d}/pure-sys/src/lib.rs", "")
    # Clean: only a pure-Rust -sys crate.
    write(f"{d}/clean/Cargo.toml", '[package]\nname="clean"\nversion="0.1.0"\nedition="2021"\n[dependencies]\npure-sys={path="../pure-sys"}\n')
    write(f"{d}/clean/src/lib.rs", "")
    r = run(f"{d}/clean")
    assert r.returncode == 0 and "checked 1 dependencies" in r.stdout, r.stdout + r.stderr
    assert "targets: x86_64-unknown-linux-gnu" in r.stdout and "aarch64-apple-darwin" in r.stdout, r.stdout
    # Same via --manifest-path from another directory.
    r = run(d, args=["--manifest-path", f"{d}/clean/Cargo.toml"])
    assert r.returncode == 0 and "checked 1 dependencies" in r.stdout, r.stdout + r.stderr
    # Dirty: native-linking crate must fail and be named.
    write(f"{d}/dirty/Cargo.toml", '[package]\nname="dirty"\nversion="0.1.0"\nedition="2021"\n[dependencies]\nnative-sys={path="../native-sys"}\n')
    write(f"{d}/dirty/src/lib.rs", "")
    r = run(f"{d}/dirty")
    assert r.returncode == 1 and "native-sys" in r.stdout and "all targets" in r.stdout, r.stdout + r.stderr
    # Exception passes for that crate only, and is printed.
    write(f"{d}/exc.txt", "native-sys\n")
    r = run(f"{d}/dirty", {"C_DEPS_EXCEPTIONS": f"{d}/exc.txt"})
    assert r.returncode == 0 and "EXCEPTION: native-sys" in r.stdout, r.stdout + r.stderr
    write(f"{d}/exc.txt", "other-crate\n")
    assert run(f"{d}/dirty", {"C_DEPS_EXCEPTIONS": f"{d}/exc.txt"}).returncode == 1
    # Target-aware: a native crate only on a platform we never build for is ignored...
    write(f"{d}/haiku-only/Cargo.toml", '[package]\nname="haiku-only"\nversion="0.1.0"\nedition="2021"\n[target.\'cfg(target_os = "haiku")\'.dependencies]\nnative-sys={path="../native-sys"}\n')
    write(f"{d}/haiku-only/src/lib.rs", "")
    r = run(f"{d}/haiku-only")
    assert r.returncode == 0 and "checked 0 dependencies" in r.stdout, r.stdout + r.stderr
    # ...while one on a single shipped target still fails and names that target.
    write(f"{d}/windows-only/Cargo.toml", '[package]\nname="windows-only"\nversion="0.1.0"\nedition="2021"\n[target.\'cfg(windows)\'.dependencies]\nnative-sys={path="../native-sys"}\n')
    write(f"{d}/windows-only/src/lib.rs", "")
    r = run(f"{d}/windows-only")
    assert r.returncode == 1 and "native-sys" in r.stdout and "targets x86_64-pc-windows-msvc" in r.stdout, r.stdout + r.stderr
    # Deny-list: a crate named like a native TLS/zlib binding fails by name even
    # when it has no `links` key and no build script at all.
    write(f"{d}/libz-sys/Cargo.toml", '[package]\nname="libz-sys"\nversion="0.1.0"\nedition="2021"\n')
    write(f"{d}/libz-sys/src/lib.rs", "")
    write(f"{d}/denied/Cargo.toml", '[package]\nname="denied"\nversion="0.1.0"\nedition="2021"\n[dependencies]\nlibz-sys={path="../libz-sys"}\n')
    write(f"{d}/denied/src/lib.rs", "")
    r = run(f"{d}/denied", {"C_DEPS_EXCEPTIONS": f"{d}/none.txt"})
    assert r.returncode == 1 and "libz-sys" in r.stdout and "deny-listed" in r.stdout, r.stdout + r.stderr

# The workspace itself must not pull a deny-listed crate on any platform
# (`cargo tree -i` inverts the tree: with no such crate it prints nothing).
for crate in ["ring", "aws-lc-sys"]:
    r = subprocess.run(["cargo", "tree", "-i", crate, "--target", "all"], cwd=ROOT, capture_output=True, text=True)
    assert r.stdout.strip() == "", f"`cargo tree -i {crate}` must print nothing:\n{r.stdout}"
print("gate tests passed")
