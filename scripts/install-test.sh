#!/usr/bin/env bash
# Install test: scripts/install.sh picks the release asset for each platform and checks its
# sha256. It runs install.sh against fake release tarballs (fox and foxd are shell scripts that
# print their target) in a file:// directory, with `uname`, `sysctl` and `cargo` replaced by
# stubs, in a scratch HOME; the cargo stub fails, so a refused download cannot build from source.
# Takes a second; no network. Run by scripts/docs-test.sh.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -n1)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/foxdb-install-test.XXXXXX")
trap 'rm -rf "$scratch"' EXIT

sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi; }

# The fake release: one tarball and .sha256 per target, laid out as scripts/dist.sh lays it out.
release=$scratch/release
mkdir -p "$release"
for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin; do
    stage=$scratch/stage-$target
    mkdir -p "$stage/skill"
    for b in fox foxd; do
        printf '#!/bin/sh\necho "%s %s %s"\n' "$b" "$version" "$target" >"$stage/$b"
        chmod 755 "$stage/$b"
    done
    echo "skill for $target" >"$stage/skill/SKILL.md"
    name=foxdb-$version-$target.tar.gz
    tar -C "$stage" -czf "$release/$name" .
    (cd "$release" && sha256 "$name" >"$name.sha256")
done

# Only the basic tools, and a `cargo` stub that fails, so a failed prebuilt install fails
# instead of building from source.
base_path=/usr/bin:/bin
PATH=$base_path command -v curl >/dev/null 2>&1 || { echo "SKIP install-test: no curl in $base_path"; exit 0; }

n=0
# run OS ARCH ARM64 [release dir]: install.sh with `uname -s` = OS, `uname -m` = ARCH and
# `sysctl -n hw.optional.arm64` = ARM64, in scratch HOME home-$n (callers count n before the
# command substitution, which would lose an increment made in here); prints install.sh's output.
run() {
    local stub=$scratch/stub-$n home=$scratch/home-$n
    mkdir -p "$stub" "$home"
    # shellcheck disable=SC2016 # $1 belongs to the stub
    printf '#!/bin/sh\ncase "$1" in -s) echo %s ;; -m) echo %s ;; *) echo %s ;; esac\n' "$1" "$2" "$1" >"$stub/uname"
    printf '#!/bin/sh\necho %s\n' "$3" >"$stub/sysctl"
    printf '#!/bin/sh\ntouch "%s/cargo-ran"\necho "cargo stub: no source builds here" >&2\nexit 1\n' "$stub" >"$stub/cargo"
    chmod 755 "$stub/uname" "$stub/sysctl" "$stub/cargo"
    if [ "${piped:-}" = 1 ]; then
        # As `curl -fsSL .../install.sh | sh` runs it: from stdin, outside any checkout.
        (cd "$home" && env -i HOME="$home" PATH="$stub:$base_path" FOXDB_RELEASE_URL="file://${4:-$release}" \
            sh <"$repo/scripts/install.sh" 2>&1)
    else
        # In $cwd when set (the directory the user runs it from).
        (cd "${cwd:-.}" && env -i HOME="$home" PATH="$stub:$base_path" FOXDB_RELEASE_URL="file://${4:-$release}" \
            sh "$repo/scripts/install.sh" 2>&1)
    fi
}

status=0
fail() { echo "FAIL install-test: $*"; status=1; }

# expect_target OS ARCH ARM64 TARGET: installs and the installed fox is TARGET's.
expect_target() {
    local out
    n=$((n + 1))
    if ! out=$(run "$1" "$2" "$3"); then fail "$1 $2: install failed: $out"; return; fi
    local home=$scratch/home-$n
    [ "$("$home/.foxdb/bin/fox")" = "fox $version $4" ] || fail "$1 $2: installed $("$home/.foxdb/bin/fox"), want $4"
    [ "$("$home/.foxdb/bin/foxd")" = "foxd $version $4" ] || fail "$1 $2: foxd is not $4's"
    grep -qF "skill for $4" "$home/.foxdb/skill/SKILL.md" || fail "$1 $2: skill is not $4's"
    printf '%s' "$out" | grep -q 'sha256 ok' || fail "$1 $2: no sha256 check: $out"
}

