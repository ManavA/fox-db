#!/bin/sh
# Install fox and foxd into ${FOXDB_HOME:-$HOME/.foxdb}/bin (and the agent skill into
# ${FOXDB_HOME:-$HOME/.foxdb}/skill), without root.
#
#   scripts/install.sh                 download the release asset (curl or wget, no login), else build from source
#   scripts/install.sh --source        always build from source (needs cargo)
#   scripts/install.sh --tarball FILE  install from a downloaded release tarball (checks FILE.sha256 if present)
#
# Environment: FOXDB_HOME (default ~/.foxdb), FOXDB_VERSION (default 0.2.2),
# FOXDB_REPO (default ManavA/foxdb), FOXDB_RELEASE_URL (default
# https://github.com/$FOXDB_REPO/releases/download/v$FOXDB_VERSION). No GitHub login is
# needed. It never edits any harness setting (Claude Code, Codex, opencode, T3 Code):
# registering the MCP server and hooks is a separate, explicit step.
set -eu

VERSION=${FOXDB_VERSION:-0.2.2}
REPO=${FOXDB_REPO:-ManavA/foxdb}
DEST=${FOXDB_HOME:-$HOME/.foxdb}/bin
TARGET=x86_64-unknown-linux-gnu
ASSET=foxdb-$VERSION-$TARGET.tar.gz
RELEASE_URL=${FOXDB_RELEASE_URL:-https://github.com/$REPO/releases/download/v$VERSION}

mode=auto
tarball=
skill=
while [ $# -gt 0 ]; do
    case $1 in
        --source) mode=source ;;
        --tarball) mode=tarball; tarball=${2:?--tarball needs a file}; shift ;;
        -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
        *) echo "install.sh: unknown argument $1" >&2; exit 2 ;;
    esac
    shift
done

if [ "$(id -u)" = 0 ]; then
    echo "install.sh: run as your own user, not root" >&2
    exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

say() { echo "install.sh: $*" >&2; }

# Unpack a release tarball (after checking its .sha256 when one sits beside it) into $work/out.
from_tarball() {
    if [ -f "$1.sha256" ]; then
        want=$(cut -d' ' -f1 <"$1.sha256")
        got=$(sha256sum "$1" | cut -d' ' -f1)
        [ "$want" = "$got" ] || { say "sha256 mismatch for $1"; return 1; }
        say "sha256 ok"
    fi
    mkdir -p "$work/out"
    tar -xzf "$1" -C "$work/out"
    src=$(dirname "$(find "$work/out" -type f -name fox | head -n1)")
    [ -x "$src/fox" ] && [ -x "$src/foxd" ] || { say "tarball has no fox/foxd"; return 1; }
    skill=$src/skill/SKILL.md
    "$src/fox" --version >/dev/null 2>&1 || { say "the prebuilt fox does not run here (it needs glibc 2.39 or newer)"; return 1; }
}

# fetch URL FILE: anonymous download with curl, else wget.
fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 2 -o "$2" "$1"
    else
        wget -q -O "$2" "$1"
    fi
}

from_release() {
    [ "$(uname -s)-$(uname -m)" = Linux-x86_64 ] || { say "no prebuilt binary for $(uname -s)-$(uname -m)"; return 1; }
    say "downloading v$VERSION from $RELEASE_URL"
    if command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1; then
        { fetch "$RELEASE_URL/$ASSET" "$work/$ASSET" && fetch "$RELEASE_URL/$ASSET.sha256" "$work/$ASSET.sha256"; } \
            || { say "could not download $RELEASE_URL/$ASSET"; return 1; }
    elif command -v gh >/dev/null 2>&1; then
        gh release download "v$VERSION" -R "$REPO" -p "$ASSET" -p "$ASSET.sha256" -D "$work" >/dev/null 2>&1 \
            || { say "no release asset v$VERSION/$ASSET"; return 1; }
    else
        say "neither curl, wget nor gh found"; return 1
    fi
    [ -s "$work/$ASSET.sha256" ] || { say "no checksum for $ASSET"; return 1; }
    from_tarball "$work/$ASSET"
}

from_source() {
    command -v cargo >/dev/null 2>&1 || { say "cargo not found: install Rust from https://rustup.rs"; return 1; }
    here=$(cd "$(dirname "$0")/.." && pwd)
    if [ -f "$here/Cargo.toml" ] && grep -q '^name = "foxdb"' "$here/Cargo.toml"; then
        checkout=$here
    else
        checkout=$work/foxdb
        git clone --quiet --depth 1 --branch "v$VERSION" "https://github.com/$REPO.git" "$checkout" >&2
    fi
    say "building from source in $checkout (a few minutes the first time)"
    (cd "$checkout" && cargo build --release --locked --bin fox --bin foxd >&2)
    dir=${CARGO_TARGET_DIR:-$checkout/target}
    case $dir in /*) ;; *) dir=$checkout/$dir ;; esac
    src=$dir/release
    skill=$checkout/skill/SKILL.md
}

case $mode in
    tarball) from_tarball "$tarball" ;;
    source) from_source ;;
    auto) from_release || { say "falling back to a source build"; from_source; } ;;
esac

mkdir -p "$DEST"
for b in fox foxd; do
    cp "$src/$b" "$DEST/$b.new"
    chmod 755 "$DEST/$b.new"
    mv -f "$DEST/$b.new" "$DEST/$b"
done
# The agent skill is kept beside the binaries; copying it into a harness is a separate step.
if [ -f "$skill" ]; then
    mkdir -p "$DEST/../skill"
    cp "$skill" "$DEST/../skill/SKILL.md"
fi
echo "installed $("$DEST/fox" --version) to $DEST"
case :$PATH: in
    *:"$DEST":*) ;;
    *) echo "add it to PATH: export PATH=\"$DEST:\$PATH\"" ;;
esac
