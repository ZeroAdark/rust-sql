//! Line-boundary segmentation for parallel text / CSV COPY FROM (text_pgrc.rs). Cuts follow
//! `copy_read_line_text`'s rules byte for byte (quote / escape toggles, the quoted line-break count
//! keyed on the decided style, text backslashes, the end-of-copy marker, mixed-style terminators are
//! no cut), so a helper's serial reader over a chunk, preset to the chunk's `cur_lineno` and style,
//! yields the serial rows and errors. Every chunk but the last holds whole engine grains (#560).

use std::sync::Arc;

use types_error::{PgError, ERRCODE_PROGRAM_LIMIT_EXCEEDED};

use crate::fromparse::EolType;

/// CONVERT W3-2b: one read block of the input, charged to the loading statement (`memledger::ChargedVec`, filled by the
/// leader, freed by the helper that drops the last segment of it); uncharged with no owner (the tests).
pub(crate) type SegBlock = memledger::ChargedVec<u8>;

/// An uncharged block holding `b` (the tests' feed).
#[cfg(test)]
pub(crate) fn seg_block(b: &[u8]) -> SegBlock {
    let mut v = SegBlock::new(None, memledger::Kind::Region);
    v.extend_from_slice(b).expect("a test block");
    v
}

#[derive(Clone)]
pub(crate) struct ChunkSeg {
    pub(crate) buf: Arc<SegBlock>,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// `CopySrc::Chunk`: after the segments, the source's own read error once, where the serial reader meets it.
pub(crate) struct ChunkCursor {
    segs: Vec<ChunkSeg>,
    seg: usize,
    off: usize,
    tail_err: Option<Box<PgError>>,
}

impl ChunkCursor {
    pub(crate) fn new(segs: Vec<ChunkSeg>, tail_err: Option<Box<PgError>>) -> ChunkCursor {
        let off = segs.first().map(|s| s.start).unwrap_or(0);
        ChunkCursor { segs, seg: 0, off, tail_err }
    }

    pub(crate) fn read(&mut self, dst: &mut [u8]) -> Result<usize, Box<PgError>> {
        let mut filled = 0usize;
        while filled < dst.len() && self.seg < self.segs.len() {
            let s = &self.segs[self.seg];
            let avail = s.end - self.off;
            if avail == 0 {
                self.seg += 1;
                self.off = self.segs.get(self.seg).map(|s| s.start).unwrap_or(0);
                continue;
            }
            let n = avail.min(dst.len() - filled);
            dst[filled..filled + n].copy_from_slice(&s.buf[self.off..self.off + n]);
            self.off += n;
            filled += n;
        }
        if filled == 0 {
            if let Some(e) = self.tail_err.take() {
                return Err(e);
            }
        }
        Ok(filled)
    }
}

pub(crate) struct ChunkDesc {
    /// The serial `cur_lineno` before the chunk's first line (0: chunk 0 holds the header line).
    pub(crate) lineno_before: u64,
    pub(crate) eol_at_start: EolType,
    /// Header and end-of-copy marker lines are not rows.
    pub(crate) rows: u64,
    pub(crate) segs: Vec<ChunkSeg>,
    pub(crate) last: bool,
    pub(crate) tail_err: Option<Box<PgError>>,
}

#[derive(Clone, Copy)]
pub(crate) struct SegOpts {
    pub(crate) csv: bool,
    pub(crate) quote: u8,
    pub(crate) escape: u8,
    pub(crate) header: bool,
    pub(crate) grain: u64,
    pub(crate) grains: u64,
    pub(crate) target_bytes: u64,
    pub(crate) max_line_bytes: u64,
}

const CLASS_PLAIN: u8 = 0;

pub(crate) struct Segmentator {
    o: SegOpts,
    /// 0 = no escape processing (ESCAPE equal to QUOTE), as the serial reader.
    escapec: u8,
    class: [u8; 256],
    eol: EolType,
    lineno: u64,
    cur: u64,
    header_pending: bool,
    in_line: bool,
    in_quote: bool,
    last_was_esc: bool,
    /// An unquoted `\r` whose lookahead byte is the next block's first.
    pending_cr: bool,
    pending_bs: bool,
    pending_bs_at_start: bool,
    line_start_at: u64,
    segs: Vec<ChunkSeg>,
    seg_bytes: u64,
    chunk_rows: u64,
    chunk_lineno_before: u64,
    chunk_eol: EolType,
    seg_start: usize,
    pub(crate) bytes_total: u64,
    pub(crate) rows_total: u64,
    pub(crate) eoc: bool,
    pub(crate) eoc_lineno: u64,
    /// Bytes after the marker's `.` owed to the final chunk (the serial checks read up to two).
    eoc_need: usize,
}

impl Segmentator {
    pub(crate) fn new(o: SegOpts) -> Segmentator {
        let escapec = if o.csv && o.escape != o.quote { o.escape } else { 0 };
        let mut class = [CLASS_PLAIN; 256];
        class[b'\r' as usize] = 1;
        class[b'\n' as usize] = 1;
        if o.csv {
            class[o.quote as usize] = 1;
            if escapec != 0 {
                class[escapec as usize] = 1;
            }
        } else {
            class[b'\\' as usize] = 1;
        }
        Segmentator {
            o,
            escapec,
            class,
            eol: EolType::Unknown,
            lineno: 0,
            cur: 1,
            header_pending: o.header,
            in_line: false,
            in_quote: false,
            last_was_esc: false,
            pending_cr: false,
            pending_bs: false,
            pending_bs_at_start: false,
            line_start_at: 0,
            segs: Vec::new(),
            seg_bytes: 0,
            chunk_rows: 0,
            chunk_lineno_before: 0,
            chunk_eol: EolType::Unknown,
            seg_start: 0,
            bytes_total: 0,
            rows_total: 0,
            eoc: false,
            eoc_lineno: 0,
            eoc_need: 0,
        }
    }

