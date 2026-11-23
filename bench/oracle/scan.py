"""Source scans that do not depend on fox: function definitions and their line ranges, Rust
containers and cfg regions, C preprocessor branches, and which lines a build compiles.

All of them work on `clean` text: comments, strings and char literals blanked to spaces with
newlines kept, so offsets and line numbers match the source.
"""

import re
from dataclasses import dataclass, field

LITERALS = {
    "c": re.compile(r"""//[^\n]*|/\*.*?(?:\*/|\Z)|"(?:\\.|[^"\\\n])*"?|'(?:\\.|[^'\\\n])+'""", re.S),
    "go": re.compile(r"""//[^\n]*|/\*.*?(?:\*/|\Z)|"(?:\\.|[^"\\\n])*"?|`[^`]*`?|'(?:\\.|[^'\\\n])+'""", re.S),
    "rust": re.compile(r"""//[^\n]*|/\*.*?(?:\*/|\Z)|(?<![\w])b?r(#*)".*?"\1|b?"(?:\\.|[^"\\])*"?"""
                       r"""|b?'(?:\\u\{[0-9a-fA-F]+\}|\\.|[^\\'\n])'""", re.S),
}


def _blank(m: re.Match) -> str:
    return re.sub(r"[^\n]", " ", m[0])


def clean(text: str, lang: str) -> str:
    """`text` with comments, strings and char literals turned into spaces (newlines kept).
    Rust block comments are taken as not nested."""
    return LITERALS[lang].sub(_blank, text)


def line_starts(text: str) -> list:
    return [0] + [m.end() for m in re.finditer("\n", text)]


def line_of(starts: list, off: int) -> int:
    lo, hi = 0, len(starts) - 1
    while lo < hi:
        mid = (lo + hi + 1) // 2
        if starts[mid] <= off:
            lo = mid
        else:
            hi = mid - 1
    return lo + 1


@dataclass
class Block:
    head: str  # code since the previous `;`, `{` or `}`
    head_at: int  # offset where that code starts
    open: int
    close: int
    parent: "Block | None" = None
    kids: list = field(default_factory=list)


def blocks(cl: str) -> list:
    """Every `{ ... }` block of cleaned text, outermost first, with the code that leads it."""
    out, stack, seg = [], [], 0
    for m in re.finditer(r"[{};]", cl):
        i, c = m.start(), m[0]
        if c == "{":
            b = Block(cl[seg:i], seg, i, len(cl), stack[-1] if stack else None)
            if stack:
                stack[-1].kids.append(b)
            stack.append(b)
            out.append(b)
            seg = i + 1
        elif c == "}":
            if stack:
                stack.pop().close = i
            seg = i + 1
        elif c == ";":
            seg = i + 1
    return out


# ---------- C ----------

COND = re.compile(r"^[ \t]*#[ \t]*(if|ifdef|ifndef|elif|elifdef|elifndef|else|endif)\b")


def c_directive_lines(text: str) -> list:
    """Per line (index 0 = line 1): True for preprocessor directive lines and their continuations."""
    lines = text.split("\n")
    out, cont = [], False
    for ln in lines:
        d = cont or ln.lstrip().startswith("#")
        out.append(d)
        cont = d and ln.endswith("\\")
    return out


def c_in_define(text: str, line: int) -> bool:
    """Whether `line` is part of a #define (its first line or a continuation)."""
    ls = text.split("\n")
    i = line - 1
    while 0 < i < len(ls) and ls[i - 1].endswith("\\"):
        i -= 1
    return 0 <= i < len(ls) and re.match(r"\s*#\s*define\b", ls[i]) is not None


def c_first_branch_only(text: str, cl: str) -> str:
    """Cleaned C with directive lines and every #elif/#else branch blanked, so braces balance as in
    one configuration (the way ctags reads C)."""
    lines, dirs = cl.split("\n"), c_directive_lines(text)
    src = text.split("\n")
    stack, out = [], []
    for i, ln in enumerate(lines):
        m = COND.match(src[i]) if dirs[i] else None
        if m:
            k = m[1]
            if k.startswith("if"):
                stack.append(False)
            elif k == "endif":
                stack and stack.pop()
            elif stack:
                stack[-1] = True
        skip = dirs[i] or any(stack)
        out.append(" " * len(ln) if skip else ln)
    return "\n".join(out)


C_KEYWORDS = {"if", "while", "for", "switch", "return", "sizeof", "__attribute__", "defined", "do", "else",
              "typeof", "__typeof__", "_Alignas", "alignof", "_Static_assert", "__declspec", "int", "char", "void",
              "long", "short", "unsigned", "signed", "float", "double", "const", "volatile", "_Bool", "bool"}


