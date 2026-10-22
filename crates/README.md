# pgrust — the crates folder, self-contained

This folder is a deliberately self-contained subset of the pgrust-fast
repository, assembled so that a reviewer (human or model) can scan ONE folder
and have everything needed to evaluate the code.

## What this project is

pgrust is a from-scratch rewrite of **PostgreSQL 18.6 in Rust**, with the goal
of being **measurably faster than C Postgres** on the same hardware — not a
port that merely matches it. Same wire protocol, same SQL semantics, same
on-disk format (a C 18.6 binary can boot a pgrust data directory and vice
versa), byte-identical error codes and messages — but faster.

## What is in this folder

| Path | What it is |
|---|---|
| `backend/`, `common/`, `bin/`, `interfaces/`, `pl/`, `port/`, `contrib/`, `_support/` | **The actual new codebase.** All Rust crates. This is the code under review. |
| `postgres-18.6-reference/` | **Read-only context, not our code.** The complete, pristine C source of PostgreSQL 18.6 (extracted from the upstream `REL_18_6` git tag). It exists so you can compare how pgrust does something against how original Postgres does the same thing. Never edit it; never review it as if it were ours. |
| `context/` | Copies of the repo's key orientation docs (see below). |

## The one rule that frames every review finding

**Any user-visible divergence between pgrust and C PostgreSQL is an issue.**

Internally we have re-architected large parts of the system — different memory
management, different data structures, different concurrency — that is
intentional and not a divergence. But in terms of what a *user of the
database* can observe, behavior must match C Postgres exactly:

- same query results, byte-identical where Postgres is deterministic
- same error codes and error message text
- same wire-protocol behavior
- same on-disk format and crash-recovery semantics

If you find a place where pgrust's observable output could differ from what
the code in `postgres-18.6-reference/` would produce, that is a finding —
report it, regardless of whether the pgrust version looks "better."

## Context docs (in `context/`)

- `GOAL.md` — the full goal statement and the dual performance gate
  (≤1.0× C instructions AND ≤1.0× C wall time) every claim must pass.
- `AGENTS.md` — the operational manual; explains the relationship to the
  earlier `pgrust-reference` port and where the reference material lives.
- `CATALOG.tsv` — the unit-by-unit porting catalog (~970 units): which C
  units are ported, to which crate, with benchmark status.
- `lessons.md` — the 11 overhead families that made the first port
  slow, and the refuted optimization levers. Read before judging idioms.
- `perf-doctrine.md` — the performance rules crates are held to.
- `beat-postgres.md` — the strategy doc for beating C.
- `RENAME-MAP.md` — the cbstore→pgrcolumnar rename, in case old names appear.

## What a useful review produces

Ways to improve the code, with priority on: (1) user-visible divergences from
the C reference (correctness), (2) places that violate the performance
doctrine or plausibly lose the ≤1.0×-C gates, (3) unsafe/soundness issues,
(4) simplifications that don't trade away speed. Cite the pgrust file AND,
for divergences, the corresponding `postgres-18.6-reference/` file.
