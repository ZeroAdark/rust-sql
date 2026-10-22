//! Row selection in `GranuleRangeSource`: the rows the predicates and the delete mask keep, and the sparse stage.

use super::*;
use super::raw::{mask_ne_u32, mask_range_i16, mask_range_i32, mask_table_u32};

impl GranuleRangeSource {
    /// The rows the predicates keep (the int comparisons at the lane's width, a code lane's `<>` against the constant's
    /// code in the part, NULL never passes) and the delete mask leaves, as `sel` / `sel16`, when at most `rows / SELECT_DIV`:
    /// (selective, survivors). The kernel still tests every predicate per row.
    /// H06 (the code verdict, `VERDICT_CODE_TABLE`): the leading code qual's table of the part drops a row whose code it
    /// rejects too, any code verdict (an EQ code set, a byte range, a LIKE), in an `AllMatch` window as well (the zone
    /// decides the zone predicates, never that qual).
    pub(super) fn row_select(&mut self, p: usize, rows: usize) -> PgResult<(bool, usize)> {
        let table = self.row_table(p);
        (self.sel_exact, self.enc_fell) = (false, false);
        // D1536: a counting execution (EXPLAIN ANALYZE, `GranuleRecipe::skipped`) selects no row inside a granule unless its
        // scan runs to its end (VERDICT_COUNT_SELECT, whose caller counts the rows dropped): every row reaches the program,
        // whose count of what its qual removes stops where the scan stops (a LIMIT's last window)
        if self.verdict != V_MIXED && (self.verdict != V_ALL || table.is_none()) || self.recipe.skipped.is_some() && !self.on(VERDICT_COUNT_SELECT) {
            return Ok((false, 0));
        }
        let (mut lanes, mut codes) = (0u64, [u32::MAX; ROW_PREDS]);
        let nzone = if self.verdict == V_MIXED { self.nzone } else { 0 };
        if table.is_some() {
            fires::fire_shared("pgrc.code_select");
        }
        // bounded("small-fixed: the zone predicates the row selection applies, at most ROW_PREDS (64)")
        for zi in 0..nzone.min(ROW_PREDS) {
            let z = self.zone[zi];
            let k = z.lane as usize;
            if k < self.recipe.natts && self.recipe.kinds[k] == lane::KIND_CODES {
                if self.elided.has(k) || self.recipe.shred_of(k).is_some() {
                    continue;
                }
                let Some(c) = self.row_code(p, zi)? else { continue };
                codes[zi] = c;
                fires::fire_shared("pgrc.code_select");
            } else if !self.row_pred(&z) {
                continue;
            }
            lanes |= 1 << zi;
        }
        let n = match self.win {
            // the PSMA window (H02c): no row outside it passes, so the window is always a selection
            Some((lo, hi)) => {
                let (lo, hi) = (lo as usize, (hi as usize).min(rows));
                let w0 = lo / 64;
                self.mask[..rows.div_ceil(64)].fill(0);
                let kept = self.apply_preds(lanes, &codes, table, w0 * 64, hi);
                let below = self.mask[w0] & ((1u64 << (lo % 64)) - 1);
                self.mask[w0] &= !below;
                kept - below.count_ones() as usize
            }
            None => {
                if lanes == 0 && table.is_none() {
                    return Ok((false, 0));
                }
                let sample = rows.min(SAMPLE_ROWS);
                let kept = self.apply_preds(lanes, &codes, table, 0, sample);
                if kept * SELECT_DIV > sample {
                    if table.is_some() {
                        self.table_dense = p;
                    }
                    return Ok((false, 0));
                }
                let n = kept + if sample < rows { self.apply_preds(lanes, &codes, table, sample, rows) } else { 0 };
                if n * SELECT_DIV > rows {
                    if table.is_some() {
                        self.table_dense = p;
                    }
                    return Ok((false, 0));
                }
                n
            }
        };
        self.sel16_ok = false;
        let k = words_to_sel32(&self.mask[..rows.div_ceil(64)], &mut self.sel, &mut self.sel16);
        debug_assert_eq!(k, n, "the selection is apply_preds' kept rows");
        // D3223 S2: the selection is exact (every selected row satisfies every zone predicate of the window, and every other
        // row fails one) when it applied every zone predicate (none a prefix, hashed, unsigned-key or by-reference one, a
        // code lane's only as `<>` with its code in the part, at most ROW_PREDS of them), took no code verdict table (a table's faults pass rows)
        // and no encoded lane fell back to the mask as it was (`enc_mask`)
        let all = if self.nzone >= 64 { u64::MAX } else { (1u64 << self.nzone) - 1 };
        self.sel_exact = table.is_none() && self.verdict == V_MIXED && self.nzone <= ROW_PREDS && lanes == all && !self.enc_fell;
        Ok((true, n))
    }

