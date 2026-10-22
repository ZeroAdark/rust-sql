//! D3312: a phase-1 table's key images reclaimed at an in-memory compact. Budget 150 lines.
//!
//! A table that evicts a text group and opens it again copies the key's image again (lowerv4 table/raw.rs `key_copy`,
//! at the record's open), and the compact that folds the two records keeps one copy: the other stays in the arena,
//! whose chunks are pinned to the statement's end. An input that cycles a few thousand text keys through a table kept
//! in memory grew 8 MiB past a 1 MB work_mem that way, with the groups' images ~320 KiB. PostgreSQL's HashAggregate
//! keeps each group's key once (its tuple in the hash table's memory context). Here, after a compact that left the
//! table empty and every record in its pool (`folded_in_memory`), the live images slide down the arena's chunk chain in
//! chain order (abi `img_bump`'s format: a chunk's start map gives each image's extent), the records' copy slots follow
//! them, and the bump then reuses the kept chunks before it pins another: the arena stays within a few times the live
//! images plus one fill's copies.
//!
//! Only where nothing outside the table and its pool holds an image (no spill or destage yet: a written record names
//! its images until the spill inlines them; no text min / max or numeric side copy, whose copies a code front's
//! records hold while a flush runs) and the lowerer named the copy slots (`T1::img_slots`).
use gravity_abi::breaker::{img_chunk, img_data_off, img_extent, img_fill, img_mark, img_open_chunk, img_set_fill, img_unmark_all, SlotKind, T1};
use gravity_abi::breaker::{IMG_SLOTS_KNOWN, IMG_SLOTS_MAX, IMG_SLOT_PLAIN, IMG_SLOT_TAGGED, IMG_TAG_COPY, MINCODE_NONE, T1_IMAGE_CHUNK};
use gravity_fn::arena::{AVec, OpArena, Refused};

/// The arena bytes past twice the last reclaim's kept bytes that start the next reclaim: each reclaim moves at most the
/// bytes bumped since the one before, so the moves cost O(1) a copied byte; a small arena is never walked.
pub(crate) const RECLAIM_MIN: u64 = 2 * T1_IMAGE_CHUNK as u64;

/// A chain position's bits: the chunk's rank above, the byte within it below.
const OFF_BITS: u32 = 40;

/// The bytes of `reclaim`'s largest list for table `t` now (0 when it cannot run: no known copy slots): a reference per
/// copy slot of each buffered record (its chain and slot lists are a few words a `T1_IMAGE_CHUNK` chunk and a slot).
/// # Safety
/// `t` is a live table.
pub(crate) unsafe fn scratch_bound(t: *mut T1) -> usize {
    let (mask, layout) = (*T1::img_slots(t), &(*t).layout);
    if mask & IMG_SLOTS_KNOWN == 0 {
        return 0;
    }
    let copies = (0..layout.nslots().min(IMG_SLOTS_MAX)).filter(|&j| matches!(mask >> (2 * j) & 3, IMG_SLOT_PLAIN | IMG_SLOT_TAGGED)).count();
    16 * (*t).buffered as usize * copies
}