@dataclass
class Def:
    name: str  # as fox names it: Type.name for methods, else the bare name
    bare: str
    line: int  # line of the name
    end: int
    public: bool = True
    trait: bool = False  # Rust: in a trait or a trait impl, whose signature the trait fixes


def c_functions(text: str) -> list:
    """Function definitions of a C file: `name(...) {` at file scope."""
    cl = c_first_branch_only(text, clean(text, "c"))
    st = line_starts(text)
    out = []
    for b in blocks(cl):
        if b.parent is not None:
            continue
        head = b.head
        last = head.rfind(")")
        if last < 0 or "=" in head.replace("==", "") or not re.fullmatch(r"[\w\s]*", head[last + 1 :]):
            continue
        cands = [m for m in re.finditer(r"\b([A-Za-z_]\w*)\s*\(", head) if m[1] not in C_KEYWORDS]
        if not cands:
            continue
        # The declarator is the last name( at paren depth 0: earlier ones are attributes or
        # macro uses such as GIT_PATH_FUNC(...); none at depth 0 means int (*f(void))(int).
        top = [m for m in cands if head.count("(", 0, m.start()) == head.count(")", 0, m.start())]
        m = top[-1] if top else cands[0]
        depth, end = 0, m.end() - 1
        for end in range(m.end() - 1, len(head)):
            depth += (head[end] == "(") - (head[end] == ")")
            if depth == 0:
                break
        m = next((c for c in cands if c.start() > end), m)
        out.append(Def(m[1], m[1], line_of(st, b.head_at + m.start(1)), line_of(st, b.close),
                       public=not re.search(r"\bstatic\b", head[: m.start()])))
    return out


def c_branches(text: str) -> list:
    """Preprocessor conditional branches: (first line, last line) of the code under each #if,
    #elif or #else, nested ones included. Lines are 1-based; directive lines are excluded."""
    src, dirs = text.split("\n"), c_directive_lines(text)
    out, stack = [], []
    for i, ln in enumerate(src):
        m = COND.match(ln) if dirs[i] else None
        if not m:
            continue
        k, line = m[1], i + 1
        if k.startswith("if"):
            stack.append(line)
        elif stack:
            start = stack.pop()
            out.append((start + 1, line - 1))
            if k != "endif":
                stack.append(line)
    return out


def preprocessed_lines(pp: str, root: str) -> dict:
    """From `gcc -E` output: source path -> set of lines that produced non-blank output.
    Only paths under `root` (or relative ones) are kept."""
    out, cur, line = {}, None, 0
    marker = re.compile(r'^# (\d+) "((?:[^"\\]|\\.)*)"')
    for raw in pp.split("\n"):
        if raw.startswith("# "):
            m = marker.match(raw)
            if m:
                line, path = int(m[1]), m[2]
                cur = path if not path.startswith(("<", "/")) or path.startswith(root) else None
                continue
        if cur is not None and raw.strip():
            out.setdefault(cur, set()).add(line)
        line += 1
    return out


def inactive_lines(text: str, active: set) -> set:
    """Lines inside a conditional branch none of whose lines produced output in any unit."""
    dirs = c_directive_lines(text)
    out = set()
    for a, b in c_branches(text):
        code = [n for n in range(a, b + 1) if not dirs[n - 1]]
        if code and not any(n in active for n in code):
            out.update(range(a, b + 1))
    return out


# ---------- Rust ----------

def rust_fns(text: str) -> list:
    """Rust fns with bodies; name is Container.fn when the fn sits directly in an impl, trait or mod."""
    cl = clean(text, "rust")
    st = line_starts(text)
    out = []
    for b in blocks(cl):
        m = re.search(r"\bfn\s+((?:r#)?[A-Za-z_]\w*)", b.head)
        if not m:
            continue
        head_rest = b.head[m.end():]
        if re.search(r"\bfn\b", head_rest):
            continue
        cont = container(b.parent)
        q = f"{cont[1]}.{m[1]}" if cont else m[1]
        pub = bool(re.search(r"(?:^|\s)pub(\s|$)", b.head[: m.start()]))
        trait = bool(cont) and cont[0] == "trait"
        out.append(Def(q, m[1], line_of(st, b.head_at + m.start(1)), line_of(st, b.close), pub, trait))
    return out