    pub(crate) fn input_complete(&self) -> bool {
        self.eoc && self.eoc_need == 0
    }

    pub(crate) fn feed(&mut self, buf: &Arc<SegBlock>, out: &mut Vec<ChunkDesc>) -> Result<(), Box<PgError>> {
        let n = buf.len();
        self.seg_start = 0;
        if n == 0 || self.input_complete() {
            return Ok(());
        }
        let end = if self.eoc {
            let k = self.eoc_need.min(n);
            self.eoc_need -= k;
            k
        } else {
            let mut i = 0usize;
            if self.pending_cr {
                self.pending_cr = false;
                i = self.resolve_cr(buf[0], buf, 0, out);
            } else if self.pending_bs {
                self.pending_bs = false;
                i = self.resolve_bs(buf, 0, self.pending_bs_at_start);
            }
            if self.eoc {
                i
            } else if self.o.csv {
                self.scan_csv(buf, i, out);
                n
            } else {
                self.scan_text(buf, i, out)
            }
        };
        if self.seg_start < end {
            self.segs.push(ChunkSeg { buf: Arc::clone(buf), start: self.seg_start, end });
            self.seg_bytes += (end - self.seg_start) as u64;
        }
        self.seg_start = end;
        self.bytes_total += n as u64;
        if self.in_line && !self.eoc && self.bytes_total - self.line_start_at > self.o.max_line_bytes {
            return Err(Box::new(
                PgError::error(format!(
                    "parallel COPY: a line exceeds {} bytes",
                    self.o.max_line_bytes
                ))
                .with_sqlstate(ERRCODE_PROGRAM_LIMIT_EXCEEDED)
                .with_hint(
                    "Set PGRUST_PARALLEL_TEXT_COPY=0 to load this input with the serial reader.",
                ),
            ));
        }
        Ok(())
    }

    /// EOF reads as the serial reader's NUL pad; the final chunk is always cut, possibly empty.
    pub(crate) fn finish(&mut self, out: &mut Vec<ChunkDesc>) {
        if self.pending_cr {
            self.pending_cr = false;
            if self.eol == EolType::Unknown {
                self.eol = EolType::Cr;
            }
            if self.eol != EolType::Crnl {
                self.end_line_at_eof();
            }
        }
        self.pending_bs = false;
        if self.in_line && !self.eoc {
            self.count_line();
        }
        let segs = std::mem::take(&mut self.segs);
        out.push(ChunkDesc {
            lineno_before: self.chunk_lineno_before,
            eol_at_start: self.chunk_eol,
            rows: self.chunk_rows,
            segs,
            last: true,
            tail_err: None,
        });
        self.seg_bytes = 0;
        self.chunk_rows = 0;
    }

    fn end_line_at_eof(&mut self) {
        self.count_line();
        self.in_line = false;
        self.in_quote = false;
        self.last_was_esc = false;
    }

    fn count_line(&mut self) {
        self.lineno = self.cur;
        self.cur = self.lineno + 1;
        if self.header_pending {
            self.header_pending = false;
        } else {
            self.chunk_rows += 1;
            self.rows_total += 1;
        }
    }

