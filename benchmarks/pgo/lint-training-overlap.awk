# Canonicalizer + set-difference for pgo/lint-training-overlap.sh.
# Driven by the environment: PGO_DENY_FILES (newline-separated paths) and
# PGO_EXEMPT_FILE (a path or empty). POSIX awk only (macOS and Linux).
#
# Two rules, both over whole statements (one per line in the corpus files; a
# statement that spans lines is joined first, see the main rule below).
#
# 1. Exact canonical form (every denylist file). String literals, numbers and
#    every parameter placeholder collapse to "@"; harness scaffolding (leading
#    RESET ALL; / SET ...;) is stripped; case is folded; punctuation is spaced
#    out and whitespace collapsed. What survives is the identifier-and-operator
#    skeleton, so a denied statement cannot be smuggled in by changing a search
#    term, a bound, or a binding.
#
# 2. Column-blind skeleton (denylist files that carry a "-- pgo-lint:
#    column-blind" or a "-- pgo-lint: skeletons" line; checked for every training file except the
#    transactional family, oltp-*). Starting from the canonical form, every
#    identifier that is not an SQL keyword and not a function name (a word
#    followed by "(") becomes the one token "I" — table names, column names
#    and aliases alike. A handful of rewrites that change no statement's shape
#    are folded too, so they cannot be used to step around the rule: AS, ASC,
#    INNER and OUTER are dropped; a qualified name (t.col) is one identifier;
#    an alias right after a column, a ")" , END or a literal is dropped; "!="
#    is "<>"; a negative literal, a decimal literal and a list of literals
#    ("IN (1, 2, 3)") are one literal; the field of extract(<field> FROM ...)
#    is a literal, as date_trunc's already is. A training statement whose
#    skeleton equals a denied statement's is the published query with its
#    columns renamed (the near-copy failure), and fails the lint.
#
#    A "-- pgo-lint: skeletons" file holds the skeletons themselves, one per
#    line, as `lint-training-overlap.sh --skeletons <file>` prints them: no
#    table, column or literal survives in it, and since equal canonical forms
#    have equal skeletons it denies exact copies as well as renamed ones. Every
#    line must be a fixed point of the reduction (the skeleton of itself), or
#    the lint refuses to run: a hand-edited line that could never match would
#    otherwise weaken the proof silently.
#
#    A denylist file may also carry "-- pgo-lint: exact-sha256 <hex>" lines: the
#    sha256 of a denied statement's canonical form (rule 1), so a file can deny
#    exact copies (in every training file, oltp-* included) without holding the
#    statement text. The driver hashes each training statement's canonical form
#    (PGO_PRINT_CANON, below) and passes the hashes in PGO_TRAIN_HASHES.
#
# PGO_PRINT_SKELETONS=1 prints the skeleton of every statement of the input
# files instead of linting them (no denylist is read). PGO_PRINT_CANON=1 prints
# "<file>:<line> TAB <canonical form>" for every statement (no denylist is read).
#
# The transactional family is exact-only: its statements are a few tokens
# long (a point lookup is "SELECT I FROM I WHERE I = @"), so any OLTP workload
# shares a column-blind skeleton with some published statement and the match
# carries no information. Matches there are printed as NOTE lines, counted,
# and do not fail the lint (pgo/README.md, "The column-blind guard").

function canon(s,   t) {
  t = s
  gsub(/'([^']|'')*'/, "@", t)            # string literals (before comment cut)
  sub(/--.*$/, "", t)                     # line comment
  t = tolower(t)
  while (match(t, /^[ \t]*reset[ \t]+all[ \t]*;/) || match(t, /^[ \t]*set[ \t][^;]*;/))
    t = substr(t, RSTART + RLENGTH)
  gsub(/\$[0-9]+/, "@", t)                # $1 placeholders
  gsub(/:[a-z_][a-z0-9_]*/, "@", t)       # :name placeholders
  gsub(/\?/, "@", t)                      # ? placeholders
  gsub(/[0-9]+/, "@", t)                  # numeric literals
  gsub(/[(),=<>+*\/;.|-]/, " & ", t)      # space out punctuation
  gsub(/[ \t]+/, " ", t)
  gsub(/^ +| +$/, "", t)
  gsub(/( @)+$/, "", t)                   # trailing normalized ";"
  gsub(/^ +| +$/, "", t)
  return t
}

