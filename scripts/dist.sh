#!/bin/sh
# Build the Linux x86_64 release tarball and its sha256 into dist/:
#   dist/foxdb-<version>-x86_64-unknown-linux-gnu.tar.gz (+ .sha256)
# The binaries are built with the builder's home and checkout paths remapped, so no machine path
# or user name ends up in them. dist/ and target/dist are gitignored.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
target=x86_64-unknown-linux-gnu
name=foxdb-$version-$target.tar.gz
cargo_home=${CARGO_HOME:-$HOME/.cargo}

# The tree-sitter grammars are C/C++, so the C compiler needs the same mapping.
cmap="-ffile-prefix-map=$cargo_home=/cargo -ffile-prefix-map=$repo=/foxdb -ffile-prefix-map=$HOME=~"
RUSTFLAGS="--remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$repo=/foxdb --remap-path-prefix=$HOME=~" \
    CFLAGS="${CFLAGS:-} $cmap" CXXFLAGS="${CXXFLAGS:-} $cmap" CARGO_TARGET_DIR="$repo/target/dist" cargo build --release --locked --bin fox --bin foxd

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/skill" "$stage/scripts"
cp target/dist/release/fox target/dist/release/foxd "$stage/"
cp LICENSE README.md AGENTS-SETUP.md CHANGELOG.md config.example.toml "$stage/"
cp skill/SKILL.md "$stage/skill/"
cp scripts/install.sh "$stage/scripts/"
cp -R docs "$stage/docs"

if strings "$stage/fox" "$stage/foxd" | grep -qF "$HOME"; then
    echo "dist.sh: a binary still contains $HOME" >&2
    exit 1
fi

mkdir -p dist
tar -C "$stage" --owner=0 --group=0 --numeric-owner -czf "dist/$name" .
(cd dist && sha256sum "$name" >"$name.sha256")
cat "dist/$name.sha256"