    /// Zone predicate `zi`'s code in part `p` when `row_select` applies it on the decoded code lane: a `<>` whose constant
    /// `pin_codes` placed in the part's byte-rank dictionary, or a record lane's, placed here once a part (`ne_codes`) in
    /// the lane's open dictionary, whose index the claim run staged (`u32::MAX` where the dictionary lacks it: no row
    /// holds it).
    fn row_code(&mut self, p: usize, zi: usize) -> PgResult<Option<u32>> {
        if let Some(c) = self.zcode_preds.get(zi).and_then(Option::as_ref).filter(|c| self.zcode_part == p && c.op == ZoneOp::Ne) {
            return Ok(Some(u32::try_from(c.value).unwrap_or(u32::MAX)));
        }
        let z = self.zone[zi];
        if z.op != ZoneOp::Ne || !z.prefix || zi >= ROW_PREDS || !self.record_lanes.has(z.lane as usize) {
            return Ok(None);
        }
        if self.ne_part != p {
            (self.ne_part, self.ne_placed) = (p, 0);
        }
        if self.ne_placed >> zi & 1 == 0 {
            self.ne_codes[zi] = self.ne_place(p, &z)?;
            self.ne_placed |= 1 << zi;
        }
        Ok(self.ne_codes[zi])
    }

    /// Record-lane predicate `z`'s constant (its one image, as `pin_codes` reads it) placed in lane `z.lane`'s dictionary in
    /// part `p`: `code_pred` over a byte-ordered part dictionary; in a verbatim part's table, ranked by bytes or by length,
    /// only '', which sorts first in either order (code 0 when the part holds it). `None`: the program's filter tests the rows.
    fn ne_place(&self, p: usize, z: &ZonePred) -> PgResult<Option<u32>> {
        let mut hits = self.zcode_images.iter().filter(|c| c.lane == z.lane && c.op == z.op && c.key == z.value);
        let Some(c) = hits.next() else { return Ok(None) };
        if hits.any(|o| o.image != c.image) {
            return Ok(None);
        }
        let Some(entry) = self.dict_at(p, z.lane as usize) else { return Ok(None) };
        Ok(match &entry.dict {
            Dict::Part(d) if d.byte_rank_sorted() => super::verdicts::code_pred(d, z, &c.image)?.map(|w| u32::try_from(w.value).unwrap_or(u32::MAX)),
            Dict::Part(_) => None,
            Dict::Ranked { table, .. } if c.image.is_empty() => table.ranks().map(|r| if r.has_empty() { 0 } else { u32::MAX }),
            Dict::Ranked { .. } => None,
        })
    }

    /// A zone predicate `row_select` applies per row over a by-value lane (its `lanes`; a code lane's is `row_code`'s).
    pub(super) fn row_pred(&self, z: &ZonePred) -> bool {
        let k = z.lane as usize;
        // NUMZONE (V4R-28): a numeric lane's mantissas at its scale compare as its i64 words, in a packed granule only (a
        // verbatim part's lane holds image pointers); float / bool fixed lanes never carry a predicate
        // SRC-09: a prefix predicate's value is a key, never the lane's word; NEW-47: so is an unsigned word's (oid), its
        // zero-extended key, where the lane's cells compare signed at their width. P5-11: a probe word is no value
        !(z.prefix || z.is_hashed() || k >= self.recipe.natts || self.elided.has(k) || self.recipe.kinds[k] > lane::KIND_CODES && !(self.recipe.numeric.has(k) && !self.lane_byref.has(k)) || self.unsigned_key(k))
    }