# Column-blind skeleton of a canonical form (rule 2).
function cblind(c,   t, n, i, tok, nxt, m, s) {
  t = c
  gsub(/[:%!~^&#{}\[\]]/, " & ", t)       # the operators canon() leaves glued
  n = split(t, T, " ")
  m = 0
  for (i = 1; i <= n; i++) {
    tok = T[i]
    nxt = (i < n) ? T[i + 1] : ""
    if (tok ~ /^["a-z_]/) {
      if (m >= 2 && O[m] == "(" && O[m - 1] == "extract") tok = "@"   # extract(<field> FROM ...)
      else if (tok in DROPKW) continue
      else if (!(tok in KW) && (nxt != "(" || tok == "i")) tok = "I"   # "i": a skeleton's I re-read (with I ( ...)
    } else if (tok == "-" && nxt == "@" && !(m > 0 && (O[m] == "I" || O[m] == "@" || O[m] == ")"))) {
      continue                                                       # a negative literal
    } else if (tok == "=" && m > 0 && O[m] == "!") {
      O[m] = "<"; tok = ">"                                          # != is <>
    }
    O[++m] = tok
  }
  s = " "
  for (i = 1; i <= m; i++) s = s O[i] " "
  while (gsub(/ I \. I /, " I ", s) > 0) {}                          # qualified name
  while (gsub(/ \) I /, " ) ", s) > 0) {}                            # alias after an expression
  while (gsub(/ end I /, " end ", s) > 0) {}
  while (gsub(/ @ I /, " @ ", s) > 0) {}
  while (gsub(/ I I /, " I ", s) > 0) {}                             # alias after a column / relation
  while (gsub(/ @ \. @ /, " @ ", s) > 0) {}                          # decimal literal
  while (gsub(/ \( @ , @ /, " ( @ ", s) > 0) {}                      # list of literals
  gsub(/^ +| +$/, "", s)
  return s
}

function isstmt(s) { return (s ~ /[a-z]/) }

function basename(p,   b) { b = p; sub(/.*\//, "", b); return b }

BEGIN {
  # Keywords stay themselves in the column-blind skeleton (rule 2); every other
  # word not followed by "(" is an identifier. Words as canon() leaves them
  # (digits are already "@": int8 is "int@").
  kw = "select from where group by order having limit offset fetch first next rows row only ties with" \
       " and or not in is null true false like ilike similar escape between symmetric case when then else end" \
       " distinct all any some exists union intersect except join left right full cross natural on using" \
       " lateral over partition range groups preceding following unbounded current window filter within" \
       " desc nulls last values default cast extract interval date time timestamp timestamptz zone at" \
       " collate recursive materialized rollup cube grouping sets insert into update set delete returning" \
       " create table index unique primary key references view drop if exists begin commit rollback" \
       " vacuum analyze copy to for no others exclude ordinality tablesample" \
       " varying character char varchar text integer int int@ bigint smallint numeric decimal real" \
       " float float@ double precision boolean bool bytea json jsonb uuid serial bigserial" \
       " current_date current_timestamp current_time localtimestamp localtime user"
  nk = split(kw, KWL, " ")
  for (i = 1; i <= nk; i++) KW[KWL[i]] = 1
  split("as asc inner outer", DL, " ")
  for (i in DL) DROPKW[DL[i]] = 1
  if (ENVIRON["PGO_PRINT_SKELETONS"] != "") { printmode = 1; nfiles = 0 }
  else if (ENVIRON["PGO_PRINT_CANON"] != "") { printmode = 2; nfiles = 0 }
  else loaddeny()
}

function loaddeny(   nd, df, i, blind, skel, line, c, b, s, h, tf, k) {
  nd = split(ENVIRON["PGO_DENY_FILES"], df, "\n")
  for (i = 1; i <= nd; i++) {
    if (df[i] == "") continue
    blind = 0; skel = 0
    while ((getline line < df[i]) > 0) {
      if (line ~ /^[ \t]*--[ \t]*pgo-lint:[ \t]*column-blind/) blind = 1
      if (line ~ /^[ \t]*--[ \t]*pgo-lint:[ \t]*skeletons/) skel = 1
    }
    close(df[i])
    if (blind || skel) nblindfiles++
    while ((getline line < df[i]) > 0) {
      if (line ~ /^[ \t]*--[ \t]*pgo-lint:[ \t]*exact-sha256[ \t]/) {
        h = line
        sub(/^[ \t]*--[ \t]*pgo-lint:[ \t]*exact-sha256[ \t]+/, "", h); sub(/[ \t]+$/, "", h)
        if (h !~ /^[0-9a-f]+$/ || length(h) != 64) {
          printf "pgo-lint: FATAL %s: malformed exact-sha256 line: %s\n", df[i], substr(line, 1, 150) > "/dev/stderr"
          fatal = 1; exit 2
        }
        if (!(h in DENYH)) { ndeny++; ndenyh++; DENYH[h] = df[i] }
        continue
      }
      if (line ~ /^[ \t]*--/ || line ~ /^[ \t]*$/) continue
      if (skel) {
        s = line
        gsub(/[ \t]+/, " ", s); gsub(/^ +| +$/, "", s)
        if (cblind(canon(s)) != s) {
          printf "pgo-lint: FATAL %s: not a column-blind skeleton (regenerate it with --skeletons): %s\n", df[i], substr(s, 1, 150) > "/dev/stderr"
          fatal = 1; exit 2
        }
        if (!(s in DENYCB)) { ndenycb++; DENYCB[s] = df[i]; DENYCBSRC[s] = "(skeleton) " substr(s, 1, 140) }
        continue
      }
      c = canon(line)
      if (!isstmt(c)) continue
      if (!(c in DENY)) ndeny++
      DENY[c] = df[i]
      if (blind) {
        b = cblind(c)
        if (!(b in DENYCB)) { ndenycb++; DENYCB[b] = df[i]; DENYCBSRC[b] = substr(line, 1, 150) }
      }
    }
    close(df[i])
  }
  tf = ENVIRON["PGO_TRAIN_HASHES"]
  if (ndenyh > 0) {
    if (tf == "") {
      print "pgo-lint: FATAL the denylist carries exact-sha256 lines but no PGO_TRAIN_HASHES file was given (run the lint through lint-training-overlap.sh)" > "/dev/stderr"
      fatal = 1; exit 2
    }
    while ((getline line < tf) > 0) {
      k = index(line, "\t")
      if (k > 0) TH[substr(line, k + 1)] = substr(line, 1, k - 1)
    }
    close(tf)
  }
  ne = 0
  exemptfile = ENVIRON["PGO_EXEMPT_FILE"]
  if (exemptfile != "") {
    while ((getline line < exemptfile) > 0) {
      if (line ~ /^[ \t]*#/ || line ~ /^[ \t]*$/) continue
      c = canon(line)
      if (!isstmt(c)) continue
      EX[c] = 1; ne++
    }
    close(exemptfile)
  }
  if (ndeny == 0) {
    print "pgo-lint: FATAL denylist parsed to 0 statements" > "/dev/stderr"
    fatal = 1; exit 2
  }
  if (ndenycb == 0) {
    print "pgo-lint: FATAL no denylist file is marked \"-- pgo-lint: column-blind\" or \"-- pgo-lint: skeletons\" (the near-copy rule would examine nothing)" > "/dev/stderr"
    fatal = 1; exit 2
  }
  hits = 0; cbhits = 0; ntrain = 0; ncb = 0; nexempted = 0; nfiles = 0; nnote = 0
}

# One training statement: `clean` has its literals collapsed and its comments cut line by line (canon() is
# idempotent on it), `raw` is the source text for the report, `where` its file:line.
function stmt(clean, raw, where,   c, b, den) {
  c = canon(clean)
  if (!isstmt(c)) return
  if (printmode == 1) { print cblind(c); return }
  if (printmode == 2) { print where "\t" c; return }
  ntrain++
  SEEN[c] = 1
  den = ""
  if (c in DENY) den = DENY[c]
  else if ((where in TH) && (TH[where] in DENYH)) den = DENYH[TH[where]] " (exact-sha256)"
  if (den != "") {
    if (c in EX) {
      nexempted++
      printf "pgo-lint: EXEMPT  %s  %s\n", where, substr(raw, 1, 110)
      return
    }
    hits++
    printf "pgo-lint: OVERLAP %s\n", where
    printf "          training : %s\n", substr(raw, 1, 180)
    printf "          denied by: %s\n", den
    printf "          canonical: %s\n", substr(c, 1, 180)
    return
  }
  b = cblind(c)
  if (oltp) {
    if (b in DENYCB) {
      nnote++
      if (!(b in NOTED)) { NOTED[b] = 1; NOTEFILE[b] = where; nnoted++ }
      NOTECOUNT[b]++
    }
    return
  }
  ncb++
  if (b in DENYCB) {
    if (c in EX) {
      nexempted++
      printf "pgo-lint: EXEMPT  %s  %s\n", where, substr(raw, 1, 110)
      return
    }
    cbhits++
    printf "pgo-lint: NEAR-COPY %s\n", where
    printf "          training : %s\n", substr(raw, 1, 180)
    printf "          denied   : %s\n", DENYCBSRC[b]
    printf "          denied by: %s\n", DENYCB[b]
    printf "          skeleton : %s\n", substr(b, 1, 180)
  }
}

function flush() {
  if (buf != "") stmt(buf, rawbuf, where)
  buf = ""; rawbuf = ""
}

# A statement runs until a line that ends in ";" or carries a psql meta-command (\parse, \g), so a
# statement split over several lines is compared whole (a fixture's DDL, or a denied statement
# reflowed onto two lines). A line that IS a meta-command (\set, \o, \if) ends any open statement.
FNR == 1 { flush(); nfiles++; oltp = (basename(FILENAME) ~ /^oltp-/) }

{
  if ($0 ~ /^[ \t]*\\/) { flush(); next }
  if ($0 ~ /^[ \t]*--/ || $0 ~ /^[ \t]*$/ || $0 ~ /^[ \t]*#/) next
  t = $0
  gsub(/'([^']|'')*'/, "@", t)
  sub(/--.*$/, "", t)
  if (buf == "") { where = FILENAME ":" FNR; buf = t; rawbuf = $0 }
  else { buf = buf " " t; rawbuf = rawbuf " " $0 }
  if (t ~ /;[ \t]*$/ || t ~ /\\[a-z]/) flush()
}

END {
  if (fatal) exit 2        # an exit in BEGIN still runs END: never let it print a PROOF
  flush()
  if (printmode) exit 0
  for (b in NOTED)
    printf "pgo-lint: NOTE transactional skeleton shared with a denied analytical statement (not enforced on oltp-*): %d statement(s), first %s\n          skeleton : %s\n          denied   : %s\n", NOTECOUNT[b], NOTEFILE[b], b, DENYCBSRC[b]
  if (hits > 0 || cbhits > 0) {
    printf "pgo-lint: FAIL - %d training statement(s) are published measurement statements and %d are published statements with their columns renamed (%d training stmts over %d file(s) vs %d denied forms, %d denied column-blind skeletons)\n", hits, cbhits, ntrain, nfiles, ndeny, ndenycb > "/dev/stderr"
    exit 1
  }
  ndistinct = 0
  for (k in SEEN) ndistinct++
  printf "pgo-lint: PROOF disjoint - %d training statements (%d distinct canonical forms) over %d file(s); 0 of %d denied canonical forms present; 0 of %d denied column-blind skeletons present (%d statements checked column-blind)", ntrain, ndistinct, nfiles, ndeny, ndenycb, ncb
  if (nexempted > 0) printf "; %d documented exemption(s) applied", nexempted
  if (nnote > 0) printf "; %d transactional statement(s) in %d skeleton(s) noted, not enforced", nnote, nnoted
  printf "\n"
  exit 0
}
