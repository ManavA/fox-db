#!/bin/sh
# hook.sh EVENT [claude|codex]: the foxdb plugin's hooks. Runs `fox hook EVENT --harness H`
# (default claude) with the installed fox and always exits 0, so a hook never gets in the
# session's way. Without an installed fox, the session-start hook says how to install it; the
# other events stay silent.
harness=${2:-claude}
fox=${FOXDB_HOME:-$HOME/.foxdb}/bin/fox
if [ -x "$fox" ]; then
    "$fox" hook "$1" --harness "$harness"
    exit 0
fi
if [ "$1" = session-start ]; then
    install="curl -fsSL https://raw.githubusercontent.com/ManavA/fox-db/main/scripts/install.sh | sh"
    context="The foxdb plugin is enabled but fox is not installed, so the fox_* tools and the fox command fail. If the user asks for foxdb, tell them to run: $install"
    if [ "$harness" = claude ]; then
        printf '{"systemMessage":"foxdb plugin: fox is not installed in ~/.foxdb. Install it with: %s (then restart the session)",' "$install"
    else
        printf '{'
    fi
    printf '"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"%s"}}\n' "$context"
fi
exit 0