    /// D3223 S1 (the encoded row filter): the zone lanes of granule `lg` of part `p` that `row_select` compares on their
    /// stored deltas instead of decoding them whole first, each leaving `first` for `enc` (it decodes after the selection
    /// with the rest lanes: the selected rows gathered, or whole when the window is no selection). A lane qualifies when
    /// every predicate `row_select` applies to it is a range or a NULL test (a `<>` reads the decoded cells), it is a by-value
    /// word of width 2 / 4 / 8 stored BYTE_FOR or a numeric lane stored PACKED_NUMERIC at the lane's scale, at delta width
    /// 1 / 2 / 4, a root stream the cursor resolves framed (`StreamCursor::framed`), and every frame's reference `r` lies in
    /// the lane's cell domain with `r + 2^(8w) - 1` inside it too: then every row's cell is exactly `r + delta` (no
    /// narrowing or wrap), so `lo <= cell <= hi` is `lo - r <= delta <= hi - r` (`enc_mask`). Its validity is read here
    /// (`apply_preds` reads it). A lane that does not qualify, or whose read fails here, decodes first as before.
    pub(super) fn enc_stage(&mut self, p: usize, lg: u32, rows: usize) {
        let natts = self.recipe.natts;
        let nz = self.nzone.min(ROW_PREDS);
        // bounded("small-fixed: the zone predicates the row filter compares, at most ROW_PREDS (64)")
        for zi in 0..nz {
            let z = self.zone[zi];
            let k = z.lane as usize;
            // a shred lane's cells may not be its stream's values (a presence or octet lane, an absent path)
            if !self.row_pred(&z) || self.enc_set.has(k) || self.recipe.kinds[k] == lane::KIND_CODES || pred_bounds(&z).is_none() || self.recipe.shred_of(k).is_some() {
                continue;
            }
            if self.zone[..nz].iter().any(|y| y.lane == z.lane && y.op == ZoneOp::Ne && self.row_pred(y)) {
                continue;
            }
            let numeric = self.recipe.numeric.has(k);
            let (dom_lo, dom_hi) = match (numeric, self.recipe.kinds[k], self.recipe.widths[k]) {
                (true, _, _) | (false, lane::KIND_BYVAL, 8) => (i64::MIN, i64::MAX),
                (false, lane::KIND_BYVAL, 4) => (i32::MIN as i64, i32::MAX as i64),
                (false, lane::KIND_BYVAL, 2) => (i16::MIN as i64, i16::MAX as i64),
                _ => continue,
            };
            let enc = (if numeric { EncodingId::PackedNumeric } else { EncodingId::ByteFor }) as u16;
            let scale = numeric.then(|| self.lane_scale(p, k) as i32);
            let Some(cur) = self.cursors[p * natts + k].as_deref_mut() else { continue };
            let w = cur.key().width;
            if cur.key().encoding != enc || !matches!(w, 1 | 2 | 4) || cur.rows_in_granule(lg) as usize != rows {
                continue;
            }
            let Ok(Some(f)) = cur.framed(lg, enc) else { continue };
            let f = pgrc_codec::bytefor::Framed { payload: f.payload, frame_table: f.frame_table, fbase: f.fbase, values: f.values };
            if f.values as usize != rows || scale.is_some_and(|s| pgrc_codec::packednum::framed_scale(&f).ok() != Some(s)) {
                continue;
            }
            let top = dom_hi - ((1i64 << (8 * w)) - 1);
            if pgrc_codec::bytefor::each_frame(&f, w, |_, r, _| (dom_lo..=top).contains(&(r as i64))).ok() != Some(true) {
                continue;
            }
            let Ok(v) = (if cur.has_validity_stream() { cur.validity(lg, &mut self.valid[k * VALIDITY_WORDS..(k + 1) * VALIDITY_WORDS]).map(|v| matches!(v, ValidityVerdict::AllValid)) } else { Ok(true) }) else { continue };
            self.all_valid[k] = v;
            self.enc.push(EncLane { lane: k as u32, part: p as u32, lg, enc, w }); // alloc: Node (kept at its high-water mark)
            self.enc_set.set(k);
            self.first.unset(k);
        }
        if !self.enc.is_empty() {
            fires::fire_shared("pgrc.enc_select"); // shapecheck fire signal (D3223 S1): row_select compares a zone lane on its stored deltas
        }
    }

