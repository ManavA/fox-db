#!/usr/bin/env bash
# Docs test: run every fenced block opened with "```sh runnable" in README.md and
# AGENTS-SETUP.md, in order, as one bash script per file (set -euo pipefail), each in its own
# scratch HOME with FOXDB_HOME=$HOME/.foxdb and a small fixture project at ~/src/my-app.
# A block tagged "needs=<cmd>" is skipped (and reported) when <cmd> is not installed.
# scripts/install-test.sh checks that install.sh picks and verifies each platform's asset.
# Then, when dist/ holds this machine's release tarball, it installs that with scripts/install.sh --tarball
# and through the download path (curl against a file:// URL). Nothing here uses the network: the
# doc runs point FOXDB_RELEASE_URL at a missing file:// directory, so they build from source.
#
# Usage: scripts/docs-test.sh [--keep]        (--keep leaves the scratch dir for inspection)
# The real home is never written: HOME points into the scratch dir, and the script fails if the
# user's Claude Code or Codex settings files change while it runs.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
keep=${1:-}
real_home=$HOME
scratch=$(mktemp -d "${TMPDIR:-/tmp}/foxdb-docs-test.XXXXXX")
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -n1)

# Tool PATH for the blocks: the caller's PATH without any foxdb bin dir, so `fox` resolves only
# once a block has installed it and put ~/.foxdb/bin first.
clean_path=$(printf '%s' "$PATH" | tr ':' '\n' | grep -v '\.foxdb' | paste -sd: -)
cargo_home=${CARGO_HOME:-$real_home/.cargo}
rustup_home=${RUSTUP_HOME:-$real_home/.rustup}

# Fingerprints of the user's harness settings, checked again at the end.
watched=("$real_home/.claude/settings.json" "$real_home/.codex/config.toml" "$real_home/.codex/hooks.json")
fingerprint() { for f in "${watched[@]}"; do [ -f "$f" ] && cksum "$f" || echo "absent $f"; done; }
before=$(fingerprint)

cleanup() {
    # Stop anything still running from the scratch install (background index runs, foxd).
    pkill -f "$scratch/" 2>/dev/null || true
    if [ "$keep" = --keep ]; then echo "kept $scratch"; else rm -rf "$scratch"; fi
}
trap cleanup EXIT

make_fixture() {
    local p=$1/src/my-app
    mkdir -p "$p"
    cat >"$p/app.py" <<'EOF'
"""A tiny app used by the foxdb docs test."""


def load_config(path):
    """Read the settings file and return a dict."""
    with open(path) as f:
        return dict(line.split("=", 1) for line in f if "=" in line)


def main():
    cfg = load_config("app.cfg")
    print(cfg)
EOF
    printf '# My app\n\n## Configuration\n\nSettings live in app.cfg and are read by load_config.\n' >"$p/README.md"
    git -C "$p" init -q
    git -C "$p" add -A
    git -C "$p" -c user.name=docs -c user.email=docs@example.invalid commit -qm fixture
}

# Turn a doc's runnable blocks into one script.
extract() {
    awk '
        /^```sh runnable/ {
            inb = 1; n++; needs = ""
            if (match($0, /needs=[A-Za-z0-9_-]+/)) needs = substr($0, RSTART + 6, RLENGTH - 6)
            printf "echo \"--- block %d (line %d)%s\"\n", n, NR, needs ? " needs " needs : ""
            if (needs) printf "if command -v %s >/dev/null 2>&1; then\n", needs
            next
        }
        inb && /^```/ {
            inb = 0
            if (needs) printf "else echo \"SKIPPED block %d: %s not installed\"\nfi\n", n, needs
            next
        }
        inb { print }
    ' "$1"
}

# run_doc NAME FILE: runs FILE's blocks in a fresh scratch home; the log goes to $scratch/NAME.log.
run_doc() {
    local name=$1 doc=$2 home=$scratch/$1/home script=$scratch/$1.sh log=$scratch/$1.log
    mkdir -p "$home"
    make_fixture "$home"
    {
        echo "[ \"\$HOME\" = '$home' ] || { echo 'docs-test: HOME is not the scratch home' >&2; exit 99; }"
        extract "$repo/$doc"
    } >"$script"
    local blocks
    blocks=$(grep -c '^```sh runnable' "$repo/$doc")
    if (cd "$repo" && env -i HOME="$home" FOXDB_HOME="$home/.foxdb" PATH="$clean_path" \
        CARGO_HOME="$cargo_home" RUSTUP_HOME="$rustup_home" LANG=C.UTF-8 TERM=dumb \
        FOXDB_RELEASE_URL="file://$scratch/no-release" bash -euo pipefail "$script") >"$log" 2>&1; then
        echo "PASS $doc: $blocks runnable blocks, $(grep -c '^SKIPPED' "$log" || true) skipped"
    else
        echo "FAIL $doc (exit $?), last block: $(grep '^--- block' "$log" | tail -n1)"
        tail -n 25 "$log"
        return 1
    fi
}

