"""Turn a Chicken program back into one opcode number per line.

Usage: python3 tools/decompiler.py [input.ch] [output.txt]
Defaults to tools/result.ch -> tools/decompiled.txt.
"""
import sys
from pathlib import Path

HERE = Path(__file__).parent

src = Path(sys.argv[1]) if len(sys.argv) > 1 else HERE / "result.ch"
dst = Path(sys.argv[2]) if len(sys.argv) > 2 else HERE / "decompiled.txt"

with open(src, "r") as infile, open(dst, "w") as out:
    for line in infile:
        # An empty line is opcode 0, not 1.
        num = len(line.split())
        out.write(str(num) + "\n")