    /// Road code.sparse's host half (H08 / P5-04 / SRC-22, `VERDICT_CODE_SPARSE_TABLE`): a leading qual answered whole by
    /// site table `t`, a host-built per-code `Bits` table over code lane `k` of a part dictionary (an EQ code set, a
    /// comparison with any constant among them, a LIKE, a byte range: lowerv4 dict/strategies/code_sparse.rs `table`), at
    /// any zone count, with no PSMA window and no covering condition-cache hit. A granule whose block base that table
    /// rejects for certain (bit clear, no fault: `Bits::rejects`) holds no survivor among its base rows; the walk selects
    /// the non-base rows whose code the table may pass (hosts v4 m3_codes.rs, their codes at their positions in lane `k`),
    /// so the selection is the qual's survivors and its faults, less the rows NULL and the deletes drop; `(k, n)`. `None`
    /// otherwise (the whole decode and row_select). The program tests every qual on them. The part's table is
    /// `cond_table`'s (the lane fill's own, cached by part and spec), kept for the part in `sparse_tab`.
    /// `cost::code_sparse_dense_pct` keeps it off parts with no dominant value. A condition-cache hit (covering or mixed:
    /// `cond_hit` is V_ALL) keeps the whole decode: its selection is known, and the non-base walk costs more than the bulk
    /// unpack at narrow lane widths (about 20% slower when measured).
    pub(super) fn sparse_stage(&mut self, p: usize, lg: u32, rows: usize) -> PgResult<Option<(usize, usize)>> {
        let Some(t) = code_sparse_table(self.enables) else { return Ok(None) };
        let k = self.recipe.tables.get(t).map_or(usize::MAX, |s| s.over as usize);
        if self.win.is_some() || self.cond_hit == V_ALL || k >= self.recipe.natts || self.elided.has(k) || self.recipe.kinds[k] != lane::KIND_CODES || self.lane_all.has(k) || self.recipe.entries == Some(k) {
            return Ok(None);
        }
        // a mixed or all-match zone window, any zone count; the code lane one the window decodes first; not in a part a
        // granule of which showed no dominant base (`sparse_dense`)
        if (self.verdict != V_MIXED && self.verdict != V_ALL) || !self.first.has(k) || self.sparse_dense == p {
            return Ok(None);
        }
        // a lane the selection gathers: on a code-only window the whole decode and the per-row test are cheaper
        if !(0..self.recipe.natts).any(|l| l != k && !self.elided.has(l) && self.recipe.kinds[l] != lane::KIND_CODES) {
            return Ok(None);
        }
        let entry = self.dict_at(p, k).expect("dictionary");
        if !matches!(entry.dict, Dict::Part(_)) {
            return Ok(None);
        }
        // the first cost choice, before any walk: a part whose dictionary holds more values than
        // `code_sparse_dense_pct()` % of its rows (a near-unique column) has no value covering the rest at the part level, so
        // it decodes whole (a heuristic: a clustered granule can still hold a dominant base; the walk's guard below is exact)
        let (dict_codes, dense_pct) = (entry.dict.ncodes() as u64, gravity_program::cost::code_sparse_dense_pct());
        if self.parts().part_if_open(p).is_some_and(|part| dict_codes * 100 > part.rows() as u64 * dense_pct) {
            self.sparse_dense = p;
            return Ok(None);
        }
        let Some(verdict) = self.sparse_table(p, t)? else { return Ok(None) };
        let cur = self.cursors[p * self.recipe.natts + k].as_deref_mut().expect("cursor");
        if cur.rows_in_granule(lg) as usize != rows {
            return Err(err("gravity: pgrc granule row skew"));
        }
        let words = rows.div_ceil(64);
        let codes = crate::ops::scan::pgrc::raw::words_as_u32_mut(&mut self.datums[k * ROWS..(k + 1) * ROWS], rows);
        let Some(b) = verdict.bits() else { return Ok(None) };
        // the walk selects the rows the table may pass (bit set, or a fault the program re-tests), not every non-base row
        let mask = &mut self.mask[..words];
        let walked = crate::dict::strategies::code_sparse::sparse_block(cur, lg, rows, |base| b.rejects(base), |c| !b.rejects(c), codes, mask);
        let Some(dropped) = walked.map_err(read_err)? else {
            return Ok(None);
        };
        // the non-base rows: those the walk selected and those the table dropped (the dense guard below)
        let nonbase = dropped + self.mask[..words].iter().map(|w| w.count_ones() as usize).sum::<usize>();
        let cur = self.cursors[p * self.recipe.natts + k].as_deref_mut().expect("cursor");
        self.all_valid[k] = !cur.has_validity_stream() || matches!(cur.validity(lg, &mut self.valid[k * VALIDITY_WORDS..(k + 1) * VALIDITY_WORDS]).map_err(read_err)?, ValidityVerdict::AllValid);
        // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
        for w in 0..words {
            let v = if self.all_valid[k] { !0 } else { self.valid[k * VALIDITY_WORDS + w] };
            self.mask[w] &= v & if self.has_dv { !self.dv[w] } else { !0 };
        }
        self.sel16_ok = false;
        fires::fire_shared("pgrc.sparse_stage");
        let n = words_to_sel32(&self.mask[..words], &mut self.sel, &mut self.sel16);
        // the second cost choice: a part whose granule held more than `dense_pct` % of its rows as non-base
        // has no dominant base, and the walk costs more than the whole decode it replaces: its later granules decode whole
        if nonbase as u64 * 100 > rows as u64 * dense_pct {
            self.sparse_dense = p;
        }
        Ok(Some((k, n)))
    }