    fn end_line(&mut self, buf: &Arc<SegBlock>, pos: usize, out: &mut Vec<ChunkDesc>) {
        self.count_line();
        self.in_line = false;
        self.in_quote = false;
        self.last_was_esc = false;
        self.line_start_at = self.bytes_total + pos as u64;
        let g = self.o.grain;
        if self.chunk_rows > 0 && self.chunk_rows % g == 0 {
            let held = self.seg_bytes + (pos - self.seg_start) as u64;
            if self.chunk_rows >= g * self.o.grains || held >= self.o.target_bytes {
                self.cut(buf, pos, out);
            }
        }
    }

    fn cut(&mut self, buf: &Arc<SegBlock>, pos: usize, out: &mut Vec<ChunkDesc>) {
        if pos > self.seg_start {
            self.segs.push(ChunkSeg { buf: Arc::clone(buf), start: self.seg_start, end: pos });
            self.seg_bytes += (pos - self.seg_start) as u64;
        }
        let segs = std::mem::take(&mut self.segs);
        out.push(ChunkDesc {
            lineno_before: self.chunk_lineno_before,
            eol_at_start: self.chunk_eol,
            rows: self.chunk_rows,
            segs,
            last: false,
            tail_err: None,
        });
        self.seg_start = pos;
        self.seg_bytes = 0;
        self.chunk_rows = 0;
        self.chunk_lineno_before = self.lineno;
        self.chunk_eol = self.eol;
    }

    fn resolve_cr(&mut self, c2: u8, buf: &Arc<SegBlock>, at: usize, out: &mut Vec<ChunkDesc>) -> usize {
        if c2 == b'\n' {
            self.eol = EolType::Crnl;
            self.end_line(buf, at + 1, out);
            at + 1
        } else if self.eol == EolType::Crnl {
            // The serial "literal carriage return" error, raised by the chunk's helper: no cut.
            at
        } else {
            self.eol = EolType::Cr;
            self.end_line(buf, at, out);
            at
        }
    }

    /// `None`: the lookahead is in the next block.
    #[inline]
    fn on_cr(&mut self, buf: &Arc<SegBlock>, i: usize, out: &mut Vec<ChunkDesc>) -> Option<usize> {
        match self.eol {
            EolType::Unknown | EolType::Crnl => {
                if i >= buf.len() {
                    self.pending_cr = true;
                    return None;
                }
                Some(self.resolve_cr(buf[i], buf, i, out))
            }
            EolType::Nl => Some(i),
            EolType::Cr => {
                self.end_line(buf, i, out);
                Some(i)
            }
        }
    }

    #[inline]
    fn on_nl(&mut self, buf: &Arc<SegBlock>, i: usize, out: &mut Vec<ChunkDesc>) {
        match self.eol {
            EolType::Cr | EolType::Crnl => {}
            EolType::Unknown | EolType::Nl => {
                self.eol = EolType::Nl;
                self.end_line(buf, i, out);
            }
        }
    }

    fn scan_csv(&mut self, buf: &Arc<SegBlock>, mut i: usize, out: &mut Vec<ChunkDesc>) {
        let n = buf.len();
        let quotec = self.o.quote;
        let escapec = self.escapec;
        while i < n {
            let start = i;
            {
                let b: &[u8] = buf;
                while i < n && self.class[b[i] as usize] == CLASS_PLAIN {
                    i += 1;
                }
            }
            if i > start {
                self.last_was_esc = false;
                self.in_line = true;
            }
            if i >= n {
                break;
            }
            let c = buf[i];
            i += 1;
            self.in_line = true;
            if self.in_quote && c == escapec {
                self.last_was_esc = !self.last_was_esc;
            }
            if c == quotec && !self.last_was_esc {
                self.in_quote = !self.in_quote;
            }
            if c != escapec {
                self.last_was_esc = false;
            }
            if self.in_quote && c == if self.eol == EolType::Nl { b'\n' } else { b'\r' } {
                self.cur += 1;
            }
            if c == b'\r' && !self.in_quote {
                match self.on_cr(buf, i, out) {
                    Some(next) => i = next,
                    None => return,
                }
                continue;
            }
            if c == b'\n' && !self.in_quote {
                self.on_nl(buf, i, out);
            }
        }
    }