/// The live images of table `t` slid down its arena when the arena grew past `RECLAIM_MIN` and twice what the last
/// reclaim kept; whether it ran. Nothing moves when any check fails (`img_reclaim`'s doc above). The walk's lists are
/// carved from `a`, the caller's compact scratch on the table's slot owner; `Err` when it refuses a carve (nothing moved).
/// # Safety
/// `t` is a live table whose records are all in its pool (its table drained, a compact done), with no appender open.
pub(crate) unsafe fn reclaim(t: *mut T1, a: &OpArena) -> Result<bool, Refused> {
    let (mask, w) = (*T1::img_slots(t), T1::img_words(t));
    let open = img_open_chunk(w);
    if mask & IMG_SLOTS_KNOWN == 0 || (*t).flushes != 0 || (*t).len != 0 || open == 0 {
        return Ok(false);
    }
    let layout = &(*t).layout;
    if layout.slot_kinds().iter().any(|k| matches!(k, SlotKind::MinVar | SlotKind::MaxVar | SlotKind::NumSideMin | SlotKind::NumSideMax | SlotKind::AggState)) {
        return Ok(false);
    }
    // the chain in bump order: (base, bytes, fill); the open chunk's fill is its cursor
    let (mut chain, mut c, mut used) = (AVec::<(usize, usize, usize)>::new(), img_chunk(open).2, 0u64);
    while c != 0 {
        let (next, size, _) = img_chunk(c);
        let fill = if c == open { *w as usize } else { img_fill(c) };
        used += (fill - c - img_data_off(size)) as u64;
        chain.push(a, (c, size, fill))?;
        c = next;
    }
    let kept = T1::img_kept(t);
    if used < 2 * *kept + RECLAIM_MIN {
        return Ok(false);
    }
    let chunks = chain.as_slice(a);
    let mut slots = AVec::<(usize, bool)>::with_capacity(a, layout.nslots().min(IMG_SLOTS_MAX))?;
    for j in 0..layout.nslots().min(IMG_SLOTS_MAX) {
        match mask >> (2 * j) & 3 {
            IMG_SLOT_PLAIN => slots.push(a, (layout.slot_off(j) as usize, false))?,
            IMG_SLOT_TAGGED => slots.push(a, (layout.slot_off(j) as usize, true))?,
            _ => {}
        }
    }
    let slots = slots.as_slice(a);
    let mut by_base = AVec::<(usize, usize)>::with_capacity(a, chunks.len())?;
    for (i, c) in chunks.iter().enumerate() {
        by_base.push(a, (c.0, i))?;
    }
    let by_base = by_base.as_mut_slice(a);
    by_base.sort_unstable();
    // every copy slot's image: its chain position and the slot
    let rec = layout.size() as usize;
    let mut refs = AVec::<(u64, *mut u64)>::with_capacity(a, (*t).buffered as usize * slots.len())?;
    for part in 0..(*t).parts() as u32 {
        for b in (*t).chain(part) {
            for i in 0..b.recs {
                let r = b.ptr.add(i * rec) as *mut u8;
                for &(off, tagged) in slots {
                    let p = r.add(off) as *mut u64;
                    let v = p.read_unaligned();
                    let at = match tagged {
                        true if v >= IMG_TAG_COPY && v != MINCODE_NONE => (v & !IMG_TAG_COPY) as usize,
                        true => continue,
                        false => v as usize,
                    };
                    let k = by_base.partition_point(|&(base, _)| base <= at);
                    if at == 0 || k == 0 || at >= chunks[by_base[k - 1].1].0 + chunks[by_base[k - 1].1].1 {
                        continue; // none, or another arena's image (a merged region's): not this chain's to move
                    }
                    let (base, size, fill) = chunks[by_base[k - 1].1];
                    if at < base + img_data_off(size) || at >= fill {
                        return Ok(false); // a word into the chain that names no image: nothing moved
                    }
                    let Some((start, _)) = img_extent(base, at, fill) else { return Ok(false) };
                    refs.push(a, ((by_base[k - 1].1 as u64) << OFF_BITS | (start - base) as u64, p))?;
                }
            }
        }
    }
    let refs = refs.as_mut_slice(a);
    refs.sort_unstable_by_key(|r| r.0);
    // slide: an image never moves past its own place (the bump packed the same chain in the same order)
    let (mut di, mut d, mut last, mut moved) = (0usize, chunks[0].0 + img_data_off(chunks[0].1), u64::MAX, (0usize, 0usize));
    for r in refs.iter_mut() {
        if r.0 != last {
            last = r.0;
            let (ord, base) = ((r.0 >> OFF_BITS) as usize, chunks[(r.0 >> OFF_BITS) as usize].0);
            let start = base + (r.0 & ((1 << OFF_BITS) - 1)) as usize;
            let (_, end) = img_extent(base, start, chunks[ord].2).expect("an image's start is marked");
            while d + (end - start) > chunks[di].0 + chunks[di].1 {
                img_set_fill(chunks[di].0, d);
                di += 1;
                d = chunks[di].0 + img_data_off(chunks[di].1);
            }
            debug_assert!(di < ord || di == ord && d <= start, "D3312: an image slid past its place");
            core::ptr::copy(start as *const u8, d as *mut u8, end - start);
            moved = (start, d);
            d += end - start;
        }
        let v = r.1.read_unaligned();
        r.1.write_unaligned((moved.1 as u64).wrapping_add(v.wrapping_sub(moved.0 as u64)));
        r.0 = (di as u64) << OFF_BITS | (moved.1 - chunks[di].0) as u64;
    }
    // the start maps again: only the moved images'; the chunks past the cursor's emptied
    for c in chunks {
        img_unmark_all(c.0);
    }
    for r in refs.iter() {
        let base = chunks[(r.0 >> OFF_BITS) as usize].0;
        img_mark(base, base + (r.0 & ((1 << OFF_BITS) - 1)) as usize);
    }
    for c in &chunks[di + 1..] {
        img_set_fill(c.0, c.0 + img_data_off(c.1));
    }
    img_set_fill(chunks[di].0, d);
    let (base, size, _) = chunks[di];
    (*w, *w.add(1), *w.add(2)) = (d as u64, (base + size) as u64, size as u64);
    *kept = chunks[..di].iter().map(|c| (img_fill(c.0) - c.0 - img_data_off(c.1)) as u64).sum::<u64>() + (d - base - img_data_off(size)) as u64;
    T1::dedupe_clear(t);
    Ok(true)
}
