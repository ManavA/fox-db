#!/usr/bin/env python3
"""Compiler wrapper that records each compile command, then runs the real compiler.

    make CC=/abs/path/cc_log.py ORACLE_COMPDB=/abs/compdb.jsonl ...

ORACLE_REAL_CC names the real compiler (default `cc`). Each command that compiles a C source
(`-c` plus a .c file) adds one JSON line: {"directory", "arguments", "file"}.
"""

import json
import os
import sys


def main() -> None:
    real = os.environ.get("ORACLE_REAL_CC", "cc")
    args = sys.argv[1:]
    log = os.environ.get("ORACLE_COMPDB")
    srcs = [a for a in args if a.endswith(".c") and not a.startswith("-")]
    if log and "-c" in args and len(srcs) == 1:
        row = {"directory": os.getcwd(), "arguments": [real] + args, "file": srcs[0]}
        with open(log, "a") as f:
            f.write(json.dumps(row) + "\n")
    os.execvp(real, [real] + args)


if __name__ == "__main__":
    main()
