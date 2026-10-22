//! drain's raw home (rule R6: a family's unsafe code sits in its raw.rs): the drain parts' host functions of a spilled
//! RIGHT / FULL / RIGHT SEMI / RIGHT ANTI build, moved as they were from drain/spill.rs (which keeps the part numbering
//! and re-exports them for spill/load.rs and spill/raw.rs), and the drain source's phase 2 host entry `drain_part`,
//! moved from drain/part.rs (which declares it to the IR and keeps its phase word).
#![allow(unsafe_code)]
use gravity_abi::ctx::Ctx;
use gravity_driver::mem::spill_host::v4;
use gravity_driver::Error;

use gravity_program::cost::JOIN_SPILL_EPOCH;

use crate::ops::hashjoin::build::{ALLOW, A_MARKS, END};
use crate::ops::hashjoin::rt::{rd, settle, Rt};
use crate::ops::hashjoin::spill::build::{SPILL, SPILLED};
use crate::ops::hashjoin::spill::load::{arg_bits, owners, part_region, records, Buf};
use crate::ops::hashjoin::spill::pair::{DRAIN_MARKED, FIRST, META, M_PART};
use crate::ops::hashjoin::spill::raw::{file_len, serialize_each, serialize_over};
use crate::ops::hashjoin::drain::part::{claim_parts, phase, resume, CLAIMED, PH_END};
use crate::ops::hashjoin::drain::spill::{drain_head, u_part, MAGIC_U};

/// The drain part of the partition at pair region `m` written and flushed: `drain_pieces` put to the stage as they come
/// (no copy of the part; the stage writes it as epochs of its own size, which a read joins back). A partition no kept row
/// reached is not written: the drain reads its build part whole. 0, or `FAILED` held.
pub(crate) unsafe fn write_drain(ctx: *mut Ctx, stream: u64, arg: u64, m: *mut u8) -> i64 {
    let d = m.add(META);
    if rd::<u8>(d, ALLOW) & A_MARKS == 0 {
        return v4::fail(ctx, Error::host("gravity: a drained spill partition without marks"));
    }
    let (bits, part, marked) = (arg_bits(arg), u_part(arg_bits(arg), rd::<u64>(m, M_PART) as u32), arg & DRAIN_MARKED != 0);
    // a failed put holds its own error
    let mut put = |b: &[u8]| -> Result<(), ()> { if v4::put(ctx, stream, bits, part, b) == v4::FAILED { Err(()) } else { Ok(()) } };
    match drain_pieces(d, marked, &mut put) {
        Err(()) => v4::FAILED,
        Ok(()) => v4::flush(ctx, stream, bits),
    }
}

/// The drain part of laid-out region `d` (its header, then its records whose mark is `marked`, as a file holds them)
/// handed to `put` in order through a stack `Batch`: each record serialized straight into the batch (`serialize_over`),
/// one past `BATCH` in pieces read in place (`serialize_each`).
pub(crate) unsafe fn drain_pieces<E>(d: *mut u8, marked: bool, put: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
    let mut batch = Batch { buf: [0; BATCH], len: 0 };
    batch.write(&drain_head(drain_n(d, marked)), put)?;
    drain_each(d, marked, &mut |t, i| batch.record(t, i, put))?;
    batch.flush(put)
}

// `write_drain`'s gather of a drain part's pieces on the stack before each `v4::put`
use gravity_program::cost::DRAIN_PUT_BATCH as BATCH;

struct Batch {
    buf: [u8; BATCH],
    len: usize,
}

impl Batch {
    /// `b` after the bytes held: held while they fit, else the held bytes put first; a piece past `BATCH` is put as it
    /// lies, in `JOIN_SPILL_EPOCH` slices.
    fn write<E>(&mut self, b: &[u8], put: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        if self.len + b.len() > BATCH {
            self.flush(put)?;
        }
        if b.len() <= BATCH {
            self.buf[self.len..self.len + b.len()].copy_from_slice(b);
            self.len += b.len();
            return Ok(());
        }
        b.chunks(JOIN_SPILL_EPOCH).try_for_each(put)
    }

    fn flush<E>(&mut self, put: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        if self.len != 0 {
            put(&self.buf[..self.len])?;
            self.len = 0;
        }
        Ok(())
    }

    /// Record `i` of `t` (its image words by distance) after the bytes held: serialized over the batch's free bytes
    /// (the held bytes put first when it does not fit them); one past `BATCH` goes in its `serialize_each` pieces.
    unsafe fn record<E>(&mut self, t: &Rt, i: usize, put: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
        let len = file_len(t, i);
        if len > BATCH {
            return serialize_each(t, i, &mut |b| self.write(b, put));
        }
        if self.len + len > BATCH {
            self.flush(put)?;
        }
        serialize_over(t, i, &mut self.buf[self.len..self.len + len]);
        self.len += len;
        Ok(())
    }
}

