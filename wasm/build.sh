#!/usr/bin/env bash
# build.sh — assemble the pgrust wasm webapp assets (postgres.wasm + VFS image).
#
# Produces under wasm/assets/ (or PGRUST_ASSETS):
#   postgres.wasm   — the wasm32-wasip1 single-user module
#   vfs.img         — packed VFS file bytes (datadir at /pgdata, share at /share)
#   vfs.json        — VFS manifest (paths/offsets/sizes/modes)
#   *.gz / *.br     — precompressed variants for serve.mjs
#
# These are gitignored build artifacts — regenerate, don't commit.
#
# Inputs (override via env):
#   PGRUST_ASSETS  output directory for browser assets (default: ./assets)
#   PGRUST_WASM    path to a built postgres.wasm
#                  (default: target/wasm32-wasip1/wasm-release/postgres.wasm,
#                   falling back to the debug build)
#   PGRUST_DATADIR an initdb'd datadir to bundle at /pgdata; when unset, one is
#                  minted here with pgrust's own initdb (target/release/initdb,
#                  or PGRUST_INITDB) and, failing that, the C PostgreSQL 18
#                  initdb (set PGINSTALL
#                  to its bin dir; /opt/homebrew/bin is probed)
#   PGRUST_SHARE   a share dir carrying timezone/timezonesets, bundled at
#                  /share; when unset it is derived from the initdb install.
#                  MUST be symlink-free (this script copies with cp -RL —
#                  WASI-style preopens refuse symlink escapes).
#
# To build the wasm module itself (pinned nightly + build-std + wasm-EH):
#   PGRUST_WASM_PROFILE=wasm-release wasm/wasm-build.sh
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
ASSETS="${PGRUST_ASSETS:-$HERE/assets}"
mkdir -p "$ASSETS"

WASM="${PGRUST_WASM:-}"
if [[ -z "$WASM" ]]; then
  for cand in "$ROOT/target/wasm32-wasip1/wasm-release/postgres.wasm" \
              "$ROOT/target/wasm32-wasip1/debug/postgres.wasm"; do
    [[ -f "$cand" ]] && WASM="$cand" && break
  done
fi
[[ -n "$WASM" && -f "$WASM" ]] || { echo "wasm not found (build it first — see header)"; exit 1; }

VFSROOT="$(mktemp -d)"
trap 'rm -rf "$VFSROOT" "$VFSROOT-datadir" "$VFSROOT-nolocale"' EXIT

DATADIR="${PGRUST_DATADIR:-}"
SHARE="${PGRUST_SHARE:-}"
if [[ -z "$DATADIR" ]]; then
  INITDB=""
  for cand in "${PGRUST_INITDB:-}" "$ROOT/target/release/initdb" "$ROOT/target/dist/initdb" "$ROOT/target/debug/initdb"; do
    [[ -n "$cand" && -x "$cand" && -x "$(dirname "$cand")/postgres" ]] && INITDB="$cand" && break
  done
  if [[ -z "$INITDB" ]]; then
    PGBIN=""
    for cand in "${PGINSTALL:-}" /tmp/pgrust_pginstall/bin /opt/homebrew/bin; do
      [[ -n "$cand" && -x "$cand/initdb" ]] && PGBIN="$cand" && break
    done
    [[ -n "$PGBIN" ]] || { echo "no initdb found: build pgrust's (cargo build --release -p initdb) or set PGINSTALL / PGRUST_DATADIR"; exit 1; }
    INITDB="$PGBIN/initdb"
    if [[ -z "$SHARE" ]]; then
      for tz in "$PGBIN/../share/postgresql/timezone" "$PGBIN/../share/postgresql@18/timezone"; do
        [[ -d "$tz" ]] && SHARE="$(dirname "$tz")" && break
      done
    fi
  elif [[ -z "$SHARE" ]]; then
    # The staged share tree beside the binary carries timezonesets but no
    # zoneinfo; assemble a bundle share with the system tz database.
    SHARE="$VFSROOT-share"
    cp -RL "$(dirname "$INITDB")/share" "$SHARE"
    [[ -d "$SHARE/timezone" ]] || cp -RL /usr/share/zoneinfo "$SHARE/timezone"
    trap 'rm -rf "$VFSROOT" "$VFSROOT-datadir" "$VFSROOT-share"' EXIT
  fi
  echo "[build] minting datadir with $INITDB..."
  DATADIR="$VFSROOT-datadir"
  # locale-less: no `locale` program on initdb's PATH, so it records no libc collation (none of which the wasm server's
  # wasi-libc, C / POSIX only, can open; a glibc host also stamps its version on each). Main 2026-10-08 ~12:3xZ.
  mkdir -p "$VFSROOT-nolocale"
  PATH="$VFSROOT-nolocale" "$INITDB" -D "$DATADIR" --no-locale --encoding=UTF8 -U postgres -A trust \
    > "$VFSROOT/initdb.log" 2>&1 || { tail -20 "$VFSROOT/initdb.log"; exit 1; }
  grep -q 'no usable system locales were found' "$VFSROOT/initdb.log" \
    || { echo "initdb imported system locales (see below)"; tail -20 "$VFSROOT/initdb.log"; exit 1; }
  rm -f "$VFSROOT/initdb.log"
