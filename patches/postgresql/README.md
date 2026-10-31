# PostgreSQL window control patch

`exclude-ties-v4.patch` is the unchanged v4 patch by Andrey Rachitskiy,
downloaded from the [PostgreSQL mailing-list attachment](https://www.postgresql.org/message-id/attachment/206080/v4-0001-Fix-first_value-nth_value-and-last_value-with-EXC.patch).
It addresses [bug #19732](https://www.postgresql.org/message-id/19732-d4eed889e3021ef8@postgresql.org).
The [CommitFest entry](https://commitfest.postgresql.org/patch/7412/) was Ready for Committer when checked on 2026-10-30 UTC; this is a pending upstream patch, not an official PostgreSQL 18.6 release fix.

SHA-256: `f08d320e017677b87846b6331b5191fc3edf28dddda98a887323819a1267ebad`.

The patch retains its upstream authorship and discussion metadata. Its code is
part of PostgreSQL's contribution stream and is used with the PostgreSQL license
retained in `crates/postgres-18.6-reference/COPYRIGHT`.

Apply it only to a separate copy of the pinned reference sources. Keep the
unmodified native installation and vendor fixtures as controls. Offset-only
application to PostgreSQL 18.6 is expected; do not accept fuzzy or rejected hunks.
The Rust implementation applies the corresponding two boundary guards in
`crates/gravity/hosts/src/ops/window/funcs.rs`.

`window_peer_probe.py --reference-patch patches/postgresql/exclude-ties-v4.patch`
labels the control and records patch/binary hashes. It does not apply the patch
or alter any SQL output. The project owner explicitly selected correct window
semantics over reproducing this PostgreSQL bug.