    fn resolve_bs(&mut self, buf: &Arc<SegBlock>, at: usize, at_line_start: bool) -> usize {
        if buf[at] == b'.' && at_line_start {
            // The marker's checks ("not alone", style) are the final chunk's helper's.
            self.eoc = true;
            self.eoc_lineno = self.cur;
            let want = at + 1 + 2;
            if want <= buf.len() {
                self.eoc_need = 0;
                return want;
            }
            self.eoc_need = want - buf.len();
            return buf.len();
        }
        self.in_line = true;
        at + 1
    }

    /// Returns the end of the block's input (the block length, or the end
    /// of an end-of-copy marker's bytes).
    fn scan_text(&mut self, buf: &Arc<SegBlock>, mut i: usize, out: &mut Vec<ChunkDesc>) -> usize {
        let n = buf.len();
        while i < n {
            let start = i;
            {
                let b: &[u8] = buf;
                while i < n && self.class[b[i] as usize] == CLASS_PLAIN {
                    i += 1;
                }
            }
            if i > start {
                self.in_line = true;
            }
            if i >= n {
                break;
            }
            let c = buf[i];
            let at_line_start = !self.in_line;
            i += 1;
            self.in_line = true;
            if c == b'\r' {
                match self.on_cr(buf, i, out) {
                    Some(next) => i = next,
                    None => return n,
                }
                continue;
            }
            if c == b'\n' {
                self.on_nl(buf, i, out);
                continue;
            }
            if i >= n {
                self.pending_bs = true;
                self.pending_bs_at_start = at_line_start;
                return n;
            }
            i = self.resolve_bs(buf, i, at_line_start);
            if self.eoc {
                return i;
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(csv: bool) -> SegOpts {
        SegOpts {
            csv,
            quote: b'"',
            escape: b'"',
            header: false,
            grain: 2,
            grains: 2,
            target_bytes: u64::MAX,
            max_line_bytes: 1 << 30,
        }
    }

    fn run(o: SegOpts, input: &[u8], bs: usize) -> Vec<(Vec<u8>, u64, u64, EolType, bool)> {
        let mut s = Segmentator::new(o);
        let mut out = Vec::new();
        for block in input.chunks(bs.max(1)) {
            let b = Arc::new(seg_block(block));
            s.feed(&b, &mut out).unwrap();
        }
        s.finish(&mut out);
        out.into_iter()
            .map(|c| {
                let mut bytes = Vec::new();
                for sg in &c.segs {
                    bytes.extend_from_slice(&sg.buf[sg.start..sg.end]);
                }
                (bytes, c.rows, c.lineno_before, c.eol_at_start, c.last)
            })
            .collect()
    }

    fn all_block_sizes(o: SegOpts, input: &[u8]) -> Vec<(Vec<u8>, u64, u64, EolType, bool)> {
        let first = run(o, input, input.len().max(1));
        for bs in 1..=input.len().max(1) {
            let r = run(o, input, bs);
            assert_eq!(
                r.iter().map(|c| (c.0.clone(), c.1, c.2, c.4)).collect::<Vec<_>>(),
                first.iter().map(|c| (c.0.clone(), c.1, c.2, c.4)).collect::<Vec<_>>(),
                "block size {bs} changed the cut"
            );
        }
        first
    }

    #[test]
    fn csv_cuts_on_grains_and_counts_quoted_newlines() {
        let input = b"a,1\n\"b\nb\",2\nc,3\nd,4\ne,5\n\"f\n\nf\",6\ng,7\nh,8\ni,9\n";
        let r = all_block_sizes(opts(true), input);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].0, b"a,1\n\"b\nb\",2\nc,3\nd,4\n");
        assert_eq!(r[0].1, 4);
        assert_eq!(r[0].2, 0);
        // Line 1's quoted '\n'? No: chunk 0 row 2 holds a quoted '\n' AFTER the
        // style is decided Nl (line 1 ended with '\n'), so it counts: lines
        // 1..4 + 1 = 5 before chunk 1.
        assert_eq!(r[1].2, 5);
        assert_eq!(r[1].1, 4);
        // Chunk 1 holds two quoted line breaks: 5 + 4 + 2 = 11.
        assert_eq!(r[2].2, 11);
        assert_eq!(r[2].0, b"i,9\n");
        assert!(r[2].4);
        assert_eq!(r[1].3, EolType::Nl);
    }

    #[test]
    fn csv_first_line_quoted_newline_does_not_count_while_unknown() {
        let input = b"\"x\ny\",1\nb,2\nc,3\nd,4\ne,5\n";
        let r = all_block_sizes(opts(true), input);
        assert_eq!(r[1].2, 4);
        let input = b"\"x\ry\",1\nb,2\nc,3\nd,4\ne,5\n";
        let r = all_block_sizes(opts(true), input);
        assert_eq!(r[1].2, 5);
    }

    #[test]
    fn csv_doubled_quotes_and_escape_option() {
        let input = b"\"a\"\"\n\",1\nb,2\nc,3\nd,4\ne,5\n";
        let r = all_block_sizes(opts(true), input);
        assert_eq!(r[0].1, 4);
        assert_eq!(r[1].0, b"e,5\n");
        let mut o = opts(true);
        o.escape = b'\\';
        let input = b"\"a\\\"\n\",1\nb,2\nc,3\nd,4\ne,5\n";
        let r = all_block_sizes(o, input);
        assert_eq!(r[0].1, 4);
        assert_eq!(r[1].0, b"e,5\n");
        let r = all_block_sizes(opts(true), input);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].1, 2);
        assert!(r[0].4);
    }