fi
[[ -d "$DATADIR" ]] || { echo "datadir not found: $DATADIR"; exit 1; }
[[ -n "$SHARE" && -d "$SHARE/timezone" ]] || { echo "share dir with timezone/ not found (set PGRUST_SHARE)"; exit 1; }

echo "[build] copying wasm module ($WASM)..."
cp "$WASM" "$ASSETS/postgres.wasm"

# psql.wasm — the Rust psql client (crates/bin/psql) that drives the terminal
# in ?client=psql mode. Build: same pinned nightly as the server module,
#   cargo +nightly-2026-07-17 build --target wasm32-wasip1 \
#     -Zbuild-std=std,panic_unwind -p psql --release
# (RUSTFLAGS as in wasm/wasm-build.sh).
PSQL_WASM="${PGRUST_PSQL_WASM:-}"
if [[ -z "$PSQL_WASM" ]]; then
  for cand in "$ROOT/target/wasm32-wasip1/wasm-release/psql.wasm" \
              "$ROOT/target/wasm32-wasip1/release/psql.wasm" \
              "$ROOT/target/wasm32-wasip1/debug/psql.wasm"; do
    [[ -f "$cand" ]] && PSQL_WASM="$cand" && break
  done
fi
if [[ -n "$PSQL_WASM" && -f "$PSQL_WASM" ]]; then
  echo "[build] copying psql module ($PSQL_WASM)..."
  cp "$PSQL_WASM" "$ASSETS/psql.wasm"
else
  echo "[build] WARNING: psql.wasm not found — ?client=psql will not work (build crates/bin/psql for wasm32-wasip1 first)"
fi

echo "[build] assembling VFS tree (datadir at /pgdata, share at /share)..."
mkdir -p "$VFSROOT/pgdata" "$VFSROOT/share"
cp -R "$DATADIR/." "$VFSROOT/pgdata/"
# cp -RL: timezone entries can be absolute symlinks (homebrew Cellar) and the
# in-memory VFS, like a WASI preopen, has no symlinks.
cp -RL "$SHARE/timezone" "$VFSROOT/share/timezone"
[[ -d "$SHARE/timezonesets" ]] && cp -RL "$SHARE/timezonesets" "$VFSROOT/share/timezonesets"

echo "[build] packing VFS image..."
node "$HERE/pack-vfs.mjs" "$VFSROOT" "$ASSETS/vfs"

if [[ "${PGRUST_COMPRESS:-1}" != "0" ]]; then
  echo "[build] compressing large browser assets..."
  gzip -kf9 "$ASSETS/postgres.wasm" "$ASSETS/vfs.img"
  [[ -f "$ASSETS/psql.wasm" ]] && gzip -kf9 "$ASSETS/psql.wasm"
  if command -v brotli >/dev/null 2>&1; then
    brotli -f -q 9 "$ASSETS/postgres.wasm" "$ASSETS/vfs.img"
    [[ -f "$ASSETS/psql.wasm" ]] && brotli -f -q 9 "$ASSETS/psql.wasm"
  fi
else
  echo "[build] compression SKIPPED (PGRUST_COMPRESS=0)"
fi

echo "[build] done:"
ls -la "$ASSETS"
