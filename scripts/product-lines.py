#!/usr/bin/env python3
"""The product's source lines (NFR-MAINT-12).

Test tooling, Python 3 standard library only. Counts the non-blank,
non-comment lines of the product's Rust sources (`core/src` and
`driver/src`, or the directories given), with test code left out:

- files named `tests.rs` or `test_support.rs`, and directories named
  `tests` or `test_support`, are skipped whole;
- an item under `#[cfg(test)]` is dropped, from the attribute's line to
  the line where the item ends (see `_item_end`);
- a `#[cfg(test)] mod name;` also skips the module's file (`name.rs`)
  and directory (`name/`) beside its parent module, as rustc finds them
  (inside inline modules too: `mod a { mod b; }` is `a/b.rs`).

Comments (`//`, `///`, `//!`, `/* */`, nested) are not counted; a line
with code and a comment is. String and character literals are code, so a
`//`, `/*` or brace inside one changes nothing.

    product-lines.py [ROOT ...]
    product-lines.py --csv --commit <sha> --date YYYY-MM-DD [ROOT ...]

The first form prints the count; the second prints a CSV (header and one
row: `date,commit,lines`) for the nightly's `bench-results` artifact.

A sketch of Rust's lexer, not a parser. Its known approximations, each
with the direction of its error:

- An attribute written before `#[cfg(test)]` stays counted (over).
- `#[cfg(any(test, ..))]`, `#[cfg(all(test, ..))]`, `#![cfg(test)]` and
  `#[path]` are not recognised: that code stays counted (over).
- A test-only statement or field ends at its first `;`, `,` or block at
  bracket depth 0. A comma between angle brackets (`let x: Map<A, B>`)
  ends it early (over); an `if` with an `else` ends after the first block
  (over).
- Lines are dropped whole: code before `#[cfg(test)]` on its line, or
  after the item's end on its last line, is not counted (under).
"""

import argparse
import os
import re
import sys
from datetime import date as _date

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_ROOTS = [os.path.join(REPO, "core", "src"), os.path.join(REPO, "driver", "src")]
SKIP_DIRS = ("tests", "test_support")
SKIP_FILES = ("tests.rs", "test_support.rs")
HEADER = "date,commit,lines"

_IDENT = re.compile(r"[A-Za-z0-9_]")
_RAW = re.compile(r'[bc]?r(#*)"')
_CHAR = re.compile(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]{1,6}\}|.)|[^\\'\n])'")
_CFG_TEST = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
_VIS = r"(?:pub\s*(?:\([^)]*\))?\s*)?"
# Items that end at a `;` or after their `{}` block.
_BLOCK_ITEM = re.compile(
    _VIS + r"(?:fn|impl|struct|enum|union|trait|mod|unsafe|extern|async|macro_rules)\b")
# Items that end at a `;` only (braces nest like brackets).
_SEMI_ITEM = re.compile(_VIS + r"(?:use|const|static|type)\b")
_MOD_DECL = re.compile(_VIS + r"mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;")
_INLINE_MOD = re.compile(r"\bmod\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{")
_COMMIT = re.compile(r"[0-9a-f]{7,40}", re.ASCII)
_DATE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}", re.ASCII)


def _blank(s):
    return "".join(c if c == "\n" else " " for c in s)


def _fill(s):
    return "".join(c if c == "\n" else "x" for c in s)


def mask(text):
    """The text with comments blanked (spaces) and the contents of string
    and character literals filled (`x`); line breaks are kept, so line k
    of the mask is line k of the text."""
    out, i, n = [], 0, len(text)
    while i < n:
        c = text[i]
        if text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
            out.append(_blank(text[i:j]))
            i = j
        elif text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            out.append(_blank(text[i:j]))
            i = j
        elif c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            j = min(j, n)
            out.append('"' + _fill(text[i + 1:j]) + text[j:j + 1])
            i = j + 1
        elif c in "bcr" and (i == 0 or not _IDENT.match(text[i - 1])) and _RAW.match(text, i):
            m = _RAW.match(text, i)
            close = '"' + m.group(1)
            j = text.find(close, m.end())
            j = n if j < 0 else j
            out.append(text[i:m.end()] + _fill(text[m.end():j]) + text[j:j + len(close)])
            i = j + len(close)
        elif c == "'" and _CHAR.match(text, i):
            m = _CHAR.match(text, i)
            out.append("'" + _fill(m.group(0)[1:-1]) + "'")
            i = m.end()
        else:
            out.append(c)
            i += 1
    return "".join(out)


def _skip_attributes(m, i):
    """The index of the first character after the whitespace and outer
    attributes (`#[..]`) at `i` of the mask."""
    n = len(m)
    while True:
        while i < n and m[i].isspace():
            i += 1
        j = i + 1
        while j < n and m[j].isspace():
            j += 1
        if not (m.startswith("#", i) and j < n and m[j] == "["):
            return i
        depth = 0
        while j < n:
            depth += {"[": 1, "]": -1}.get(m[j], 0)
            if depth == 0:
                break
            j += 1
        i = j + 1


def _before(m, j, floor):
    """The last non-blank index before `j` (not below `floor`)."""
    j -= 1
    while j > floor and m[j].isspace():
        j -= 1
    return j


