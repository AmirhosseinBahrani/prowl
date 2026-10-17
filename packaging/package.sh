#!/usr/bin/env bash
# Builds release binaries for one target and packages them into dist/:
#   prowl-<version>-<target>.tar.gz (+ .sha256), and on Linux also a .deb and an .rpm.
# Usage: packaging/package.sh <rust-target-triple> [version]
set -euo pipefail

target="${1:?usage: package.sh <target-triple> [version]}"
version="${2:-$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="prowld"))')}"
root="$(cd "$(dirname "$0")/.." && pwd)"
dist="$root/dist"
name="prowl-$version-$target"
stage="$dist/$name"
bins=(prowl prowld prowld-service)

checksum() {
    if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi
}

cd "$root"
cargo build --release --locked --target "$target" -p prowld -p prowl

rm -rf "$stage"
mkdir -p "$stage"
for bin in "${bins[@]}"; do
    cp "target/$target/release/$bin" "$stage/"
done
cp README.md "$stage/"
tar -C "$dist" -czf "$dist/$name.tar.gz" "$name"
(cd "$dist" && checksum "$name.tar.gz" > "$name.tar.gz.sha256")

case "$target" in
    *-linux-gnu)
        arch="$(case "$target" in x86_64-*) echo amd64;; aarch64-*) echo arm64;; *) echo "${target%%-*}";; esac)"
        deb="$dist/deb"
        rm -rf "$deb" && mkdir -p "$deb/DEBIAN" "$deb/usr/bin" "$deb/usr/share/doc/prowl"
        for bin in "${bins[@]}"; do install -m 0755 "$stage/$bin" "$deb/usr/bin/$bin"; done
        install -m 0644 README.md "$deb/usr/share/doc/prowl/README.md"
        cat > "$deb/DEBIAN/control" <<CONTROL
Package: prowl
Version: $version
Architecture: $arch
Maintainer: PRowl <noreply@github.com>
Depends: git, libdbus-1-3
Description: Reviews GitHub pull requests around the clock with your local claude CLI
 PRowl watches the repositories you choose for pull requests where you are a requested
 reviewer, reviews them with Claude against your review policy, and either posts the review
 or flags the pull request for your approval.
CONTROL
        dpkg-deb --build --root-owner-group "$deb" "$dist/prowl_${version}_${arch}.deb"
        (cd "$dist" && checksum "prowl_${version}_${arch}.deb" > "prowl_${version}_${arch}.deb.sha256")

        if command -v rpmbuild >/dev/null; then
            rpmarch="${target%%-*}"
            top="$dist/rpmbuild"
            rm -rf "$top" && mkdir -p "$top"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
            cat > "$top/SPECS/prowl.spec" <<SPEC
Name: prowl
Version: $version
Release: 1
Summary: Reviews GitHub pull requests around the clock with your local claude CLI
License: MIT OR Apache-2.0
Requires: git, dbus-libs
%description
PRowl watches the repositories you choose for pull requests where you are a requested
reviewer, reviews them with Claude against your review policy, and either posts the review
or flags the pull request for your approval.
%install
mkdir -p %{buildroot}/usr/bin
$(for bin in "${bins[@]}"; do echo "install -m 0755 $stage/$bin %{buildroot}/usr/bin/$bin"; done)
%files
$(for bin in "${bins[@]}"; do echo "/usr/bin/$bin"; done)
SPEC
            rpmbuild --define "_topdir $top" --target "$rpmarch" -bb "$top/SPECS/prowl.spec" >/dev/null
            rpm="$(find "$top/RPMS" -name '*.rpm' | head -n1)"
            cp "$rpm" "$dist/"
            (cd "$dist" && checksum "$(basename "$rpm")" > "$(basename "$rpm").sha256")
        fi
        ;;
esac

echo "packaged $name into $dist"
