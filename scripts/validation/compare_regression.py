#!/usr/bin/env python3
"""Audit regression output using SQL's ordering contract, with no result masks.

Only a complete, single-line-per-row aligned psql table whose SQL echo is found
unambiguously can be compared by order class. Everything else stays byte-exact.
This does not suppress EXPLAIN, errors, notices, helper-function results, floats,
array element order, or annotations embedded in upstream fixtures.
"""
import argparse
from bisect import bisect_right
from collections import Counter
import difflib
import hashlib
import importlib.util
import json
from pathlib import Path
import re


SPEC = importlib.util.spec_from_file_location(
    "ordering_contract", Path(__file__).resolve().parents[1] / "gravity-gate" / "order_class.py")
ORDER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ORDER)
FOOTER = re.compile(r"\((\d+) rows?\)")
SEPARATOR = re.compile(r"-+(?:\+-+)*")
DOLLAR = re.compile(r"\$(?:[A-Za-z_][A-Za-z_0-9]*)?\$")


def physical_lines(text, keepends=False):
    """psql lines end in LF, not Unicode paragraph separators or vertical tabs."""
    parts = text.split("\n")
    lines = [part + "\n" for part in parts[:-1]]
    if parts[-1]:
        lines.append(parts[-1])
    return lines if keepends else [line.removesuffix("\n").removesuffix("\r") for line in lines]


def statements(source):
    """One lexical pass; emit whole-line SQL anchors, reject mixed/meta lines.

    Unterminated literals/comments consume the remainder. Line lookup uses
    binary search; no repeatedly re-lexing growing multi-line statements.
    """
    starts = [0] + [i + 1 for i, c in enumerate(source) if c == "\n"]
    lines = physical_lines(source)
    i, begin, depth = 0, None, 0
    quoted_lines = set()
    while i < len(source):
        c = source[i]
        if source.startswith("--", i):
            end = source.find("\n", i + 2)
            i = len(source) if end < 0 else end + 1
            continue
        if source.startswith("/*", i):
            nesting, i = 1, i + 2
            while i < len(source) and nesting:
                if source.startswith("/*", i):
                    nesting, i = nesting + 1, i + 2
                elif source.startswith("*/", i):
                    nesting, i = nesting - 1, i + 2
                else:
                    i += 1
            continue
        if c.isspace():
            i += 1
            continue
        if c == "\\":
            # psql meta commands may execute or mutate a pending buffer. Do not
            # interpret that buffer, or a second SQL statement on the same line.
            end = source.find("\n", i)
            i, begin, depth = (len(source) if end < 0 else end + 1), None, 0
            quoted_lines.clear()
            continue
        if begin is None:
            begin = bisect_right(starts, i) - 1
        if c in "'\"":
            quote = c
            quote_start = i
            # Backslash escapes are defined for E strings only. SQL quoting uses
            # doubled delimiters in both string literals and quoted identifiers.
            escaped = c == "'" and i > 0 and source[i - 1] in "eE" and (i < 2 or not (source[i - 2].isalnum() or source[i - 2] == "_"))
            i += 1
            while i < len(source):
                if escaped and source[i] == "\\":
                    i += 2
                elif source[i] == quote:
                    if source[i:i + 2] == quote * 2:
                        i += 2
                    else:
                        i += 1
                        break
                else:
                    i += 1
            quoted_lines.update(range(bisect_right(starts, quote_start) - 1, bisect_right(starts, i - 1)))
            continue
        dollar = DOLLAR.match(source, i) if c == "$" else None
        if dollar:
            tag = dollar.group()
            end = source.find(tag, dollar.end())
            quote_start = i
            i = len(source) if end < 0 else end + len(tag)
            quoted_lines.update(range(bisect_right(starts, quote_start) - 1, bisect_right(starts, max(quote_start, i - 1))))
            continue
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
        elif c == ";" and depth == 0:
            end = bisect_right(starts, i) - 1
            tail = source[i + 1:starts[end + 1] if end + 1 < len(starts) else len(source)]
            sql = "\n".join(lines[begin:end + 1])
            if not tail.strip() or tail.lstrip().startswith("--"):
                # psql mainloop.c drops empty input lines outside literals before
                # echoing them. Empty lines inside dollar/string literals remain.
                anchor = [lines[k] for k in range(begin, end + 1) if lines[k] or k in quoted_lines]
                yield begin + 1, anchor, sql
            # Multiple statements on a line remain exact, including later ones.
            i = starts[end + 1] if end + 1 < len(starts) else len(source)
            begin, depth = None, 0
            quoted_lines.clear()
            tokens = ORDER.lex(sql)
            if tokens and tokens[0] == ("w", "copy"):
                depths = ORDER.depths(tokens)
                stdin = any(t == ("w", "from") and depths[k] == 0 and tokens[k + 1:k + 2] == [("w", "stdin")]
                            for k, t in enumerate(tokens))
                if stdin:
                    while i < len(source):
                        stop = source.find("\n", i)
                        stop = len(source) if stop < 0 else stop
                        data = source[i:stop].rstrip("\r")
                        i = min(stop + 1, len(source))
                        if data == "\\.":
                            break
            continue
        i += 1