    /// The table form's verdict table `t` for part `p` (`sparse_stage`): `cond_table`'s, kept for the part; `None` when it
    /// is no host-built `Bits` table over a part dictionary (the window decodes whole).
    pub(super) fn sparse_table(&mut self, p: usize, t: usize) -> PgResult<Option<Arc<crate::dict::text_hosts::FpTable>>> {
        if let Some((_, v)) = self.sparse_tab.as_ref().filter(|(q, _)| *q == p) {
            return Ok(Some(v.arc_clone()));
        }
        let Some(spec) = self.recipe.tables.get(t).cloned() else { return Ok(None) };
        // NEW-72 entry C: a bound code set or byte interval by this execution's values (the program wrote them before its seek)
        let spec = self.bound_spec(spec);
        if !matches!(spec.func, crate::dict::text_hosts::TableFn::Like { .. } | crate::dict::text_hosts::TableFn::Eq { .. } | crate::dict::text_hosts::TableFn::Range { .. }) {
            return Ok(None);
        }
        let k = spec.over as usize;
        // FASTADD: the lane's part (a stand-in for a column the part lacks)
        let part = self.lane_part(p, k)?;
        let tkey = self.table_key(p, k, &part)?;
        let Dict::Part(d) = &self.dict_at(p, k).expect("dictionary").dict else { return Ok(None) };
        let read_err_cell = core::cell::Cell::new(None);
        let view = crate::dict::text_hosts::DictView::Part(d, &read_err_cell);
        let v = crate::dict::text_hosts::cond_table_in(self.fp_memo(), &part, tkey, &spec, &view).map_err(table_err)?;
        if v.bits().is_none() {
            return Ok(None);
        }
        self.sparse_tab = Some((p, crate::dict::text_hosts::FpPin::of(&v)));
        Ok(Some(v))
    }

    /// Lane `k`'s zone values are zero-extended keys of an unsigned word (`UnsignedWordSmall`: oid), not its cells' order.
    pub(super) fn unsigned_key(&self, k: usize) -> bool {
        self.recipe.profiles.get(k).copied().flatten().is_some_and(|p| p.key == pgrc_meta::key::KeyDerivation::UnsignedWordSmall)
    }

    /// H06: the code verdict's table of part `p` over its decoded code lane (`VERDICT_CODE_TABLE`; the table `sparse_table`
    /// kept for the part, built at its entry by `code_part_none`), for `row_select`.
    fn row_table(&self, p: usize) -> Option<(usize, gravity_dict::table::Bits)> {
        let (_, k) = self.code_table()?;
        let (q, v) = self.sparse_tab.as_ref()?;
        (*q == p && self.table_dense != p && !self.elided.has(k)).then(|| v.bits().map(|b| (k, b))).flatten()
    }