expect_target Linux x86_64 0 x86_64-unknown-linux-gnu
expect_target Linux aarch64 0 aarch64-unknown-linux-gnu
expect_target Linux arm64 0 aarch64-unknown-linux-gnu
expect_target Darwin arm64 1 aarch64-apple-darwin
expect_target Darwin x86_64 0 x86_64-apple-darwin
# A Rosetta shell on Apple silicon reports x86_64; the native arm64 build is the one to install.
expect_target Darwin x86_64 1 aarch64-apple-darwin
# Piped into sh, as the README's one-line install runs it.
piped=1 expect_target Linux x86_64 0 x86_64-unknown-linux-gnu

# A platform without a prebuilt binary says so (then the stubbed source build fails).
n=$((n + 1))
if out=$(run FreeBSD amd64 0); then
    fail "FreeBSD amd64: install.sh succeeded without a binary"
else
    printf '%s' "$out" | grep -qF 'no prebuilt binary for FreeBSD-amd64' || fail "FreeBSD amd64: $out"
fi

# A tarball whose sha256 does not match is refused, and the install stops there: no source build.
bad=$scratch/bad
mkdir -p "$bad"
cp "$release"/* "$bad/"
name=foxdb-$version-aarch64-unknown-linux-gnu.tar.gz
echo "0000000000000000000000000000000000000000000000000000000000000000  $name" >"$bad/$name.sha256"
n=$((n + 1))
if out=$(run Linux aarch64 0 "$bad"); then
    fail "a bad sha256 was installed"
else
    printf '%s' "$out" | grep -qF "sha256 mismatch" || fail "bad sha256: $out"
    [ ! -e "$scratch/home-$n/.foxdb/bin/fox" ] || fail "bad sha256: fox was installed"
    [ ! -e "$scratch/stub-$n/cargo-ran" ] || fail "bad sha256: fell back to a source build"
fi

# A tarball that does not unpack (its sha256 matches) installs nothing, not even the fox and
# foxd of the directory install.sh runs in.
corrupt=$scratch/corrupt
mkdir -p "$corrupt" "$scratch/cwd"
cp "$release"/* "$corrupt/"
name=foxdb-$version-x86_64-unknown-linux-gnu.tar.gz
echo "not a tarball" >"$corrupt/$name"
(cd "$corrupt" && sha256 "$name" >"$name.sha256")
for b in fox foxd; do printf '#!/bin/sh\necho "%s from the working directory"\n' "$b" >"$scratch/cwd/$b"; done
chmod 755 "$scratch/cwd/fox" "$scratch/cwd/foxd"
n=$((n + 1))
if out=$(cwd=$scratch/cwd run Linux x86_64 0 "$corrupt"); then
    fail "a tarball that does not unpack was installed: $out"
else
    [ ! -e "$scratch/home-$n/.foxdb/bin/fox" ] || fail "corrupt tarball: installed $("$scratch/home-$n/.foxdb/bin/fox")"
fi

# A tarball without fox installs nothing either.
empty=$scratch/empty
mkdir -p "$empty" "$scratch/stage-empty/skill"
cp "$release"/* "$empty/"
echo skill >"$scratch/stage-empty/skill/SKILL.md"
tar -C "$scratch/stage-empty" -czf "$empty/$name" .
(cd "$empty" && sha256 "$name" >"$name.sha256")
n=$((n + 1))
if out=$(cwd=$scratch/cwd run Linux x86_64 0 "$empty"); then
    fail "a tarball without fox was installed: $out"
else
    printf '%s' "$out" | grep -qF "tarball has no fox/foxd" || fail "no fox: $out"
    [ ! -e "$scratch/home-$n/.foxdb/bin/fox" ] || fail "no fox: installed $("$scratch/home-$n/.foxdb/bin/fox")"
fi

# -h works when piped into sh, where $0 is the shell, not the script.
help=$(sh -s -- -h <"$repo/scripts/install.sh" 2>&1) || true
printf '%s' "$help" | grep -qF -- "--tarball FILE" || fail "piped -h: $help"

[ $status = 0 ] && echo "PASS install.sh: picks and checks the asset for 4 targets ($n runs)"
exit $status
