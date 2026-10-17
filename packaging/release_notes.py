#!/usr/bin/env python3
"""Writes the GitHub release page for a version: its CHANGELOG.md section, how to install, and
a download table with checksums (read from the *.sha256 files in dist/, when present).
Usage: release_notes.py <version> <repo> [dist] > notes.md"""
import pathlib
import re
import sys

version, repo = sys.argv[1].lstrip("v"), sys.argv[2]
dist = pathlib.Path(sys.argv[3]) if len(sys.argv) > 3 else None
root = pathlib.Path(__file__).resolve().parent.parent
base = f"https://github.com/{repo}/releases/download/v{version}"


def changelog_section():
    text = (root / "CHANGELOG.md").read_text()
    match = re.search(
        rf"^## \[{re.escape(version)}\][^\n]*\n(.*?)(?=^## \[|\Z)", text, re.M | re.S
    )
    if not match:
        sys.exit(f"CHANGELOG.md has no section for [{version}]")
    section = re.sub(r"^\[[^\]]+\]: .*$", "", match.group(1), flags=re.M)  # link references
    # Demote headings so they sit under the release title.
    return re.sub(r"^### ", "#### ", section.strip(), flags=re.M)


def sha(name):
    if dist is None:
        return None
    path = dist / f"{name}.sha256"
    return path.read_text().split()[0] if path.exists() else None


platforms = [
    ("macOS (Apple silicon)", f"prowl-{version}-aarch64-apple-darwin.tar.gz"),
    ("macOS (Intel)", f"prowl-{version}-x86_64-apple-darwin.tar.gz"),
    ("Linux (x86_64)", f"prowl-{version}-x86_64-unknown-linux-gnu.tar.gz"),
    ("Linux (ARM64)", f"prowl-{version}-aarch64-unknown-linux-gnu.tar.gz"),
    ("Windows (x64)", f"prowl-{version}-x86_64-pc-windows-msvc.zip"),
    ("Debian / Ubuntu (amd64)", f"prowl_{version}_amd64.deb"),
    ("Debian / Ubuntu (arm64)", f"prowl_{version}_arm64.deb"),
    ("Fedora / RHEL (x86_64)", f"prowl-{version}-1.x86_64.rpm"),
    ("Fedora / RHEL (aarch64)", f"prowl-{version}-1.aarch64.rpm"),
]
rows = []
for label, name in platforms:
    checksum = sha(name)
    if dist is not None and checksum is None:
        continue  # not built for this release
    link = f"[`{name}`]({base}/{name})"
    rows.append(f"| {label} | {link} | `{checksum}` |" if checksum else f"| {label} | {link} |")
header = "| Platform | File | SHA-256 |\n|---|---|---|" if dist else "| Platform | File |\n|---|---|"

print(f"""{changelog_section()}

### Install

**macOS and Linux**

```sh
curl -fsSL {base}/install.sh | sh
```

**Windows** (PowerShell)

```powershell
irm {base}/install.ps1 | iex
```

Already installed? Run the same command to upgrade. Then open PRowl with `prowl`. New to PRowl?
Start with the [quick start](https://github.com/{repo}#quick-start).

### Downloads

{header}
{chr(10).join(rows)}

Also attached: a Homebrew formula (`prowl.rb`, `brew install ./prowl.rb`) and a Scoop manifest
(`prowl.json`). Each file has a `.sha256` next to it.""")