    /// The mask words of rows `from..to` (`from` a multiple of 64): the delete mask, the predicates in `lanes`, the code
    /// verdict's `table` (H06), their lanes' validity; the rows kept.
    pub(super) fn apply_preds(&mut self, lanes: u64, codes: &[u32; ROW_PREDS], table: Option<(usize, gravity_dict::table::Bits)>, from: usize, to: usize) -> usize {
        let (w0, w1) = (from / 64, to.div_ceil(64));
        let mask = &mut self.mask[w0..w1];
        // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
        for (j, m) in mask.iter_mut().enumerate() {
            let n = to - (w0 + j) * 64;
            *m = if n < 64 { (1u64 << n) - 1 } else { !0 };
        }
        if self.has_dv {
            // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
            for (m, d) in mask.iter_mut().zip(&self.dv[w0..w1]) {
                *m &= !*d;
            }
        }
        let mut fused = 0u64;
        for (zi, z) in self.zone[..self.nzone].iter().enumerate().take(ROW_PREDS) {
            if lanes & (1 << zi) == 0 || fused & (1 << zi) != 0 {
                continue;
            }
            let k = z.lane as usize;
            let slot = &self.datums[k * ROWS..(k + 1) * ROWS];
            // L5a: the lane's comparison predicates in `lanes` as one inclusive range, one pass (a code lane's are `Ne`)
            let range = pred_bounds(z).filter(|_| self.recipe.kinds[k] != lane::KIND_CODES).map(|b| {
                self.zone[zi + 1..self.nzone.min(ROW_PREDS)].iter().enumerate().fold(b, |(lo, hi), (d, y)| {
                    let zj = zi + 1 + d;
                    match pred_bounds(y).filter(|_| y.lane == z.lane && lanes & (1 << zj) != 0) {
                        Some((l2, h2)) => {
                            fused |= 1 << zj;
                            (lo.max(l2), hi.min(h2))
                        }
                        None => (lo, hi),
                    }
                })
            });
            let enc = if self.enc_set.has(k) { self.enc.iter().find(|e| e.lane as usize == k).copied() } else { None };
            // the lane's slot holds `to` cells at its width; D3223 S1: an encoded lane's (`enc_stage`) is not decoded yet: its
            // range compares on its stored deltas, its other predicates are NULL tests (the validity below)
            match (self.recipe.kinds[k], self.recipe.widths[k], range) {
                _ if enc.is_some() => {
                    if let (Some((lo, hi)), Some(e)) = (range, enc) {
                        if !enc_mask(self.cursors[e.part as usize * self.recipe.natts + k].as_deref_mut(), e, mask, from, to, lo, hi) {
                            self.enc_fell = true;
                        }
                    }
                }
                (lane::KIND_CODES, _, _) => mask_ne_u32(mask, from, to, super::raw::cells::<u32>(slot, to), codes[zi]),
                (_, 4, Some((lo, hi))) => mask_range_i32(mask, from, to, super::raw::cells::<i32>(slot, to), lo, hi),
                (_, 2, Some((lo, hi))) => mask_range_i16(mask, from, to, super::raw::cells::<i16>(slot, to), lo, hi),
                (_, 1, Some((lo, hi))) => mask_range(mask, from, to, super::raw::cells::<i8>(slot, to), lo, hi),
                (_, _, Some((lo, hi))) => mask_range(mask, from, to, super::raw::cells::<i64>(slot, to), lo, hi),
                (_, 4, None) => int_pred(mask, from, to, super::raw::cells::<i32>(slot, to), z),
                (_, 2, None) => int_pred(mask, from, to, super::raw::cells::<i16>(slot, to), z),
                (_, 1, None) => int_pred(mask, from, to, super::raw::cells::<i8>(slot, to), z),
                _ => int_pred(mask, from, to, super::raw::cells::<i64>(slot, to), z),
            }
            // NULL never passes a comparison; `IS NULL` keeps the NULL rows alone
            let is_null = z.op == ZoneOp::IsNull;
            if is_null && self.all_valid[k] {
                mask.fill(0);
            } else if !self.all_valid[k] {
                // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
                for (m, v) in mask.iter_mut().zip(&self.valid[k * VALIDITY_WORDS + w0..k * VALIDITY_WORDS + w1]) {
                    *m &= if is_null { !*v } else { *v };
                }
            }
        }
        // H06: the code verdict's qual is strict, so a NULL row fails it as well
        if let Some((k, b)) = table {
            mask_table_u32(mask, from, to, super::raw::cells::<u32>(&self.datums[k * ROWS..(k + 1) * ROWS], to), &b);
            if !self.all_valid[k] {
                // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
                for (m, v) in mask.iter_mut().zip(&self.valid[k * VALIDITY_WORDS + w0..k * VALIDITY_WORDS + w1]) {
                    *m &= *v;
                }
            }
        }
        mask.iter().map(|m| m.count_ones() as usize).sum()
    }
}

