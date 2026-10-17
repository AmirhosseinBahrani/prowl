#!/bin/sh
# Installs or upgrades PRowl on macOS and Linux.
#
#   curl -fsSL https://github.com/AmirhosseinBahrani/prowl/releases/latest/download/install.sh | sh
#   curl -fsSL …/install.sh | sh -s v0.1.0          # a specific version
#   curl -fsSL …/install.sh | sh -s -- --no-service # don't (re)start the background service
#
# Layout (everything in your home directory, no root needed):
#   ~/.local/share/prowl/versions/<version>/   the binaries of each installed version
#   ~/.local/bin/{prowl,prowld,prowld-service} symlinks to the current version
# The two most recent versions are kept, so an upgrade can be rolled back by re-running with
# the older version.
#
# Environment: PROWL_INSTALL_DIR (default ~/.local/share/prowl), PROWL_BIN_DIR (default
# ~/.local/bin), PROWL_INSTALL_ALLOW_SUDO=1 to install for root on purpose, PROWL_RELEASES_URL
# to download from a mirror of https://github.com/AmirhosseinBahrani/prowl/releases.
set -eu

repo="AmirhosseinBahrani/prowl"
releases="${PROWL_RELEASES_URL:-https://github.com/$repo/releases}"
version="latest"
service=1
for arg in "$@"; do
    case "$arg" in
        --no-service) service=0 ;;
        latest | v[0-9]* | [0-9]*) version="$arg" ;;
        -h | --help)
            echo "usage: install.sh [latest|VERSION] [--no-service]"
            exit 0
            ;;
        *)
            echo "usage: install.sh [latest|VERSION] [--no-service]" >&2
            exit 2
            ;;
    esac
done

red="" bold="" reset=""
if [ -t 2 ]; then
    red="$(printf '\033[31m')" bold="$(printf '\033[1m')" reset="$(printf '\033[0m')"
fi
say() { printf '%s\n' "$*"; }
fail() {
    printf '%sError: %s%s\n' "$red" "$*" "$reset" >&2
    exit 1
}

# Everything goes under $HOME; under sudo that would be root's home and `prowl` wouldn't be
# found from your own shell (the same reasoning as Claude Code's installer).
if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ] &&
    [ -z "${PROWL_INSTALL_ALLOW_SUDO:-}" ]; then
    fail "don't run this installer with sudo. PRowl installs into your home directory and runs as
you. Re-run the same command without sudo."
fi

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL -o "$2" "$1"; }
    resolve_latest() { curl -fsSLI -o /dev/null -w '%{url_effective}' "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
    resolve_latest() { wget -q --max-redirect=5 -S --spider "$1" 2>&1 | sed -n 's/^ *[Ll]ocation: *//p' | tail -n1 | tr -d '\r'; }
else
    fail "either curl or wget is required"
fi
command -v tar >/dev/null 2>&1 || fail "tar is required"

case "$(uname -s)" in
    Darwin) os=apple-darwin ;;
    Linux) os=unknown-linux-gnu ;;
    MINGW* | MSYS* | CYGWIN*) fail "on Windows, run in PowerShell: irm https://github.com/$repo/releases/latest/download/install.ps1 | iex" ;;
    *) fail "unsupported operating system: $(uname -s)" ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) fail "unsupported CPU architecture: $(uname -m)" ;;
esac
# An x86_64 shell under Rosetta on an Apple-silicon Mac: install the native build.
if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ] &&
    [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = 1 ]; then
    arch=aarch64
fi
if [ "$os" = unknown-linux-gnu ] &&
    { [ -f /lib/libc.musl-x86_64.so.1 ] || [ -f /lib/libc.musl-aarch64.so.1 ] || ldd /bin/ls 2>&1 | grep -q musl; }; then
    fail "musl-based Linux (e.g. Alpine) isn't supported yet; PRowl's Linux builds need glibc"
fi
target="$arch-$os"

if [ "$version" = latest ]; then
    url="$(resolve_latest "$releases/latest" || true)"
    version="${url##*/}"
    case "$version" in
        v[0-9]*) ;;
        *) fail "couldn't find the latest PRowl release (is GitHub reachable?)" ;;
    esac
fi
case "$version" in v*) ;; *) version="v$version" ;; esac

root="${PROWL_INSTALL_DIR:-$HOME/.local/share/prowl}"
bin_dir="${PROWL_BIN_DIR:-$HOME/.local/bin}"
case "$root/" in
    "$HOME/Desktop/"* | "$HOME/Documents/"* | "$HOME/Downloads/"*)
        [ "$os" = apple-darwin ] && fail "macOS doesn't let background services run programs from $root; choose another PROWL_INSTALL_DIR"
        ;;
esac

name="prowl-${version#v}-$target"
base="$releases/download/$version"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading PRowl $version ($target)…"
fetch "$base/$name.tar.gz" "$tmp/$name.tar.gz" || fail "PRowl $version has no build for $target"
fetch "$base/$name.tar.gz.sha256" "$tmp/sum" || fail "PRowl $version has no checksum for $name.tar.gz"
expected="$(cut -d' ' -f1 <"$tmp/sum")"
if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)"
else
    actual="$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1)"
fi
[ "$expected" = "$actual" ] || fail "checksum verification failed for $name.tar.gz"
tar -xzf "$tmp/$name.tar.gz" -C "$tmp"

# Unpack into versions/<version> and point the symlinks at it. Each step replaces atomically,
# so a daemon running the previous version is never left with a half-written file.
dest="$root/versions/$version"
mkdir -p "$root/versions" "$bin_dir"
rm -rf "$dest.tmp"
mkdir -p "$dest.tmp"
for bin in prowl prowld prowld-service; do
    cp "$tmp/$name/$bin" "$dest.tmp/$bin"
    chmod 0755 "$dest.tmp/$bin"
done
rm -rf "$dest"
mv "$dest.tmp" "$dest"
for bin in prowl prowld prowld-service; do
    ln -sfn "$dest/$bin" "$bin_dir/.$bin.tmp"
    mv -f "$bin_dir/.$bin.tmp" "$bin_dir/$bin"
done
say "Installed PRowl $version"

if [ "$service" = 1 ]; then
    say "Starting the background service…"
    # Run the versioned binary so the service points at this version's prowld-service.
    "$dest/prowld" service install >/dev/null ||
        fail "couldn't start the background service; run '$bin_dir/prowld service install' to see why"
fi

# Keep the current and the previous version.
ls -1t "$root/versions" 2>/dev/null | grep -v '\.tmp$' | tail -n +3 | while read -r old; do
    [ "$root/versions/$old" != "$dest" ] && rm -rf "${root:?}/versions/$old"
done

for tool in claude git; do
    command -v "$tool" >/dev/null 2>&1 || say "Note: PRowl reviews with '$tool', which isn't on your PATH yet."
done

say ""
case ":$PATH:" in
    *":$bin_dir:"*) say "✅ Done. Open PRowl with: ${bold}prowl${reset}" ;;
    *)
        case "${SHELL:-}" in
            */zsh) rc="$HOME/.zshrc" ;;
            */bash) rc="$HOME/.bashrc" ;;
            */fish) rc="$HOME/.config/fish/config.fish" ;;
            *) rc="$HOME/.profile" ;;
        esac
        say "✅ Done. $bin_dir isn't on your PATH yet; add it with:"
        if [ "${rc##*/}" = config.fish ]; then
            say "    fish_add_path $bin_dir"
        else
            say "    echo 'export PATH=\"$bin_dir:\$PATH\"' >> $rc && . $rc"
        fi
        say "then open PRowl with: ${bold}prowl${reset}"
        ;;
esac
