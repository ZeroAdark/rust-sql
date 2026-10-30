"""Linear proof of one pinned MERGE's row actions, trigger phases and RETURNING.

Rows may be processed in any order, but every action and old/new value must match
the source-bound input oracle. BEFORE/AFTER row notices and returned rows must
share that order; statement-trigger phases stay exact. No other notices change.
"""
from functools import lru_cache
import hashlib
import json
from pathlib import Path

from compare_regression import columns, find_anchor, physical_lines, statements, table


POLICY = Path(__file__).resolve().parents[2] / "regress/engine-profile/policy/merge-trace.json"
BEFORE = [f"BEFORE {action} STATEMENT trigger" for action in ("INSERT","UPDATE","DELETE")]
AFTER = [f"AFTER {action} STATEMENT trigger" for action in ("DELETE","UPDATE","INSERT")]


def digest(text):
    return hashlib.sha256(text.encode(errors="surrogateescape")).hexdigest()


@lru_cache(maxsize=1)
def policy():
    value = json.loads(POLICY.read_text())
    if value["version"] != 1:
        raise ValueError("unsupported MERGE trace policy")
    return value


def expected_actions(target_rows,source_rows):
    target, source = dict(target_rows), dict(source_rows)
    if len(target) != len(target_rows) or len(source) != len(source_rows):
        raise ValueError("duplicate pinned MERGE input key")
    result = {}
    for key in target.keys() | source.keys():
        old = f"({key},{target[key]})" if key in target else None
        if key in source:
            if key in target:
                action = "UPDATE"
                balance = target[key]-source[key] if target[key]>source[key] else 0
            else:
                action,balance = "INSERT",source[key]
            new = f"({key},{balance})"
        elif key == 1:
            action,balance,new = "UPDATE",0,f"({key},0)"
        else:
            action,balance,new = "DELETE",target[key],None
        result[key] = (action,old,new,key,balance)
    return result


def row_notice(when,row):
    action,old,new,_,_ = row
    value = f"{old} -> {new}" if action == "UPDATE" else new if action == "INSERT" else old
    return f"{when} {action} ROW trigger row: {value}"


def valid_trace(notices,rows,target_rows,source_rows):
    expected = expected_actions(target_rows,source_rows)
    if len(rows) != len(expected) or any(len(row) != 5 or type(row[3]) is not int for row in rows):
        return False
    if len({row[3] for row in rows}) != len(expected) or any(tuple(row) != expected.get(row[3]) for row in rows):
        return False
    return notices == BEFORE + [row_notice("BEFORE",row) for row in rows] + [row_notice("AFTER",row) for row in rows] + AFTER


def trace_block(raw,start,rule):
    lines = [line.removesuffix("\n") for line in raw]
    cursor, notices = start, []
    while cursor < len(raw) and raw[cursor].startswith("NOTICE:  "):
        notices.append(raw[cursor][9:].removesuffix("\n"));cursor += 1
    result = table(lines,cursor)
    if not result:
        return None
    end,records = result
    labels,spans = columns(lines[cursor],lines[cursor+1])
    if labels != ["merge_action","old","new","tid","balance"] or any(
            not line.isascii() or not line.endswith("\n") or line.endswith("\r\n") for line in raw[start:end+1]):
        return None
    values = []
    try:
        for row in records:
            if len(row)>spans[-1][1] or any(row[b:b+1]!="|" for _,b in spans[:-1]):
                return None
            action,old,new,key,balance = [row[a:b].strip(" ") for a,b in spans]
            if str(int(key))!=key or str(int(balance))!=balance:
                return None
            values.append((action,old or None,new or None,int(key),int(balance)))
    except (ValueError,IndexError):
        return None
    if not valid_trace(notices,values,rule["target_rows"],rule["source_rows"]):
        return None
    return end+1,cursor,values


def compare_merge_trace(source,expected,actual):
    rule = policy()
    if digest(source)!=rule["source_sha256"] or digest(expected)!=rule["expected_sha256"]:
        return actual,[]
    e,a = physical_lines(expected,keepends=True),physical_lines(actual,keepends=True)
    el,al = physical_lines(expected),physical_lines(actual)
    ep=ap=0
    for line,anchor,sql in statements(source):
        ei,ai = find_anchor(el,anchor,ep),find_anchor(al,anchor,ap)
        if ei is None or ai is None:
            return actual,[]
        ep,ap = ei+len(anchor),ai+len(anchor)
        if line!=rule["sql_line"] or sql!=rule["sql"]:
            continue
        before,after = trace_block(e,ep,rule),trace_block(a,ap,rule)
        if not before or not after:
            return actual,[]
        ee,eh,er = before;ae,ah,ar = after
        if e[eh:eh+2]!=a[ah:ah+2] or e[ee-1]!=a[ae-1] or e[ep:ee]==a[ap:ae]:
            return actual,[]
        proof = {"kind":"merge_action_trigger_trace","sql_line":line,"sql_sha256":digest(sql),"rows":len(ar),
                 "native_row_order":[r[3] for r in er],"candidate_row_order":[r[3] for r in ar],
                 "native_trace_sha256":digest("".join(e[ep:ee])),"candidate_trace_sha256":digest("".join(a[ap:ae]))}
        return "".join(a[:ap]+e[ep:ee]+a[ae:]),[proof]
    return actual,[]
