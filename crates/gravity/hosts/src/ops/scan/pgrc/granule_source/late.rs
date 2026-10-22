//! Late materialization in `GranuleRangeSource`: lanes decoded after the selection, and the dictionary renders.

use super::*;

impl GranuleRangeSource {
    /// M2-ZONEREAD card A: the mapped lanes whose meta sections the claim entering part `p` at `g` reads: a lane with a
    /// predicate bloom (`=`) or PSMA (a range) could decide, and whose Stats / Bloom / PSMA the list's faces lack, when the
    /// map leaves one of the claim's granules Mixed on it (a granule another predicate rules out asks nothing). A part the
    /// map decides whole reads none. Under a walk order every such lane, as before.
    pub(super) fn late_meta_need(&mut self, list: &PartList, p: usize, g: u64) -> PgResult<()> {
        // D4554: the part's part-grain verdicts once (`verdicts_of` below asks them per granule)
        self.arm_part_dec(p);
        let mut need = core::mem::take(&mut self.meta_need);
        need.clear();
        let meta = self.on(VERDICT_BLOOM) || self.on(VERDICT_PSMA);
        if !meta || !self.mapped.any() {
            self.meta_need = need;
            return Ok(());
        }
        let mut want = core::mem::take(&mut self.scratch_a);
        want.clear();
        let mut stopped = false;
        let psma = |z: &ZonePred| self.on(VERDICT_PSMA) && !matches!(z.op, ZoneOp::Ne | ZoneOp::NotNull | ZoneOp::IsNull | ZoneOp::InMember | ZoneOp::HashEq | ZoneOp::HashIn);
        let applies = |z: &ZonePred| {
            self.on(VERDICT_BLOOM) && z.is_hashed()
                || !z.prefix && (self.on(VERDICT_BLOOM) && matches!(z.op, ZoneOp::Eq | ZoneOp::InMember) || psma(z))
                // P5-13: a fixed image's order-key predicate reads its PSMA (`psma_rows`)
                || z.prefix && psma(z) && self.fixed_keyed(z.lane)
        };
        let rp = self.parts();
        // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
        for z in self.zone[..self.nzone_decl].iter().filter(|z| applies(z)) {
            let k = z.lane as usize;
            let face = self.face(k);
            if self.mapped.has(k) && (rp.meta_if_read(p, face).is_none() || rp.body_if_read(p, face).is_none()) {
                want.set(k);
            }
        }
        if want.any() {
            if self.order.is_some() {
                need.assign(&want);
            } else {
                let start = list.starts()[p];
                let (lo, hi) = (g.saturating_sub(start), self.hi.min(list.end_of(p)).saturating_sub(start));
                let base = p * self.recipe.natts;
                // C9: a tick per granule of the claim (its verdicts over the zone predicates)
                let mut step = cancelpoll::step("granule_source stage: a claim's late meta need");
                'granules: for lg in lo as u32..hi as u32 {
                    if step.tick().is_err() {
                        stopped = true;
                        break;
                    }
                    let rows = self.cursors[base + self.rows_lane].as_deref().map_or(0, |c| c.rows_in_granule(lg));
                    if self.nzone > 0 && self.verdicts_of(p, lg, rows).0 == V_NONE {
                        continue;
                    }
                    // bounded("other: one zone predicate of the statement's own quals each (an IN list's member is one): the query text bounds them, and PG's ExecQual tests the same quals per row with no poll")
                    for z in self.zone[..self.nzone_decl].iter().filter(|z| applies(z) && want.has(z.lane as usize)) {
                        // a prefix predicate's record is no map's (the map holds exact keys): one granule left asks it
                        if z.prefix || self.map_verdict(p, lg, rows, z) == Some(V_MIXED) {
                            need.set(z.lane as usize);
                            if need.count() == want.count() {
                                break 'granules;
                            }
                        }
                    }
                }
            }
        }
        self.scratch_a = want;
        self.meta_need = need;
        if stopped {
            return Err(err(WALK_CANCELED));
        }
        Ok(())
    }

    /// M2-ZONEREAD card A: the faces of the lanes `late_meta_need` named: their sections came with the claim run into the
    /// segment cache (a lane the run did not carry reads them in one batch here, as the pin did), and the Stats body,
    /// bloom and PSMA faces are filled from it for the granule verdicts that follow.
    pub(super) fn load_late_meta(&mut self, p: usize) -> PgResult<()> {
        if !self.meta_need.any() {
            return Ok(());
        }
        // D3961 (C9): a part's late meta held a walk 668 ms on memtest's ladder: a step, a tick per lane read
        let mut step = cancelpoll::step("granule_source stage: a part's late meta");
        let rp = self.parts();
        let part = rp.part_if_open(p).expect("pinned");
        for k in 0..self.recipe.natts {
            if !self.meta_need.has(k) {
                continue;
            }
            let face = self.face(k);
            rp.prefault_meta(p, &[(face, true)], part, self.held.owner())?;
            rp.body(p, face, part)?;
            rp.meta(p, face, part)?;
            step.tick().map_err(|_| err(WALK_CANCELED))?;
        }
        Ok(())
    }

    /// decode.late: lane `k` of granule `lg` left for `lane_fill`: its kind and validity now (a window's `VALID` and its
    /// no-nulls fact are exact before any fill), no row decoded.
    pub(super) fn late_stage(&mut self, p: usize, k: usize, lg: u32, rows: usize) -> PgResult<()> {
        let cur = self.cursors[p * self.recipe.natts + k].as_deref_mut().expect("cursor");
        if cur.rows_in_granule(lg) as usize != rows {
            return Err(err("gravity: pgrc granule row skew"));
        }
        if self.recipe.numeric.has(k) && cur.key().encoding != EncodingId::PackedNumeric as u16 {
            self.lane_byref.set(k);
        }
        self.all_valid[k] = !cur.has_validity_stream() || matches!(cur.validity(lg, &mut self.valid[k * VALIDITY_WORDS..(k + 1) * VALIDITY_WORDS]).map_err(read_err)?, ValidityVerdict::AllValid);
        self.late_done[k * VALIDITY_WORDS..(k + 1) * VALIDITY_WORDS].fill(0);
        self.late_at[k] = (0, 0);
        (self.late_cnt[k], self.late_req[k], self.late_span[k], self.late_calls[k]) = (0, 0, 0, (0, 0));
        self.late_pend |= 1 << k;
        Ok(())
    }

    /// decode.late: pending lane `k`'s rows in `sel` (every row for none) decoded into the staged window at their
    /// positions, each row once per window; a DeltaFor lane decodes each touched frame whole (a frame's delta chain is
    /// its own), so a frame no survivor reaches is never decoded. A fill of every row (the straight body's), or of a lane
    /// whose fills so far are dense (`late_dense`), decodes the lane whole instead of gathering it (`late_whole`).
    pub(super) fn late_fill(&mut self, k: usize, sel: &[u32]) -> PgResult<u32> {
        if k >= 64 || self.late_pend >> k & 1 == 0 {
            return Ok(1);
        }
        let (p, lg, rows) = self.late_g;
        if sel.is_empty() {
            return self.late_whole(k, p, lg, rows);
        }
        // the asked rows' bits, and the words they reach: only those are walked (a block's fill touches one or two)
        let mut need = [0u64; VALIDITY_WORDS];
        let (mut lo, mut hi, mut asked) = (VALIDITY_WORDS, 0, 0u32);
        // bounded("small-fixed: the selected rows of one granule, at most GRANULE_ROWS (8192); the claim's walk ticks per granule")
        for &r in sel.iter().filter(|&&r| (r as usize) < rows) {
            let w = r as usize / 64;
            need[w] |= 1 << (r % 64);
            (lo, hi, asked) = (lo.min(w), hi.max(w), asked + 1);
        }
        if asked == 0 {
            return Ok(1);
        }
        // the frames holding a row no earlier fill decoded: the ones this fill unpacks
        let (fw, done) = (FRAME_VALUES as usize / 64, &self.late_done[k * VALIDITY_WORDS..(k + 1) * VALIDITY_WORDS]);
        let frames = (lo / fw..=hi / fw).filter(|f| (f * fw..((f + 1) * fw).min(VALIDITY_WORDS)).any(|w| need[w] & !done[w] != 0)).count() as u32;
        if self.late_dense(k, asked, (hi * 64 + 64 - need[hi].leading_zeros() as usize) as u32, frames) {
            return self.late_whole(k, p, lg, rows);
        }
        let natts = self.recipe.natts;
        let cur = self.cursors[p * natts + k].as_deref().expect("cursor");
        if cur.key().encoding == EncodingId::DeltaFor as u16 {
            // bounded("small-fixed: the frames of one granule, at most FRAMES_PER_GRANULE (8)")
            for f in lo / fw..=hi / fw {
                let words = &mut need[f * fw..((f + 1) * fw).min(VALIDITY_WORDS)];
                if words.iter().any(|&w| w != 0) {
                    words.fill(u64::MAX);
                }
            }
            (lo, hi) = (lo / fw * fw, ((hi / fw + 1) * fw).min(VALIDITY_WORDS) - 1);
        }
        let done = &mut self.late_done[k * VALIDITY_WORDS..(k + 1) * VALIDITY_WORDS];
        self.late_sel.clear();
        self.late_sel16.clear();
        let mut added = 0;
        // bounded("small-fixed: the validity words of one granule, at most VALIDITY_WORDS (128)")
        for w in lo..=hi {
            let (n, d) = (&mut need[w], &mut done[w]);
            let live = match rows.saturating_sub(w * 64) {
                0 => 0,
                l if l >= 64 => u64::MAX,
                l => (1u64 << l) - 1,
            };
            *n &= live & !*d;
            *d |= *n;
            added += n.count_ones();
            let mut m = *n;
            // bounded("small-fixed: the set bits of one 64-bit word")
            while m != 0 {
                let r = w * 64 + m.trailing_zeros() as usize;
                self.late_sel.push(r as u32);
                self.late_sel16.push(r as u16);
                m &= m - 1;
            }
        }
        self.late_cnt[k] += added;
        if self.late_cnt[k] as usize == rows {
            self.late_pend &= !(1 << k);
        }
        let n = self.late_sel.len();
        if n == 0 {
            return Ok(1);
        }
        self.late_decode(k, p, lg, rows, Some(n))?;
        Ok(1)
    }

    /// Whether lane `k`'s fills this window, one more call of `asked` rows reaching row `reach` and `frames` frames
    /// included, are dense: at least `SAMPLE_ROWS` spanned, and the fills so far cost at least the whole decode of the
    /// rows they span by the stream's fitted costs (H09: `LaneDecode::gather_ns` of the calls, the frames each unpacked
    /// and the rows asked, against `whole_ns` of the span, a frame-coded stream's span taken to its frames' end: a
    /// block-by-block fill of a cheap stream pays its calls past a whole decode), or, for a stream the fit did not measure, more than 1 /
    /// `SELECT_DIV` of the span asked (the selective decode's bar). Decided from the fills observed at run time; a span
    /// shorter than the sample decides nothing yet. A selective lane stays late whatever it spans.
    pub(super) fn late_dense(&mut self, k: usize, asked: u32, reach: u32, frames: u32) -> bool {
        self.late_req[k] += asked;
        self.late_span[k] = self.late_span[k].max(reach);
        let calls = &mut self.late_calls[k];
        // a fill whose rows were all decoded already decodes nothing
        (calls.0, calls.1) = (calls.0 + (frames != 0) as u32, calls.1 + frames);
        let (span, (calls, frames)) = (self.late_span[k] as usize, *calls);
        if span < SAMPLE_ROWS {
            return false;
        }
        let kind = if self.lane_byref.has(k) { lane::KIND_BYREF } else { self.recipe.kinds[k] };
        let cur = self.cursors[self.late_g.0 * self.recipe.natts + k].as_deref().expect("cursor");
        match lane_decode(cur.key(), kind, self.recipe.widths[k]) {
            Some(d) => {
                // a frame-coded stream's fills unpack whole frames: against the whole decode of the frames the span reaches
                let whole = if matches!(d, LaneDecode::Ffor | LaneDecode::DeltaFor) { span.next_multiple_of(FRAME_VALUES as usize).min(self.late_g.2) } else { span };
                d.gather_ns(calls as u64, frames as u64, self.late_req[k] as u64) >= d.whole_ns(self.recipe.widths[k], whole as u64)
            }
            None => self.late_req[k] as usize * SELECT_DIV > span,
        }
    }

    /// Lane `k` decoded whole into the staged window (no selection: every row at its position), then done for the
    /// window: no later fill gathers or decodes it again. A by-reference lane's images go to the arena's next bytes (as
    /// a gather's), so the images of rows an earlier fill decoded stay where a consumer may hold them.
    pub(super) fn late_whole(&mut self, k: usize, p: usize, lg: u32, rows: usize) -> PgResult<u32> {
        fires::fire_shared("pgrc.late_whole");
        self.late_decode(k, p, lg, rows, None)?;
        // bounded("small-fixed: the validity words of one granule, at most VALIDITY_WORDS (128)")
        for (w, d) in self.late_done[k * VALIDITY_WORDS..(k + 1) * VALIDITY_WORDS].iter_mut().enumerate() {
            *d = match rows.saturating_sub(w * 64) {
                0 => 0,
                l if l >= 64 => u64::MAX,
                l => (1u64 << l) - 1,
            };
        }
        self.late_cnt[k] = rows as u32;
        self.late_pend &= !(1 << k);
        Ok(1)
    }

    /// Lane `k` of the staged granule: the `n` rows of `late_sel` / `late_sel16` gathered to their positions, or (`None`)
    /// every row; a by-reference lane's images into its arena chunk past the window's earlier images (the next chunk,
    /// twice the last, when that one is full).
    pub(super) fn late_decode(&mut self, k: usize, p: usize, lg: u32, rows: usize, sel: Option<usize>) -> PgResult<()> {
        let natts = self.recipe.natts;
        let scale = self.lane_scale(p, k) as i32;
        let cur = self.cursors[p * natts + k].as_deref_mut().expect("cursor");
        let (want, decoded) = if self.recipe.numeric.has(k) && !self.lane_byref.has(k) {
            if self.recipe.dyn_scale.has(k) {
                fires::fire_shared("pgrc.packed_dyn"); // shapecheck fire signal (N14): a min / max lane reads a chunk's packed codes
            }
            // lane k's slot; the late selection holds `n`
            super::raw::decode_lane_mantissa(cur, scale, lg, rows, sel, &mut self.datums[k * ROWS..], &self.late_sel16)
        } else if self.recipe.kinds[k] == lane::KIND_CODES {
            return Err(err("gravity: a code lane is never late"));
        } else {
            let kind = if self.lane_byref.has(k) { lane::KIND_BYREF } else { self.recipe.kinds[k] };
            let mut small = [0u64; 8];
            // bounded("small-fixed: one retry per doubling of the window's arena chunk, at most 64 (a usize's bits)")
            loop {
                let (c, used) = self.late_at[k];
                let arena: &mut [u8] = match (kind, c) {
                    (lane::KIND_BYREF, 0) => {
                        if self.arenas[k].is_empty() {
                            self.arenas[k].resize(ARENA_FIRST, 0); // alloc: Cold
                        }
                        &mut self.arenas[k][used..]
                    }
                    (lane::KIND_BYREF, c) => &mut self.late_more[k][c - 1][used..],
                    _ => crate::ops::scan::pgrc::raw::words_as_bytes_mut(&mut small),
                };
                let mut took = 0;
                // as `decode_lane`'s; the late selection holds `n` rows
                let (want, r) = super::raw::decode_lane_into(cur, kind, self.recipe.widths[k], lg, rows, sel, &mut self.datums[k * ROWS..], &mut self.dense, &self.late_sel16, &self.late_sel, arena, &mut took);
                match r {
                    Err(pgrc_read::ReadError::Format(pgrc_format::FormatError::ArenaExhausted { .. })) if kind == lane::KIND_BYREF => {
                        // the next chunk (twice the last): the window's earlier images stay where they are
                        let last = if c == 0 { self.arenas[k].len() } else { self.late_more[k][c - 1].len() };
                        // the data sizes the chunk (D2582), reserved fallibly: a machine that cannot hold it is the typed error
                        let size = 2 * last;
                        if self.late_more[k].len() <= c {
                            self.late_more[k].push(Vec::new()); // alloc: Cold
                        }
                        if !super::grow_arena(&mut self.late_more[k][c], size) {
                            break (want, r);
                        }
                        self.late_at[k] = (c + 1, 0);
                    }
                    r => {
                        // the next fill's arena starts 8-aligned (ByteArena's contract)
                        self.late_at[k].1 = (used + took).next_multiple_of(8);
                        break (want, r);
                    }
                }
            }
        };
        if decoded.map_err(read_err)? as usize != want {
            return Err(err("gravity: pgrc granule decoded count"));
        }
        Ok(())
    }

    /// The value of a verbatim part's code: its representative row `(lg, i)` decoded alone
    /// (`decode_sel`) into the render's chunks, an image that stays for the statement (the leader's
    /// fold holds the datums).
    /// D4548: each new chunk is charged to the source's held owner as it is pushed (`push_render`).
    pub(super) fn render_row(&mut self, p: usize, k: usize, lg: u32, i: u32) -> PgResult<(Word, u32)> {
        let cur = self.cursors[p * self.recipe.natts + k].as_deref_mut().expect("cursor");
        if i >= cur.rows_in_granule(lg) {
            return Err(err("gravity: a row locator outside its granule"));
        }
        let i = i as usize;
        let sel = [i as u16];
        let mut one = [0u64; 1];
        // bounded("small-fixed: at most three passes: a fresh chunk, then one sized past the image")
        loop {
            if self.render.is_empty() {
                push_render(&mut self.render, &mut self.held, RENDER_CHUNK_MIN);
                self.render_used = 0;
            }
            let chunk = self.render.last_mut().expect("chunk");
            let len = chunk.len() * 8;
            // the images below `render_used` are never rewritten
            let bytes = crate::ops::scan::pgrc::raw::words_as_bytes_mut(chunk);
            let mut arena = ByteArena::new(bytes);
            if arena.alloc(self.render_used).is_err() {
                let want = len / 4;
                push_render(&mut self.render, &mut self.held, want);
                self.render_used = 0;
                continue;
            }
            let mut out = DecodeOut { datums: &mut one, arena };
            match cur.decode_sel(lg, &pgrc_format::abi::Selection { rows: &sel }, &mut out) {
                Ok(1) => {
                    self.render_used = out.arena.used();
                    // the decoded datum is a 4B-U image in the chunk
                    let hdr = super::raw::image_header(one[0]);
                    return Ok((one[0], (hdr >> 2) - 4));
                }
                Ok(_) => return Err(err("gravity: render decoded count")),
                Err(pgrc_read::ReadError::Format(pgrc_format::FormatError::ArenaExhausted { needed })) => {
                    push_render(&mut self.render, &mut self.held, (needed * 2).max(len * 2).div_ceil(8));
                    self.render_used = 0;
                }
                Err(e) => return Err(read_err(e)),
            }
        }
    }
}

