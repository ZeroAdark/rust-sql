#!/usr/bin/env bash
# SIGPIPE gate: `psql -f script.sql | head -1`, real PGDG psql 18 vs the Rust
# psql, against one server. The script's second statement is an INSERT; with
# stdout's reader gone after the first line, C psql is killed by SIGPIPE
# before it runs. Checks, per client: exit status 141 (signal 13), the INSERT
# did not run, empty stderr.
#
# Usage: sigpipe-gate.sh REALPSQL RUSTPSQL HOST PORT [USER]
set -u
REAL="$1"; RUST="$2"; HOST="$3"; PORT="$4"; U="${5:-postgres}"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
cat > "$tmp/script.sql" <<'SQL'
select g from generate_series(1, 200000) g;
insert into sigpipe_gate values (1);
SQL
q() { "$REAL" -h "$HOST" -p "$PORT" -U "$U" -d postgres -X -qAt -c "$1"; }
fail=0
for who in real rust; do
    if [ $who = real ]; then P="$REAL"; else P="$RUST"; fi
    q "drop table if exists sigpipe_gate" >/dev/null 2>&1
    q "create table sigpipe_gate (i int)"
    "$P" -h "$HOST" -p "$PORT" -U "$U" -d postgres -X -f "$tmp/script.sql" 2> "$tmp/$who.err" | head -1 > /dev/null
    st=${PIPESTATUS[0]}
    n=$(q "select count(*) from sigpipe_gate")
    echo "$who: exit=$st inserted=$n stderr=$(wc -c < "$tmp/$who.err")B"
    if [ "$st" != 141 ] || [ "$n" != 0 ] || [ -s "$tmp/$who.err" ]; then
        fail=1; sed 's/^/  stderr: /' "$tmp/$who.err"
    fi
done
q "drop table if exists sigpipe_gate" >/dev/null 2>&1
if [ $fail = 0 ]; then echo "GATE sigpipe: IDENTICAL"; else echo "GATE sigpipe: DIVERGENT"; fi
exit $fail
