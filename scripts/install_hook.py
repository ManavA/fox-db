#!/usr/bin/env python3
"""Add (idempotently) the foxdb SessionStart hook to ~/.claude/settings.json, preserving every other key.
Usage: install_hook.py [--remove]"""
import json, os, sys, tempfile
P = os.path.expanduser("~/.claude/settings.json")
CMD = 'H="$HOME/.foxdb/bin/fox"; [ -x "$H" ] && "$H" mem brief --budget 400 2>/dev/null; exit 0'
with open(P) as f:
    d = json.load(f)
hooks = d.setdefault("hooks", {})
ss = hooks.setdefault("SessionStart", [])
# drop any previous foxdb entry
ss[:] = [g for g in ss if not any("foxdb/bin/fox" in h.get("command", "") for h in g.get("hooks", []))]
if "--remove" not in sys.argv:
    ss.append({"hooks": [{"type": "command", "command": CMD, "timeout": 2}]})
if not ss:
    hooks.pop("SessionStart")
if not hooks:
    d.pop("hooks")
fd, tmp = tempfile.mkstemp(dir=os.path.dirname(P))
with os.fdopen(fd, "w") as f:
    json.dump(d, f, indent=2, ensure_ascii=False)
    f.write("\n")
os.chmod(tmp, os.stat(P).st_mode & 0o777)
os.replace(tmp, P)
print("ok", json.dumps(d.get("hooks", {}).get("SessionStart")))