/// Whether record `i` of laid-out region `t` is sent by a drain of `marked` (its mark is `marked`).
unsafe fn sent(t: &Rt, i: usize, marked: bool) -> bool {
    (*t.d.add(t.marks_at() + i) != 0) == marked
}

/// The records (keyed and NULL-key) of laid-out region `d` whose mark is `marked`.
pub(crate) unsafe fn drain_n(d: *mut u8, marked: bool) -> u32 {
    let t = Rt::of(d, rd::<u64>(d, END) as usize);
    (0..t.n + t.kept).filter(|&i| sent(&t, i, marked)).count() as u32
}

/// The bytes the records `drain_each` visits in region `d` take in a file (`file_len`).
pub(crate) unsafe fn drain_bytes(d: *mut u8, marked: bool) -> usize {
    let mut bytes = 0;
    let _ = drain_each(d, marked, &mut |t, i| -> Result<(), core::convert::Infallible> {
        bytes += file_len(t, i);
        Ok(())
    });
    bytes
}

/// `f` over every record (keyed and NULL-key) of laid-out region `d` whose mark is `marked`, in order, to serialize it
/// (`file_len`, `serialize_over`, `serialize_each`). A laid-out record's image words are offsets from the base (`settle`)
/// and `image` reads them by distance, so each is turned back for its call and settled again after (M7-DRAIN's e2e: a
/// drained text read another record's bytes).
pub(crate) unsafe fn drain_each<E>(d: *mut u8, marked: bool, f: &mut dyn FnMut(&Rt, usize) -> Result<(), E>) -> Result<(), E> {
    let end = rd::<u64>(d, END) as usize;
    let t = Rt::of(d, end);
    for i in (0..t.n + t.kept).filter(|&i| sent(&t, i, marked)) {
        settle(&t, i..i + 1, end);
        let r = f(&t, i);
        settle(&t, i..i + 1, end);
        r?;
    }
    Ok(())
}

/// The drain part of laid-out region `d` in one buffer (`drain_pieces` joined; the tests').
#[cfg(test)]
pub(crate) unsafe fn drain_set(d: *mut u8, marked: bool) -> Vec<u8> {
    let mut out = Vec::new();
    drain_pieces(d, marked, &mut |b| -> Result<(), ()> {
        out.extend_from_slice(b);
        Ok(())
    })
    .unwrap();
    out
}

/// A drain part's records, from each owner's bytes of it (`owners`): `None` when no owner wrote it (no claim opened the
/// partition). One owner writes a partition's drain part (a claim takes whole partitions); a short, headless, truncated
/// or miscounted part is an error by name, never a partial region.
pub(crate) unsafe fn drain_records<'a>(orig: *const u8, owners: &[&'a [u8]]) -> gravity_driver::Result<Option<&'a [u8]>> {
    let mut wrote = owners.iter().filter(|o| !o.is_empty());
    let Some(bytes) = wrote.next() else { return Ok(None) };
    if wrote.next().is_some() {
        return Err(Error::host("gravity: a spill drain part written by two claims"));
    }
    if bytes.len() < 8 || bytes[..4] != MAGIC_U {
        return Err(Error::host("gravity: a spill drain part has no header"));
    }
    let (n, body) = (u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize, &bytes[8..]);
    if records(orig, body)?.0 != n {
        return Err(Error::host("gravity: a spill drain part's record count does not match its header"));
    }
    Ok(Some(body))
}

/// Part `part` of `stream` (the build slot's) laid out in a region of its own headed as the build region `orig` (the drain,
/// M7-DRAIN's phase 2): (the region, its length, its records, the byte its next chunk starts at or 0), walked by record
/// over `[0, n)`. D2715: a part past the region's length loads in chunks from byte `from` (spill/raw.rs `chunk_of`). A
/// build part is always `Some` (no bytes: no record). A drain part (`u_part`) is `None` when no claim opened its partition
/// (`drain_records`). The region stays until `give_part` or the statement's end.
pub(crate) unsafe fn load_part(ctx: *mut Ctx, stream: u64, part: u32, orig: *const u8, from: usize) -> gravity_driver::Result<Option<(*mut u8, usize, usize, usize)>> {
    let mut raw = Buf::new(ctx, FIRST)?;
    let r = (|| {
        // the first two owners that wrote bytes of the part: `drain_records` reads one, and refuses a second by name
        let (mut wrote, mut got) = ([(0usize, 0usize); 2], 0);
        for o in 0..owners(ctx, stream)? {
            let from = raw.len;
            raw.read_owner(stream, o, part)?;
            if raw.len > from && got < 2 {
                (wrote[got], got) = ((from, raw.len), got + 1);
            }
        }
        let bytes = core::slice::from_raw_parts(raw.p, raw.len);
        let body = match part >= u_part(rd::<u8>(orig, SPILL), 0) {
            true => {
                let [(a0, z0), (a1, z1)] = wrote;
                match drain_records(orig, &[&bytes[a0..z0], &bytes[a1..z1]])? {
                    Some(b) => b,
                    None => return Ok(None),
                }
            }
            false => bytes,
        };
        let to = crate::ops::hashjoin::spill::raw::chunk_of(orig, body, from, crate::ops::hashjoin::spill::raw::part_cap())?;
        let (_, d, len, n) = part_region(ctx, 0, orig, &body[from.min(to)..to])?;
        Ok(Some((d, len, n, if to < body.len() { to } else { 0 })))
    })();
    v4::give(ctx, raw.p);
    r
}