def container(b: Block | None):
    """(kind, name) of an impl/trait/mod block; kind is `trait` for a trait or a trait impl."""
    if b is None:
        return None
    h = " ".join(re.sub(r"#!?\[[^\]]*\]", " ", b.head).split())
    m = re.match(r"(?:pub(?:\([^)]*\))?\s+)?(?:unsafe\s+)?(impl|mod|trait)\b(.*)", h)
    if not m:
        return None
    if m[1] in ("mod", "trait"):
        n = re.match(r"\s*(\w+)", m[2])
        return (m[1], n[1]) if n else None
    rest = skip_generics(m[2])
    is_trait = bool(re.search(r"\bfor\b", rest))
    rest = re.split(r"\bfor\b", rest)[-1]
    ty = re.match(r"\s*(?:&\s*(?:'\w+\s+)?)?(?:mut\s+)?(?:dyn\s+)?(?:\w+::)*(\w+)", rest)
    return ("trait" if is_trait else "impl", ty[1]) if ty else None


def skip_generics(s: str) -> str:
    s = s.lstrip()
    if not s.startswith("<"):
        return s
    depth = 0
    for i, ch in enumerate(s):
        depth += (ch == "<") - (ch == ">" and s[i - 1] != "-")
        if depth == 0:
            return s[i + 1 :]
    return ""


RUST_CFG_TRUE = {"unix", "test", "debug_assertions", 'target_os="linux"', 'target_family="unix"',
                 'target_arch="x86_64"', 'target_pointer_width="64"', 'target_endian="little"', 'target_env="gnu"',
                 'panic="unwind"'}


def eval_cfg(pred: str, features=None) -> bool:
    """cfg predicate for Linux x86_64 with tests on. Features count as on unless `features` lists
    the enabled ones. Unknown flags count as off."""
    toks = re.findall(r'\w+\s*=\s*"[^"]*"|\w+|[(),]', pred)
    toks = [re.sub(r"\s*=\s*", "=", t) for t in toks]
    pos = 0

    def one():
        nonlocal pos
        t = toks[pos]
        pos += 1
        if t in ("any", "all", "not") and pos < len(toks) and toks[pos] == "(":
            pos += 1
            vals = []
            while toks[pos] != ")":
                vals.append(one())
                if toks[pos] == ",":
                    pos += 1
            pos += 1
            return any(vals) if t == "any" else all(vals) if t == "all" else not vals[0]
        if t.startswith("feature="):
            return features is None or t[9:-1] in features
        return t in RUST_CFG_TRUE

    try:
        return one()
    except IndexError:
        return True


def rust_cfg_off(text: str, features=None) -> set:
    """Lines of items whose #[cfg(...)] is false on Linux x86_64 (heuristic: the item runs from the
    attribute to the first `;` or the end of its first `{}` block)."""
    cl = clean(text, "rust")
    st = line_starts(text)
    out = set()
    for m in re.finditer(r"#(!?)\[\s*cfg\s*\(", cl):
        depth, i = 1, m.end()
        while i < len(cl) and depth:
            depth += (cl[i] == "(") - (cl[i] == ")")
            i += 1
        pred = cl[m.end() : i - 1]
        if eval_cfg(pred, features):
            continue
        if m[1] == "!":
            return set(range(1, len(st) + 1))
        j, depth = cl.find("]", i) + 1, 0
        while 0 < j < len(cl):
            c = cl[j]
            if c in "([":
                depth += 1
            elif c in ")]":
                depth -= 1
            elif depth == 0 and c == ";":
                break
            elif depth == 0 and c == "{":
                d = 0
                while j < len(cl):
                    d += (cl[j] == "{") - (cl[j] == "}")
                    if d == 0:
                        break
                    j += 1
                break
            j += 1
        out.update(range(line_of(st, m.start()), line_of(st, min(j, len(cl) - 1)) + 1))
    return out


def rust_macro_defs(text: str) -> list:
    """(first, last) lines of each macro_rules! definition."""
    cl = clean(text, "rust")
    st = line_starts(text)
    out = []
    for m in re.finditer(r"\bmacro_rules!\s*\w+\s*([({\[])", cl):
        close = {"(": ")", "{": "}", "[": "]"}[m[1]]
        depth, j = 0, m.end() - 1
        while j < len(cl):
            depth += (cl[j] == m[1]) - (cl[j] == close)
            if depth == 0:
                break
            j += 1
        out.append((line_of(st, m.start()), line_of(st, j)))
    return out


def innermost(defs: list, line: int):
    best = None
    for d in defs:
        if d.line <= line <= d.end and (best is None or d.line >= best.line):
            best = d
    return best