def find_anchor(lines, anchor, start):
    """KMP: bounded linear search, including repeated SQL lines."""
    prefix = [0] * len(anchor)
    k = 0
    for i in range(1, len(anchor)):
        while k and anchor[k] != anchor[i]:
            k = prefix[k - 1]
        if anchor[k] == anchor[i]:
            k += 1
        prefix[i] = k
    k = 0
    for i in range(start, len(lines)):
        while k and anchor[k] != lines[i]:
            k = prefix[k - 1]
        if anchor[k] == lines[i]:
            k += 1
        if k == len(anchor):
            return i + 1 - k
    return None


def table(lines, start):
    if start + 2 >= len(lines) or not SEPARATOR.fullmatch(lines[start + 1]):
        return None
    end = start + 2
    while end < len(lines):
        match = FOOTER.fullmatch(lines[end])
        if match:
            if int(match[1]) != end - start - 2:
                return None  # multiline values or a malformed/truncated table
            return end, lines[start + 2:end]
        if not lines[end]:
            return None
        end += 1
    return None


def columns(header, separator):
    widths = [len(part) for part in separator.split("+")]
    spans, offset = [], 0
    for width in widths:
        spans.append((offset, offset + width))
        offset += width + 1
    return [header[a:b].strip() for a, b in spans], spans


def ordered_equal(expected, actual, keys, spans):
    """O(rows * keys) time and O(largest peer group) multiset storage."""
    def key(row):
        return tuple(row[spans[k - 1][0]:spans[k - 1][1]] for k in keys)
    start = 0
    while start < len(expected):
        value, end = key(expected[start]), start + 1
        while end < len(expected) and key(expected[end]) == value:
            end += 1
        if any(key(row) != value for row in actual[start:end]):
            return False
        if Counter(expected[start:end]) != Counter(actual[start:end]):
            return False
        start = end
    return True


def compare(source, expected, actual):
    e, a = physical_lines(expected, keepends=True), physical_lines(actual, keepends=True)
    # SQL echo/table parsing uses lines without EOL, while final equality retains
    # original line endings and trailing whitespace outside approved row moves.
    el, al = physical_lines(expected), physical_lines(actual)
    ep = ap = 0
    changes, refused = [], []
    for line, anchor, sql in statements(source):
        ei, ai = find_anchor(el, anchor, ep), find_anchor(al, anchor, ap)
        if ei is None or ai is None:
            refused.append({"sql_line": line, "reason": "SQL echo missing; remaining output stays exact"})
            break  # Never re-scan unmatched tails for every remaining statement.
        ep, ap = ei + len(anchor), ai + len(anchor)
        et, at = table(el, ep), table(al, ap)
        if et is None or at is None:
            continue
        ee, er = et
        ae, ar = at
        if er == ar:
            continue
        if el[ep:ep + 2] != al[ap:ap + 2] or el[ee] != al[ae] or len(er) != len(ar):
            continue
        if Counter(e[ep + 2:ee]) != Counter(a[ap + 2:ae]):
            continue  # Even row line endings must survive the permutation.
        cols, spans = columns(el[ep], el[ep + 1])
        kind, keys = ORDER.classify(sql, cols)
        allowed = kind == "unordered" and Counter(er) == Counter(ar)
        if kind == "ordered" and len(set(cols)) == len(cols) and all(row.isascii() for row in er + ar):
            allowed = ordered_equal(er, ar, keys, spans)
        if allowed:
            # Equal row counts: no offsets change. Metadata/errors/plan text are
            # untouched, and only a verified permutation is replaced for diffing.
            a[ap + 2:ae] = e[ep + 2:ee]
            changes.append({"sql_line": line, "sql_sha256": hashlib.sha256(sql.encode(errors="surrogateescape")).hexdigest(),
                            "class": kind, "keys": keys, "rows": len(er), "sql": sql})
        ep, ap = ee + 1, ae + 1
    return {"equal_after_permitted_order": e == a, "permitted_order_changes": changes,
            "refused": refused}, "".join(difflib.unified_diff(e, a, fromfile="native", tofile="candidate"))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sql", type=Path, required=True)
    parser.add_argument("--expected", type=Path, required=True)
    parser.add_argument("--actual", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--diff", type=Path, required=True)
    args = parser.parse_args()
    report, diff = compare(*(p.read_bytes().decode(errors="surrogateescape") for p in (args.sql, args.expected, args.actual)))
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.diff.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    args.diff.write_text(diff, errors="surrogateescape")
    print(json.dumps({k: v for k, v in report.items() if k != "permitted_order_changes"}))
    return 0 if report["equal_after_permitted_order"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