/// A region `load_part` took, given back.
pub(crate) unsafe fn give_part(ctx: *mut Ctx, region: *mut u8) {
    v4::give(ctx, region);
}

gravity_abi::host! {
    /// `next`'s host: the region `reg` of the chunk just sent given back; the partition's next chunk when the phase names one
    /// (D2715), else the next spilled partition `p >= ph` of the resident build `orig` with a record to send laid out (its
    /// drain part, the records P' chose; when no claim opened `p`, its build part whole, none for RIGHT SEMI): `p + 1` (with
    /// the next chunk's byte above `PH_PART` bits) with the region and its count at `out`; `PH_END` past the last; -1 with the
    /// error held. `how`: bit 0 RIGHT SEMI's; D3024 (b), with `CLAIMED` only the partitions of a wide drain's claim
    /// (`claim_parts`). The drain runs after P' (`Follows`), so every part is written and flushed before any call; each
    /// call (the leader's, or a wide drain's claim thread's) holds one region of its own at a time, its partitions no other
    /// claim's (spill_load.rs `load_part` / `give_part`).
    pub(crate) unsafe fn drain_part(ctx: u64, stream: u64, orig: u64, ph: u64, reg: u64, how: u64, out: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let ctx = ctx as *mut Ctx;
        let orig = orig as *const u8;
        let out = out as *mut u64;
        if reg != 0 {
            give_part(ctx, reg as *mut u8);
        }
        let (bits, mask) = (rd::<u8>(orig, SPILL), rd::<u64>(orig, SPILLED));
        let (first, at) = resume(ph);
        let semi = how & 1;
        let (lo, hi) = match how & CLAIMED {
            0 => (0, 1u32 << bits),
            _ => claim_parts(how >> 16 & 0xffff, how >> 32, bits as u32),
        };
        for p in (first.max(lo)..hi).filter(|p| mask >> p & 1 != 0) {
            let from = if p == first { at } else { 0 };
            let got = match load_part(ctx, stream, u_part(bits, p), orig, from) {
                Ok(None) if semi == 0 => load_part(ctx, stream, p, orig, from),
                r => r,
            };
            match got {
                Err(e) => return v4::fail(ctx, e),
                Ok(Some((r, _, n, next))) if n > 0 => {
                    (out.write(r as u64), out.add(1).write(n as u64));
                    return phase(p, next);
                }
                Ok(Some((r, _, _, _))) => give_part(ctx, r),
                Ok(None) => {}
            }
        }
        PH_END
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D4431 / CONVERT-WAVE1: `write_drain`'s batch hands every byte on in order, each put at most `BATCH` bytes, but a
    /// piece past `BATCH`, which goes in `JOIN_SPILL_EPOCH` slices after the bytes held before it.
    #[test]
    fn a_batch_puts_every_byte_in_order_in_bounded_pieces() {
        let sizes = [8usize, 0, 100, BATCH - 108, 1, BATCH, 5, BATCH + 1, 3 * JOIN_SPILL_EPOCH + 17, 40, 2 * BATCH, 9];
        let mut want = Vec::new();
        let (mut puts, mut got): (Vec<usize>, Vec<u8>) = (Vec::new(), Vec::new());
        let mut put = |b: &[u8]| -> Result<(), ()> {
            puts.push(b.len());
            got.extend_from_slice(b);
            Ok(())
        };
        let mut batch = Batch { buf: [0; BATCH], len: 0 };
        for (k, &n) in sizes.iter().enumerate() {
            let piece: Vec<u8> = (0..n).map(|i| (i * 31 + k * 7) as u8).collect();
            batch.write(&piece, &mut put).unwrap();
            want.extend_from_slice(&piece);
        }
        batch.flush(&mut put).unwrap();
        assert_eq!(got, want);
        let e = JOIN_SPILL_EPOCH;
        assert_eq!(puts, [BATCH, 1, BATCH, 5, BATCH + 1, e, e, e, 17, 40, 2 * BATCH, 9]);
        // a failed put stops the batch
        let mut n = 0;
        let mut failing = |_: &[u8]| -> Result<(), u8> {
            n += 1;
            if n == 2 { Err(7) } else { Ok(()) }
        };
        let mut batch = Batch { buf: [0; BATCH], len: 0 };
        let r = (|| {
            for _ in 0..4 {
                batch.write(&[1u8; BATCH], &mut failing)?;
            }
            batch.flush(&mut failing)
        })();
        assert_eq!(r, Err(7));
    }
}