    #[test]
    fn crlf_and_cr_styles() {
        let input = b"a\r\nb\r\nc\r\nd\r\ne\r\n";
        let r = all_block_sizes(opts(true), input);
        assert_eq!(r[0].0, b"a\r\nb\r\nc\r\nd\r\n");
        assert_eq!(r[1].3, EolType::Crnl);
        let input = b"a\rb\rc\rd\re\r";
        let r = all_block_sizes(opts(false), input);
        assert_eq!(r[0].0, b"a\rb\rc\rd\r");
        assert_eq!(r[1].0, b"e\r");
        assert_eq!(r[1].3, EolType::Cr);
    }

    #[test]
    fn mixed_terminator_is_not_a_cut() {
        let input = b"a\nb\rx\nc\nd\ne\n";
        let r = all_block_sizes(opts(false), input);
        assert_eq!(r[0].0, b"a\nb\rx\nc\nd\n");
        assert_eq!(r[0].1, 4);
    }

    #[test]
    fn text_backslash_escapes_and_marker() {
        let input = b"a\\\nb\nc\nd\ne\nf\n";
        let r = all_block_sizes(opts(false), input);
        assert_eq!(r[0].0, b"a\\\nb\nc\nd\ne\n");
        assert_eq!(r[0].1, 4);
        let input = b"a\nb\nc\nd\ne\n\\.\nzzz\n";
        let r = all_block_sizes(opts(false), input);
        assert_eq!(r.len(), 2);
        assert_eq!(r[1].1, 1);
        assert_eq!(r[1].0, b"e\n\\.\nz");
        let input = b"a\\.\nb\nc\nd\ne\n";
        let r = all_block_sizes(opts(false), input);
        assert_eq!(r[0].1, 4);
    }

    #[test]
    fn header_is_not_a_row() {
        let mut o = opts(true);
        o.header = true;
        let input = b"h1,h2\na,1\nb,2\nc,3\nd,4\ne,5\n";
        let r = all_block_sizes(o, input);
        assert_eq!(r[0].0, b"h1,h2\na,1\nb,2\nc,3\nd,4\n");
        assert_eq!(r[0].1, 4);
        assert_eq!(r[1].2, 5);
    }

    #[test]
    fn final_unterminated_line_is_a_row() {
        let input = b"a\nb\nc\nd\ne";
        let r = all_block_sizes(opts(false), input);
        assert_eq!(r[1].0, b"e");
        assert_eq!(r[1].1, 1);
        let input = b"a\nb\nc\nd\n";
        let r = all_block_sizes(opts(false), input);
        assert_eq!(r.len(), 2);
        assert!(r[1].0.is_empty());
        assert_eq!(r[1].1, 0);
        assert_eq!(r[1].2, 4);
    }

    #[test]
    fn cursor_reports_tail_error_after_bytes() {
        let b = Arc::new(seg_block(b"abcdef"));
        let segs = vec![ChunkSeg { buf: Arc::clone(&b), start: 1, end: 3 }, ChunkSeg { buf: b, start: 4, end: 6 }];
        let mut c = ChunkCursor::new(segs, Some(Box::new(PgError::error("boom".to_string()))));
        let mut dst = [0u8; 3];
        assert_eq!(c.read(&mut dst).unwrap(), 3);
        assert_eq!(&dst, b"bce");
        assert_eq!(c.read(&mut dst).unwrap(), 1);
        assert!(c.read(&mut dst).is_err());
        assert_eq!(c.read(&mut dst).unwrap(), 0);
    }
}
