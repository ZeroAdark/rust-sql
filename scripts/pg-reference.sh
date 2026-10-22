#!/usr/bin/env bash
# Ensure the pristine upstream PostgreSQL reference tree is present at
# public/crates/postgres-18.6-reference (gitignored) and print its path.
# REPO below is the public/ tree (this script's parent's parent), so every
# path here is relative to public/.
#
# The tree is the git archive of the tag pinned in postgres-reference.lock,
# verified by sha256, plus the pgrust overlay under scripts/pg-reference/
# (our PGRUST section appended to postgresql.conf.sample). A stamp file
# records the sha the tree was built from; a pin change rebuilds it.
#
#   scripts/pg-reference.sh            ensure + print path
#   scripts/pg-reference.sh --check    exit 1 if absent or stale, never fetch
#   PGRUST_REFERENCE_CACHE=<dir>       where the tarball is kept
#                                      (default ~/.cache/pgrust)
set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
LOCK="$REPO/postgres-reference.lock"
OVERLAY="$REPO/scripts/pg-reference"
CACHE="${PGRUST_REFERENCE_CACHE:-$HOME/.cache/pgrust}"

version=$(sed -n 's/^version=//p' "$LOCK")
tag=$(sed -n 's/^tag=//p' "$LOCK")
url=$(sed -n 's/^url=//p' "$LOCK")
sha=$(sed -n 's/^sha256=//p' "$LOCK")
[ -n "$version" ] && [ -n "$tag" ] && [ -n "$url" ] && [ -n "$sha" ] || {
    echo "pg-reference: malformed $LOCK" >&2; exit 2; }

major=${version%%.*}
DEST="$REPO/crates/postgres-$version-reference"
STAMP="$DEST/.pgrust-reference-stamp"

# The gravity line keeps the reference tree tracked (.gitignore): the committed tree is the reference and is never
# fetched over. A fetch replaces it with rm -rf + mv, so a crate test reading it at that moment found no file (D3128 merge,
# precheck dev of c12bd381ca7: facts_m8b's pg_opfamily.dat while lint-walker-arms.sh ran this script), and the extracted
# tree differs from the committed one (README, the PGRUST section of postgresql.conf.sample).
# A sparse checkout without the tree (tracked, absent on disk) still fetches it, as before.
if [ -f "$DEST/configure.ac" ] && git -C "$REPO" ls-files --error-unmatch "crates/postgres-$version-reference/configure.ac" > /dev/null 2>&1; then
    echo "$DEST"; exit 0
fi
# the overlay changes the tree too, so it is part of the stamp
overlay_sum=$(cat "$OVERLAY"/postgresql.conf.sample.pgrust | shasum -a 256 | cut -c1-16)
want="$sha $overlay_sum"

if [ -f "$STAMP" ] && [ "$(cat "$STAMP")" = "$want" ]; then
    echo "$DEST"; exit 0
fi
if [ "${1:-}" = "--check" ]; then
    echo "pg-reference: $DEST is absent or stale; run scripts/pg-reference.sh" >&2; exit 1
fi

mkdir -p "$CACHE"
tarball="$CACHE/postgres-$tag.tar.gz"
verify() { echo "$sha  $tarball" | shasum -a 256 -c --status; }
if ! { [ -f "$tarball" ] && verify; }; then
    echo "pg-reference: fetching PostgreSQL $version ($tag) source" >&2
    tmp="$tarball.part"
    if command -v curl >/dev/null; then
        curl -fsSL --retry 3 -o "$tmp" "$url"
    else
        wget -q -O "$tmp" "$url"
    fi
    mv "$tmp" "$tarball"
    verify || { echo "pg-reference: sha256 mismatch for $tarball (expected $sha)" >&2; rm -f "$tarball"; exit 1; }
fi

echo "pg-reference: extracting to crates/postgres-$version-reference" >&2
work="$DEST.tmp.$$"
rm -rf "$work" && mkdir -p "$work"
tar -xzf "$tarball" -C "$work" --strip-components=1
[ -f "$work/configure.ac" ] && [ -d "$work/src/test/regress" ] || {
    echo "pg-reference: archive layout unexpected" >&2; rm -rf "$work"; exit 1; }
cat "$OVERLAY/postgresql.conf.sample.pgrust" >> "$work/src/backend/utils/misc/postgresql.conf.sample"
cp "$OVERLAY/README.md" "$work/README-WHY-THIS-IS-HERE.md"
echo "$want" > "$work/.pgrust-reference-stamp"
rm -rf "$DEST" && mv "$work" "$DEST"
echo "$DEST"