# expect NAME TEXT: the doc's output must contain TEXT.
expect() {
    grep -qF -- "$2" "$scratch/$1.log" || { echo "FAIL $1: output lacks: $2"; tail -n 25 "$scratch/$1.log"; return 1; }
}

status=0
bash "$repo/scripts/install-test.sh" || status=1
run_doc readme README.md || status=1
if [ $status = 0 ]; then
    expect readme "fox $version" && expect readme "function load_config  app.py:4" \
        && expect readme "callers of ~c:" || status=1
fi

run_doc agents AGENTS-SETUP.md || status=1
if [ $status = 0 ]; then
    for t in "fox $version" '"name":"fox_find"' '"name":"fox_graph"' '"name":"fox_memory"' \
        '"name":"fox_read"' '{"hookSpecificOutput":{"additionalContext":"foxdb · my-app'; do
        expect agents "$t" || status=1
    done
    if command -v claude >/dev/null 2>&1; then expect agents "Connected" || status=1; fi
    if command -v jq >/dev/null 2>&1; then expect agents "SubagentStop" || status=1; fi
    [ ! -e "$scratch/agents/home/.foxdb" ] || { echo "FAIL AGENTS-SETUP.md: uninstall left ~/.foxdb"; status=1; }
fi

# The plugin: each installed harness accepts it from this checkout (offline, in a scratch home).
if command -v claude >/dev/null 2>&1; then
    home=$scratch/plugin-claude/home
    mkdir -p "$home"
    if out=$(cd "$repo" && env -i HOME="$home" PATH="$clean_path" claude plugin validate plugin 2>&1 \
        && env -i HOME="$home" PATH="$clean_path" claude plugin validate . 2>&1) \
        && [ "$(printf '%s' "$out" | grep -c 'Validation passed')" = 2 ]; then
        echo "PASS claude plugin validate (plugin and marketplace)"
    else
        echo "FAIL claude plugin validate: $out"
        status=1
    fi
fi
if command -v codex >/dev/null 2>&1; then
    home=$scratch/plugin-codex/home
    mkdir -p "$home/.codex"
    cx() { env -i HOME="$home" CODEX_HOME="$home/.codex" PATH="$clean_path" codex "$@" 2>&1; }
    if out=$(cx plugin marketplace add "$repo" && cx plugin add foxdb@foxdb && cx mcp get foxdb --json) \
        && printf '%s' "$out" | tr -d ' \n' | grep -q '"env_vars":\["FOXDB_AGENT","FOXDB_PARENT","FOXDB_HOME","FOXDB_SESSION"\]'; then
        echo "PASS codex plugin add foxdb@foxdb"
    else
        echo "FAIL codex plugin: $out"
        status=1
    fi
fi

# This machine's release tarball, when scripts/dist.sh has built one.
tarball=$repo/dist/foxdb-$version-$(rustc -vV | sed -n 's/^host: //p').tar.gz
if [ -f "$tarball" ]; then
    home=$scratch/tarball/home
    mkdir -p "$home"
    if out=$(env -i HOME="$home" PATH="$clean_path" sh "$repo/scripts/install.sh" --tarball "$tarball" 2>&1) \
        && [ "$("$home/.foxdb/bin/fox" --version)" = "fox $version" ] && [ -f "$home/.foxdb/skill/SKILL.md" ]; then
        echo "PASS install.sh --tarball $(basename "$tarball")"
    else
        echo "FAIL install.sh --tarball: $out"
        status=1
    fi
    # The default path: download the asset and its .sha256 (here from a file:// URL), then install.
    if command -v curl >/dev/null 2>&1; then
        home=$scratch/download/home
        mkdir -p "$home"
        if out=$(env -i HOME="$home" PATH="$clean_path" FOXDB_RELEASE_URL="file://$repo/dist" \
            sh "$repo/scripts/install.sh" 2>&1) && printf '%s' "$out" | grep -q 'sha256 ok' \
            && [ "$("$home/.foxdb/bin/fox" --version)" = "fox $version" ]; then
            echo "PASS install.sh download (file://dist)"
        else
            echo "FAIL install.sh download: $out"
            status=1
        fi
    else
        echo "no curl: skipped install.sh download"
    fi
else
    echo "no tarball in dist/: skipped install.sh --tarball and download"
fi

if [ "$(fingerprint)" != "$before" ]; then
    echo "FAIL: the user's harness settings changed during the run"
    status=1
fi
[ $status = 0 ] && echo "docs-test: PASS" || echo "docs-test: FAIL"
exit $status