/// D4548: a new render chunk of `words` words, charged to `held` (the source's statement / breaker owner) as it is made. A
/// render runs from `dict_value`, after the window's sync: the consumer's fold renders its group keys there, so an uncharged
/// chunk sat on the thread's account, unattributed, until the unbind (a parallel GROUP BY over wide dictionary-coded text
/// keys rendered late: 255 MiB of chunks on the workers). `held_bytes` counts the chunks, so the bind's truncate (`bind_pg`) gives the freed ones back at
/// the bind's sync, as every later sync does (D4536's `TableState::push_chunk` pattern).
fn push_render(render: &mut Vec<Vec<u64>>, held: &mut HeldCharge, words: usize) {
    render.push(vec![0u64; words]); // alloc: Exec charged held
    held.grow(words as u64 * 8);
}

#[cfg(test)]
mod tests {
    /// D4548: a render chunk is the held owner's from its push (a render runs after the window's sync, so no sync would
    /// charge it before the unbind); the next sync over what the chunks hold keeps it there, and the bind's truncate gives
    /// the freed chunks back at its sync.
    #[test]
    fn a_render_chunk_is_its_held_owners_from_its_push() {
        use memledger::{charge, Kind};
        std::thread::spawn(|| {
            // a backend slot (procno < ACCOUNT_SLOTS - BACKEND_SLOT_BASE; 3990-3996 are the siblings'), not UNBOUND
            memledger::bind_backend(3997, 0);
            memledger::statement_begin();
            let (stmt, _) = charge::context();
            let c = |o| charge::owner_row(o).committed;
            let mut h = super::HeldCharge::new();
            h.home(Some((stmt, Kind::Region)));
            let mut render: Vec<Vec<u64>> = Vec::new(); // alloc: Cold
            super::push_render(&mut render, &mut h, 1 << 17);
            assert_eq!(c(stmt), 1 << 20, "the first chunk: the owner's at its push");
            super::push_render(&mut render, &mut h, 1 << 18);
            assert_eq!(c(stmt), 3 << 20, "a chunk past the first: the owner's at its push");
            let held = |r: &Vec<Vec<u64>>| r.iter().map(|v| v.capacity() * 8).sum::<usize>() as u64;
            h.sync(held(&render));
            assert_eq!(c(stmt), 3 << 20, "the next sync finds the chunks charged");
            render.truncate(1);
            h.sync(held(&render));
            assert_eq!(c(stmt), 1 << 20, "the bind's truncate: the freed chunk given back at its sync");
            drop(h);
            assert_eq!(c(stmt), 0, "dropped with the source");
            memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .expect("the test thread");
    }
}