def _item_end(m, i):
    """The index of the last character of the item at `i` of the mask:
    a block item (`fn`, `impl`, `struct`, `mod`, ..) at its `;` or the
    `}` closing its block; a `use`, `const`, `static` or `type` at its
    `;`; anything else (a field, a statement, an expression) at its `;`,
    `,` or block, or before the bracket that closes around it."""
    if _BLOCK_ITEM.match(m, i):
        stops = ";{"
    elif _SEMI_ITEM.match(m, i):
        stops = ";"
    else:
        stops = ";,{"
    depth, j, n = 0, i, len(m)
    while j < n:
        ch = m[j]
        if ch in "([" or (ch == "{" and "{" not in stops):
            depth += 1
        elif ch in ")]}" and depth == 0:
            return _before(m, j, i)
        elif ch in ")]}":
            depth -= 1
        elif depth == 0 and ch in stops:
            if ch != "{":
                return j
            return _closing_brace(m, j)
        j += 1
    return n - 1


def _closing_brace(m, j):
    """The index of the `}` closing the `{` at `j` of the mask (the
    mask's end if none)."""
    braces = 0
    for k in range(j, len(m)):
        braces += {"{": 1, "}": -1}.get(m[k], 0)
        if braces == 0:
            return k
    return len(m) - 1


def _enclosing_modules(m, pos):
    """The names of the inline modules (`mod name { .. }`) around `pos`
    of the mask, outermost first."""
    return [
        a.group(1) for a in _INLINE_MOD.finditer(m, 0, pos)
        if pos < _closing_brace(m, a.end() - 1)
    ]


def test_regions(m):
    """[(start, end, out-of-line module name or None)] of the `#[cfg(test)]`
    items of a mask, outermost only."""
    regions, pos = [], 0
    while True:
        a = _CFG_TEST.search(m, pos)
        if a is None:
            return regions
        i = _skip_attributes(m, a.end())
        end = max(_item_end(m, i), a.end() - 1)
        decl = _MOD_DECL.match(m, i)
        regions.append((a.start(), end, decl.group(1) if decl else None))
        pos = end + 1


def _count_mask(m, regions):
    dropped = set()
    for start, end, _ in regions:
        dropped.update(range(m.count("\n", 0, start), m.count("\n", 0, end) + 1))
    return sum(1 for k, line in enumerate(m.split("\n")) if k not in dropped and line.strip())


def count_text(text):
    """The counted lines of one file's text."""
    m = mask(text)
    return _count_mask(m, test_regions(m))


def _files(root):
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS)
        for name in sorted(filenames):
            if name.endswith(".rs") and name not in SKIP_FILES:
                yield os.path.normpath(os.path.join(dirpath, name))


def _module_dir(path):
    """Where a file's child modules live: beside a `mod.rs`, `lib.rs` or
    `main.rs`; in the directory named after any other file."""
    head, name = os.path.split(path)
    if name in ("mod.rs", "lib.rs", "main.rs"):
        return head
    return os.path.join(head, name[:-3])


def count_tree(roots):
    """The counted lines of every product file under the roots."""
    masks = {}
    for root in roots:
        for path in _files(root):
            with open(path, encoding="utf-8", errors="replace") as fh:
                m = mask(fh.read())
            masks[path] = (m, test_regions(m))
    skipped_files, skipped_dirs = set(), []
    for path, (m, regions) in masks.items():
        for start, _, name in regions:
            if name:
                base = os.path.join(_module_dir(path), *_enclosing_modules(m, start), name)
                skipped_files.add(base + ".rs")
                skipped_dirs.append(base + os.sep)
    total = 0
    for path, (m, regions) in masks.items():
        if path in skipped_files or any(path.startswith(d) for d in skipped_dirs):
            continue
        total += _count_mask(m, regions)
    return total


def _commit_arg(text):
    if not _COMMIT.fullmatch(text):
        raise argparse.ArgumentTypeError(f"{text!r}: 7 to 40 lowercase hex digits")
    return text


def _date_arg(text):
    try:
        if _DATE.fullmatch(text):
            return _date.fromisoformat(text).isoformat()
    except ValueError:
        pass
    raise argparse.ArgumentTypeError(f"{text!r}: YYYY-MM-DD")


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--csv", action="store_true",
                   help="print a CSV header and row (needs --commit and --date)")
    p.add_argument("--commit", type=_commit_arg, help="with --csv: the counted commit")
    p.add_argument("--date", type=_date_arg, help="with --csv: the date of the count")
    p.add_argument("roots", nargs="*", metavar="ROOT",
                   help="directories to count (default: core/src and driver/src)")
    args = p.parse_args(argv)
    roots = args.roots or DEFAULT_ROOTS
    for root in roots:
        if not os.path.isdir(root):
            p.error(f"{root}: not a directory")
    if args.csv and (args.commit is None or args.date is None):
        p.error("--csv needs --commit and --date")
    lines = count_tree(roots)
    if args.csv:
        print(HEADER)
        print(f"{args.date},{args.commit},{lines}")
    else:
        print(lines)
    return 0


if __name__ == "__main__":
    sys.exit(main())
