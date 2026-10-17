#!/usr/bin/env python3
"""Fills the Homebrew formula and Scoop manifest templates with a release's version, URLs and
checksums (read from the *.sha256 files in dist/). Usage: render.py <version> <repo> <dist>"""
import pathlib
import sys

version, repo, dist = sys.argv[1], sys.argv[2], pathlib.Path(sys.argv[3])
here = pathlib.Path(__file__).parent
base = f"https://github.com/{repo}/releases/download/v{version}"


def sha(target, ext):
    path = dist / f"prowl-{version}-{target}.{ext}.sha256"
    return path.read_text().split()[0] if path.exists() else "MISSING"


values = {
    "VERSION": version,
    "BASE_URL": base,
    "SHA_MAC_ARM": sha("aarch64-apple-darwin", "tar.gz"),
    "SHA_MAC_INTEL": sha("x86_64-apple-darwin", "tar.gz"),
    "SHA_LINUX_X64": sha("x86_64-unknown-linux-gnu", "tar.gz"),
    "SHA_LINUX_ARM": sha("aarch64-unknown-linux-gnu", "tar.gz"),
    "SHA_WINDOWS": sha("x86_64-pc-windows-msvc", "zip"),
}
for template, out in [
    (here / "homebrew" / "prowl.rb.in", dist / "prowl.rb"),
    (here / "scoop" / "prowl.json.in", dist / "prowl.json"),
]:
    text = template.read_text()
    for key, value in values.items():
        text = text.replace("{{" + key + "}}", value)
    out.write_text(text)
    print(f"wrote {out}")
