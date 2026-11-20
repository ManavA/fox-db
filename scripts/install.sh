#!/bin/sh
# Install fox and foxd into ${FOXDB_HOME:-$HOME/.foxdb}/bin (and the agent skill into
# ${FOXDB_HOME:-$HOME/.foxdb}/skill), without root.
#
#   scripts/install.sh                 download this machine's release asset (curl or wget, no login),
#                                      check its sha256, else build from source
#   scripts/install.sh --source        always build from source (needs cargo)
#   scripts/install.sh --tarball FILE  install from a downloaded release tarball (checks FILE.sha256 if present)
#
# Prebuilt assets: Linux (glibc) and macOS, each on x86_64 and arm64. Environment: FOXDB_HOME
# (default ~/.foxdb), FOXDB_VERSION (default 0.2.4), FOXDB_REPO (default ManavA/foxdb),
# FOXDB_RELEASE_URL (default https://github.com/$FOXDB_REPO/releases/download/v$FOXDB_VERSION).
# No GitHub login is needed. It never edits any harness setting (Claude Code, Codex, opencode,
# T3 Code): registering the MCP server and hooks is a separate, explicit step.
set -eu

VERSION=${FOXDB_VERSION:-0.2.4}
REPO=${FOXDB_REPO:-ManavA/foxdb}
DEST=${FOXDB_HOME:-$HOME/.foxdb}/bin
RELEASE_URL=${FOXDB_RELEASE_URL:-https://github.com/$REPO/releases/download/v$VERSION}

# The header above, repeated because $0 is the shell, not this file, when piped into sh.
usage() {
    cat <<'USAGE'
Install fox and foxd into ${FOXDB_HOME:-$HOME/.foxdb}/bin (and the agent skill into
${FOXDB_HOME:-$HOME/.foxdb}/skill), without root.

  install.sh                 download this machine's release asset (curl or wget, no login),
                             check its sha256, else build from source
  install.sh --source        always build from source (needs cargo)
  install.sh --tarball FILE  install from a downloaded release tarball (checks FILE.sha256 if present)

Prebuilt assets: Linux (glibc) and macOS, each on x86_64 and arm64. Environment: FOXDB_HOME
(default ~/.foxdb), FOXDB_VERSION, FOXDB_REPO (default ManavA/foxdb), FOXDB_RELEASE_URL.
No GitHub login is needed. It never edits any harness setting.
USAGE
}

mode=auto
tarball=
skill=
while [ $# -gt 0 ]; do
    case $1 in
        --source) mode=source ;;
        --tarball) mode=tarball; tarball=${2:?--tarball needs a file}; shift ;;
        -h|--help) usage; exit 0 ;;
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

# The release target for this machine; empty when there is no prebuilt binary for it.
host_target() {
    os=$(uname -s)
    arch=$(uname -m)
    # A shell under Rosetta on Apple silicon reports x86_64; install the native arm64 build.
    if [ "$os-$arch" = Darwin-x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
        arch=arm64
    fi
    case $os-$arch in
        Linux-x86_64 | Linux-amd64) echo x86_64-unknown-linux-gnu ;;
        Linux-aarch64 | Linux-arm64) echo aarch64-unknown-linux-gnu ;;
        Darwin-x86_64) echo x86_64-apple-darwin ;;
        Darwin-arm64 | Darwin-aarch64) echo aarch64-apple-darwin ;;
    esac
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

# Unpack a release tarball (after checking its .sha256 when one sits beside it) into $work/out.
# Returns 3 on a sha256 mismatch and 1 on any other failure. Each step checks its own status,
# because set -e does not apply inside a function called left of ||.
from_tarball() {
    if [ -f "$1.sha256" ]; then
        want=$(cut -d' ' -f1 <"$1.sha256")
        got=$(sha256 "$1")
        [ "$want" = "$got" ] || { say "sha256 mismatch for $1"; return 3; }
        say "sha256 ok"
    fi
    mkdir -p "$work/out" || return 1
    tar -xzf "$1" -C "$work/out" || { say "could not unpack $1"; return 1; }
    found=$(find "$work/out" -type f -name fox | head -n1)
    [ -n "$found" ] || { say "tarball has no fox/foxd"; return 1; }
    src=$(dirname "$found")
    [ -x "$src/fox" ] && [ -x "$src/foxd" ] || { say "tarball has no fox/foxd"; return 1; }
    skill=$src/skill/SKILL.md
    "$src/fox" --version >/dev/null 2>&1 || { say "the prebuilt fox does not run here (on Linux it needs glibc 2.35 or newer)"; return 1; }
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
    target=$(host_target)
    [ -n "$target" ] || { say "no prebuilt binary for $(uname -s)-$(uname -m)"; return 1; }
    ASSET=foxdb-$VERSION-$target.tar.gz
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
        git clone --quiet --depth 1 --branch "v$VERSION" "https://github.com/$REPO.git" "$checkout" >&2 || return 1
    fi
    say "building from source in $checkout (a few minutes the first time)"
    (cd "$checkout" && cargo build --release --locked --bin fox --bin foxd >&2) || return 1
    dir=${CARGO_TARGET_DIR:-$checkout/target}
    case $dir in /*) ;; *) dir=$checkout/$dir ;; esac
    src=$dir/release
    skill=$checkout/skill/SKILL.md
}

case $mode in
    tarball) from_tarball "$tarball" ;;
    source) from_source ;;
    auto)
        # A sha256 mismatch stops the install; any other failure falls back to a source build.
        rc=0
        from_release || rc=$?
        if [ $rc = 3 ]; then
            say "not installing: the downloaded asset does not match its sha256"
            exit 1
        elif [ $rc != 0 ]; then
            say "falling back to a source build"
            from_source
        fi
        ;;
esac
[ -n "${src:-}" ] && [ -x "$src/fox" ] && [ -x "$src/foxd" ] || { say "no fox/foxd to install"; exit 1; }

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
