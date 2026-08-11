"""Turn a list of opcode numbers (one per line) into a Chicken program.

Usage: python3 tools/compiler.py [input.txt] [output.ch]
Defaults to tools/num.txt -> tools/result.ch. A 0 ends the program.

For anything bigger than a few instructions, use tools/chasm.py instead.
"""
import sys
from pathlib import Path

HERE = Path(__file__).parent

src = Path(sys.argv[1]) if len(sys.argv) > 1 else HERE / "num.txt"
dst = Path(sys.argv[2]) if len(sys.argv) > 2 else HERE / "result.ch"

with open(src, "r") as infile, open(dst, "w") as out:
    for line in infile:
        num = int(line.strip())
        if num == 0:
            break
        out.write(" ".join(["chicken"] * num) + "\n")
    out.write("\n")
