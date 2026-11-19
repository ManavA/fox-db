#!/bin/sh
# Build a release tarball and its sha256 into dist/:
#   dist/foxdb-<version>-<target>.tar.gz (+ .sha256)
#
#   scripts/dist.sh            for this machine's target (rustc's host)
#   scripts/dist.sh TARGET     for TARGET (the rustup target must be installed), e.g.
#                              x86_64-apple-darwin on an Apple silicon Mac
#
# The release workflow (.github/workflows/release.yml) runs this for every released target.
# The binaries are built with the builder's home and checkout paths remapped, so no machine path
# or user name ends up in them. dist/ and target/dist are gitignored.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
target=${1:-$(rustc -vV | sed -n 's/^host: //p')}
name=foxdb-$version-$target.tar.gz
cargo_home=${CARGO_HOME:-$HOME/.cargo}

# The tree-sitter grammars are C/C++, so the C compiler needs the same mapping.
cmap="-ffile-prefix-map=$cargo_home=/cargo -ffile-prefix-map=$repo=/foxdb -ffile-prefix-map=$HOME=~"
RUSTFLAGS="--remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$repo=/foxdb --remap-path-prefix=$HOME=~" \
    CFLAGS="${CFLAGS:-} $cmap" CXXFLAGS="${CXXFLAGS:-} $cmap" CARGO_TARGET_DIR="$repo/target/dist" \
    cargo build --release --locked --target "$target" --bin fox --bin foxd
bin=target/dist/$target/release

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/skill" "$stage/scripts"
cp "$bin/fox" "$bin/foxd" "$stage/"
cp LICENSE README.md AGENTS-SETUP.md CHANGELOG.md config.example.toml "$stage/"
cp skill/SKILL.md "$stage/skill/"
cp scripts/install.sh "$stage/scripts/"
cp -R docs "$stage/docs"

if strings "$stage/fox" "$stage/foxd" | grep -qF "$HOME"; then
    echo "dist.sh: a binary still contains $HOME" >&2
    exit 1
fi

# GNU tar on Linux, bsdtar on macOS: both store every entry as owned by root (and bsdtar adds no
# AppleDouble ._ files with COPYFILE_DISABLE).
export COPYFILE_DISABLE=1
if tar --version 2>/dev/null | grep -q GNU; then
    owner="--owner=0 --group=0 --numeric-owner"
else
    owner="--uid 0 --gid 0 --numeric-owner"
fi
mkdir -p dist
# shellcheck disable=SC2086 # $owner is a list of flags
tar -C "$stage" $owner -czf "dist/$name" .
if command -v sha256sum >/dev/null 2>&1; then
    (cd dist && sha256sum "$name" >"$name.sha256")
else
    (cd dist && shasum -a 256 "$name" >"$name.sha256")
fi
cat "dist/$name.sha256"
