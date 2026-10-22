# PostgreSQL reference tree — fetched, not committed

`crates/postgres-<version>-reference` is the pristine upstream PostgreSQL
source at the tag pinned in `postgres-reference.lock`. It is gitignored and
materialized by `scripts/pg-reference.sh`, which downloads the tag archive
(sha256-verified, cached under `~/.cache/pgrust`), extracts it, and appends
`postgresql.conf.sample.pgrust` (the PGRUST settings section) to
`src/backend/utils/misc/postgresql.conf.sample`.

- **Do not edit anything under the tree.** Edits are lost on the next
  rebuild; changes that must ride along go in this overlay directory.
- **Do not build from it in place.** Release and TAP boxes copy it out first.
- It is the tag content: no local patches, no c2rust shims. `src/test/regress`
  is the default corpus for `scripts/pg-regress-fast.sh`, so the regression
  overlays under `regress/` are keyed to this exact version.
- Not a Cargo workspace member and not read by any `build.rs`.
- CI refuses any PR that tracks files under the tree (lint-gates), so a
  branch cut before the tree left git cannot re-add it.

Bumping the pin: edit the four lines in `postgres-reference.lock`, run the
script, and rename the tree path in scripts if the major version changed.
History: the tree was vendored in git from 2026-08-21 (18.3) and 2026-09-01
(18.6) until 2026-09-15.
