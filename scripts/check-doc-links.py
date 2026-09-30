#!/usr/bin/env python3
"""Check relative links and #anchors in README.md and docs/**/*.md.

Every `[text](target)` (and `[label]: target` definition) outside code blocks and
inline code is checked: a relative file target must exist, and a `#anchor` on a
Markdown target (or on the same file) must match a heading, using GitHub's slug
rules, or an explicit `<a name/id="...">`. http(s), mailto and other scheme links
are skipped. Pure stdlib: python3 scripts/check-doc-links.py [--self-test]
"""
import os
import re
import sys
import unicodedata
from urllib.parse import unquote

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

FENCE = re.compile(r"^\s{0,3}(`{3,}|~{3,})")
HEADING = re.compile(r"^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$")
INLINE_CODE = re.compile(r"(`+)(?:(?!\1).)+?\1")
LINK = re.compile(r"!?\[(?:[^\[\]]|\[[^\]]*\])*\]\(\s*<?([^)\s>]*)>?(?:\s+\"[^\"]*\")?\s*\)")
REF_DEF = re.compile(r"^\s{0,3}\[[^\]]+\]:\s*<?(\S+?)>?(?:\s+.*)?$")
HTML_ANCHOR = re.compile(r"<a\s+[^>]*(?:name|id)\s*=\s*[\"']([^\"']+)[\"']", re.I)
SCHEME = re.compile(r"^[a-zA-Z][a-zA-Z0-9+.-]*:")


def slug(text):
    """GitHub's heading slug: strip markup, lowercase, keep letters, digits,
    spaces, '-' and '_', then spaces become '-'."""
    text = re.sub(r"!?\[([^\]]*)\]\([^)]*\)", r"\1", text)  # links keep their text
    text = re.sub(r"<[^>]+>", "", text)  # inline HTML tags
    out = []
    for ch in text.lower():
        cat = unicodedata.category(ch)
        if ch in " -_" or cat[0] in ("L", "N") or cat == "Mn":
            out.append(ch)
    return "".join(out).replace(" ", "-")


def strip_code_lines(lines):
    """Yield (line_no, text) for lines outside fenced code blocks."""
    fence = None
    for i, line in enumerate(lines, 1):
        m = FENCE.match(line)
        if fence:
            if m and m.group(1)[0] == fence[0] and len(m.group(1)) >= len(fence):
                fence = None
            continue
        if m:
            fence = m.group(1)
            continue
        yield i, line


_anchor_cache = {}


def anchors(path):
    if path in _anchor_cache:
        return _anchor_cache[path]
    with open(path, encoding="utf-8-sig") as f:
        lines = f.read().splitlines()
    seen = {}
    result = set()
    for _, line in strip_code_lines(lines):
        for a in HTML_ANCHOR.findall(line):
            result.add(a)
        m = HEADING.match(line)
        if not m:
            continue
        base = slug(m.group(2))
        n = seen.get(base, 0)
        seen[base] = n + 1
        result.add(base if n == 0 else f"{base}-{n}")
    _anchor_cache[path] = result
    return result


def links(path):
    with open(path, encoding="utf-8-sig") as f:
        lines = f.read().splitlines()
    for no, line in strip_code_lines(lines):
        m = REF_DEF.match(line)
        if m:
            yield no, m.group(1)
            continue
        for lm in LINK.finditer(INLINE_CODE.sub("", line)):
            yield no, lm.group(1)


def check_file(path, root):
    errors = []
    for no, target in links(path):
        if not target or SCHEME.match(target) or target.startswith("//"):
            continue
        file_part, _, anchor = target.partition("#")
        file_part = unquote(file_part.split("?")[0])
        if file_part:
            if file_part.startswith("/"):
                dest = os.path.join(root, file_part.lstrip("/"))
            else:
                dest = os.path.join(os.path.dirname(path), file_part)
            dest = os.path.normpath(dest)
            if not os.path.exists(dest):
                errors.append(f"{os.path.relpath(path, root)}:{no}: missing target {target}")
                continue
        else:
            dest = path
        if anchor and dest.lower().endswith(".md") and os.path.isfile(dest):
            # GitHub matches fragments case-insensitively; emphasis markers (* and
            # the leading/trailing punctuation) are already dropped by slug().
            if unquote(anchor).lower() not in {a.lower() for a in anchors(dest)}:
                errors.append(f"{os.path.relpath(path, root)}:{no}: missing anchor #{anchor} in {os.path.relpath(dest, root)}")
    return errors


def doc_files(root):
    files = [os.path.join(root, "README.md")]
    for d, _, names in os.walk(os.path.join(root, "docs")):
        files += [os.path.join(d, n) for n in names if n.endswith(".md")]
    return sorted(f for f in files if os.path.isfile(f))


def self_test():
    assert slug("Server mode") == "server-mode"
    assert slug("More nodes: join, promote, remove") == "more-nodes-join-promote-remove"
    assert slug("`serve` options and the LOCK file") == "serve-options-and-the-lock-file"
    assert slug("S3 in production: TLS, lifecycle and IAM") == "s3-in-production-tls-lifecycle-and-iam"
    assert slug("Sizing: threads, memory, disk") == "sizing-threads-memory-disk"
    assert slug("Spikes (evidence)") == "spikes-evidence"
    assert slug("Automatic backups to a directory (`--backup-url`)") == "automatic-backups-to-a-directory---backup-url"
    assert slug("snake_case name") == "snake_case-name"
    assert slug("[Link](x.md) text") == "link-text"
    import tempfile
    with tempfile.TemporaryDirectory() as d:
        os.makedirs(os.path.join(d, "docs"))
        with open(os.path.join(d, "README.md"), "w", encoding="utf-8") as f:
            f.write("# Top\n\n## Dup\n## Dup\n\n[ok](docs/a.md#there) [ok2](#dup-1) [web](https://x/#nope)\n"
                    "```\n[in code](missing.md)\n```\n`[inline](missing.md)`\n[bad](docs/nope.md) [badanchor](docs/a.md#nowhere)\n")
        with open(os.path.join(d, "docs", "a.md"), "w", encoding="utf-8") as f:
            f.write("# There\n[up](../README.md#top)\n")
        errs = [e for p in doc_files(d) for e in check_file(p, d)]
        assert len(errs) == 2, errs
        assert "missing target docs/nope.md" in errs[0] and "missing anchor #nowhere" in errs[1], errs
    print("check-doc-links self-test passed")


def main(argv):
    if "--self-test" in argv:
        self_test()
        return 0
    root = ROOT
    files = doc_files(root)
    errors = [e for p in files for e in check_file(p, root)]
    for e in errors:
        print(e)
    print(f"checked {len(files)} Markdown files: {len(errors)} broken link(s)")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
