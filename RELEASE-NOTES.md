# pgrust v0.3.1

v0.3.1 is a bug-fix release on top of v0.3, still compatible with Postgres 18.6.

## Fixes

- Fix whole-row record references while preserving fields literally named `"*"`.
- Fix duplicated COPY notice context.
- Decode only the requested prefix for LZ4-compressed TOAST slices.
- Fix BRIN indexes with custom operator classes to use their own support functions instead of the built-in ones.
- Fix integer percentile ordering and transaction statistics accounting.

## Upgrade

Rebuild existing BRIN indexes that use custom operator classes with `REINDEX`.

pgrust is still experimental and not for production.