/// D3223 S1: a zone lane `row_select` compares on its stored deltas this window (`enc_stage`): its part, granule, values
/// stream encoding (BYTE_FOR or PACKED_NUMERIC) and delta width (1 / 2 / 4).
#[derive(Clone, Copy, Debug)]
pub(super) struct EncLane {
    pub(super) lane: u32,
    pub(super) part: u32,
    pub(super) lg: u32,
    pub(super) enc: u16,
    pub(super) w: u8,
}

/// D3223 S1: `lo <= cell <= hi` over rows `from..to` (`from` a multiple of 64) of encoded lane `e` into `mask` (the words of
/// those rows), frame by frame on the stored deltas: frame reference `r` moves the range to `[lo - r, hi - r]`, clamped to
/// the deltas' `[0, 2^(8w) - 1]` (`enc_stage` proved `r + delta` is every row's exact cell); an empty range drops the
/// frame's rows, the whole delta range keeps them, else `raw::mask_delta`. Frames start at multiples of 1024 rows, so each
/// frame's rows start a mask word. A cursor that no longer resolves the granule framed leaves the mask as it is: a
/// superset, which the program narrows (it tests every predicate per row), and the lane's own decode reports the read:
/// `false` then (the selection is no exact one: `row_select`'s `sel_exact`).
fn enc_mask(cur: Option<&mut StreamCursor>, e: EncLane, mask: &mut [u64], from: usize, to: usize, lo: i64, hi: i64) -> bool {
    let Some(Ok(Some(f))) = cur.map(|c| c.framed(e.lg, e.enc)) else { return false };
    let f = pgrc_codec::bytefor::Framed { payload: f.payload, frame_table: f.frame_table, fbase: f.fbase, values: f.values };
    // a walk that stops early (a frame past `to`) or fails past the frames it masked: the frames walked are exact, a failed
    // one (a bound the enc_stage walk proved) leaves its rows unmasked
    pgrc_codec::bytefor::each_frame(&f, e.w, |first, r, deltas| frame_mask(mask, from, to, first, r, deltas, e.w, lo, hi)).is_ok()
}

/// `enc_mask`'s frame: the frame of reference `r` whose `deltas` (width `w`) are rows `first..` of the granule, its rows in
/// `from..to` into `mask` (the words of `from..to`); `false` once the frame starts at or past `to` (the walk stops).
#[allow(clippy::too_many_arguments)]
pub(super) fn frame_mask(mask: &mut [u64], from: usize, to: usize, first: usize, r: u64, deltas: &[u8], w: u8, lo: i64, hi: i64) -> bool {
    let (wb, dmax) = (w as usize, (1i128 << (8 * w)) - 1);
    let end = first + deltas.len() / wb;
    if first >= to {
        return false;
    }
    if end <= from {
        return true;
    }
    let (a, b) = (first.max(from), end.min(to));
    let words = &mut mask[(a - from) / 64..(b - from).div_ceil(64)];
    let r = r as i64 as i128;
    let (dlo, dhi) = ((lo as i128 - r).max(0), (hi as i128 - r).min(dmax));
    if dlo > dhi {
        words.fill(0);
    } else if dlo > 0 || dhi < dmax {
        super::raw::mask_delta(words, &deltas[(a - first) * wb..(b - first) * wb], w, dlo as u32, (dhi - dlo) as u32);
    }
    true
}
