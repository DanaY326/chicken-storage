"""chasm: a tiny assembler for the Chicken esolang.

Chicken source is one instruction per line, where the instruction is the
number of times the word "chicken" appears on that line. Writing that by hand
is miserable, so this assembler takes readable mnemonics, labels and named
variables and emits the equivalent .ch file.

Usage: python3 tools/chasm.py input.chasm [output.ch]

Syntax (one statement per line, ';' starts a comment):

    .vars a b c        declare variables (must come before any code)
    label:             define a jump target

    push N             push an integer (negative numbers allowed)
    push 'x'           push the character code of x (a number)
    pushchar N|'x'     push a one-character string
    chicken            push the string "chicken"
    add sub mul eq     arithmetic / equality (eq pushes 1 or 0)
    char               pop n, push the character with code n
    load               pop an address, push the value stored there
    loadin             pop i, push the i-th character of the input string
    store              pop an address, pop a value, store it there
    exit               stop; the top of the stack is the program's output

    get v              push variable v
    set v              pop into variable v
    inc v / dec v      v += 1 / v -= 1
    jmp L              jump to L
    jnz L              pop a value, jump to L if it is non-zero

How the VM is laid out (this is why variables work): the interpreter keeps a
single stack where slot 0 is the instruction pointer, slot 1 is the input, and
slots 2.. hold the program itself. The assembler starts every program with a
jump over one placeholder line per variable, and those never-executed lines
are used as the variables' storage. That keeps variable addresses fixed no
matter how long the program is or whether the file ends with a newline.
"""
import re
import sys
from pathlib import Path

OPS = {
    "exit": 0, "chicken": 1, "add": 2, "sub": 3, "mul": 4, "eq": 5,
    "store": 7, "jump": 8, "char": 9,
}
CODE_START = 2  # stack[0] is the IP, stack[1] is the input


class AsmError(Exception):
    pass


def parse_int(tok, lineno):
    m = re.fullmatch(r"'(.)'", tok)
    if m:
        return ord(m.group(1))
    try:
        return int(tok, 0)
    except ValueError:
        raise AsmError(f"line {lineno}: expected a number or 'c', got {tok!r}")


def assemble(text):
    vars_ = []
    stmts = []  # (lineno, mnemonic, arg)
    labels_seen = set()

    for lineno, raw in enumerate(text.splitlines(), 1):
        line = raw.split(";", 1)[0].strip()
        if not line:
            continue
        if line.startswith(".vars"):
            if stmts:
                raise AsmError(f"line {lineno}: .vars must come before code")
            vars_.extend(line.split()[1:])
            continue
        if line.endswith(":"):
            name = line[:-1].strip()
            if name in labels_seen:
                raise AsmError(f"line {lineno}: duplicate label {name!r}")
            labels_seen.add(name)
            stmts.append((lineno, "label", name))
            continue
        parts = line.split(None, 1)
        stmts.append((lineno, parts[0].lower(), parts[1].strip() if len(parts) > 1 else None))

    if len(set(vars_)) != len(vars_):
        raise AsmError("duplicate variable name")

    # Prologue: "jmp over the variable slots" is push 1, push <nvars>, jump.
    var_addr = {v: CODE_START + 3 + i for i, v in enumerate(vars_)}

    # Expand statements into primitives:
    #   ("push", n)    -> 1 line if n >= 0, else 3 lines (0, |n|, sub)
    #   ("op", k)      -> 1 line
    #   ("rel", label) -> push of the offset to label, measured from just
    #                     after the jump instruction that follows it
    #   ("label", L)   -> 0 lines
    prims = []

    def need(arg, lineno, what):
        if arg is None:
            raise AsmError(f"line {lineno}: {what} needs an argument")
        return arg

    def addr(v, lineno):
        if v not in var_addr:
            raise AsmError(f"line {lineno}: unknown variable {v!r}")
        return var_addr[v]

    for lineno, m, arg in stmts:
        if m == "label":
            prims.append(("label", arg))
        elif m == "push":
            prims.append(("push", parse_int(need(arg, lineno, m), lineno)))
        elif m == "pushchar":
            prims += [("push", parse_int(need(arg, lineno, m), lineno)), ("op", 9)]
        elif m in OPS:
            prims.append(("op", OPS[m]))
        elif m == "load":
            prims += [("op", 6), ("op", 0)]
        elif m == "loadin":
            prims += [("op", 6), ("op", 1)]
        elif m == "get":
            prims += [("push", addr(need(arg, lineno, m), lineno)), ("op", 6), ("op", 0)]
        elif m == "set":
            prims += [("push", addr(need(arg, lineno, m), lineno)), ("op", 7)]
        elif m in ("inc", "dec"):
            a = addr(need(arg, lineno, m), lineno)
            prims += [("push", a), ("op", 6), ("op", 0), ("push", 1),
                      ("op", 2 if m == "inc" else 3), ("push", a), ("op", 7)]
        elif m == "jmp":
            prims += [("push", 1), ("rel", need(arg, lineno, m)), ("op", 8)]
        elif m == "jnz":
            prims += [("rel", need(arg, lineno, m)), ("op", 8)]
        else:
            raise AsmError(f"line {lineno}: unknown instruction {m!r}")

    for kind, arg in prims:
        if kind == "rel" and arg not in labels_seen:
            raise AsmError(f"unknown label {arg!r}")

    def push_size(n):
        return 1 if n >= 0 else 3

    # Relative offsets change size when they flip sign, so iterate to a fixed point.
    rel_value = {i: 0 for i, (k, _) in enumerate(prims) if k == "rel"}
    base = CODE_START + 3 + len(vars_)
    for _ in range(100):
        positions, label_pos, pos = [], {}, base
        for i, (kind, arg) in enumerate(prims):
            positions.append(pos)
            if kind == "label":
                label_pos[arg] = pos
            elif kind == "push":
                pos += push_size(arg)
            elif kind == "rel":
                pos += push_size(rel_value[i])
            else:
                pos += 1
        new = {}
        for i in rel_value:
            after_jump = positions[i] + push_size(rel_value[i]) + 1
            new[i] = label_pos[prims[i][1]] - after_jump
        if new == rel_value:
            break
        rel_value = new
    else:
        raise AsmError("jump offsets did not converge")

    def emit_push(n, out):
        if n >= 0:
            out.append(n + 10)
        else:
            out += [10, -n + 10, 3]

    codes = []
    emit_push(1, codes)
    emit_push(len(vars_), codes)
    codes.append(8)
    codes += [1] * len(vars_)  # variable slots, never executed
    for i, (kind, arg) in enumerate(prims):
        if kind == "push":
            emit_push(arg, codes)
        elif kind == "rel":
            emit_push(rel_value[i], codes)
        elif kind == "op":
            codes.append(arg)

    return "\n".join(" ".join(["chicken"] * n) for n in codes) + "\n"


def main():
    if len(sys.argv) not in (2, 3):
        print(__doc__.strip().splitlines()[0])
        print("usage: python3 tools/chasm.py input.chasm [output.ch]")
        sys.exit(2)
    src = Path(sys.argv[1])
    dst = Path(sys.argv[2]) if len(sys.argv) == 3 else src.with_suffix(".ch")
    try:
        out = assemble(src.read_text())
    except AsmError as e:
        print(f"{src}: {e}", file=sys.stderr)
        sys.exit(1)
    dst.write_text(out)
    print(f"{src} -> {dst} ({out.count(chr(10))} instructions)")


if __name__ == "__main__":
    main()
