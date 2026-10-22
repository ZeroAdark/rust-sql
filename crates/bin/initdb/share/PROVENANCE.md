# initdb share inputs

Vendored verbatim from PostgreSQL 18.6 (the pinned reference minor):

- `postgres.bki`, `system_constraints.sql`, `snowball_create.sql`: generated
  by the C build (genbki.pl / snowball Makefile) — byte-identical between the
  Homebrew postgresql@18 18.6 install and a local `pg186-build` of the
  reference tree, so there is one canonical image.
- `information_schema.sql`, `system_functions.sql`, `system_views.sql`,
  `sql_features.txt`, `pg_hba.conf.sample`, `pg_ident.conf.sample`: from the
  reference source tree (crates/postgres-18.6-reference).
- `postgresql.conf.sample`: the reference file plus the pgrust overlay
  (scripts/pg-reference/postgresql.conf.sample.pgrust), as scripts/pg-reference.sh
  installs it.
- `tsearch_data/*.stop`: the snowball stop-word lists from the reference source
  tree (src/backend/snowball/stopwords), which C installs into tsearch_data.
- `timezone/`: the reference tree's `src/timezone/data/tzdata.zi` compiled by
  C 18.6's own `zic` exactly as `make install` does without
  `--with-system-tzdata` (`zic -d <dir> tzdata.zi`, no extra options) —
  byte-identical to the local `pg186-install` share/postgresql/timezone.
  Regenerate: `src/timezone/zic -d crates/bin/initdb/share/timezone
  src/timezone/data/tzdata.zi` from a configured reference build (remove the
  old directory first; git stores zic's hard links as separate files).

Regenerate when the reference pin moves (postgres-reference.lock).
