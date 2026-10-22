//! D327: `stream_runs` carries a record straddling two chunks in a buffer of the record's width. A wide record
//! (`LAYOUT_WIDE`: a group of 120 sums is 1008 B) panicked "range end index 1008 out of range for slice of length 256"
//! in the fixed `[u8; 256]` it had; a carry the allocator refuses is the statement's `OutOfMemory`, never an abort.
use gravity_driver::{stream_records, stream_runs, Error, RunReader, SpillFile};

/// One partition's bytes, read back in chunks of `chunk` bytes (a record straddles a chunk boundary unless `chunk` is a multiple).
struct Chunks {
    bytes: Vec<u8>,
    chunk: usize,
}

impl SpillFile for Chunks {
    fn write_epoch(&mut self, _parts: &[(u32, &[u8])]) -> Result<(), Error> {
        unreachable!("a read-only fixture")
    }

    fn read_part(&self, _part: u32, sink: &mut dyn FnMut(&[u8]) -> Result<(), Error>) -> Result<(), Error> {
        for c in self.bytes.chunks(self.chunk) {
            sink(c)?;
        }
        Ok(())
    }

    fn open_part(&self, _part: u32) -> Result<Option<Box<dyn RunReader>>, Error> {
        Ok(None)
    }

    fn spilled_bytes(&self) -> u64 {
        self.bytes.len() as u64
    }
}

/// The record of a group with 120 `int8` sums: key, 120 sums, the count sentinel, the validity words.
const WIDE: usize = 1008;

/// `n` records of `rec` bytes, every byte its record's and its offset's.
fn records(n: usize, rec: usize) -> Vec<u8> {
    (0..n * rec).map(|i| ((i / rec) as u8).wrapping_mul(31) ^ (i % rec) as u8 ^ (i % rec >> 8) as u8).collect()
}

#[test]
fn d327_a_wide_record_straddling_two_chunks_streams_whole() {
    for rec in [WIDE, 257, 4096, 24] {
        for chunk in [1000, 777, 4096, 8192, 65536] {
            let file = Chunks { bytes: records(40, rec), chunk };
            let mut seen = 0usize;
            let n = stream_records(&file, 0, rec, &mut |r: &[u8]| {
                assert_eq!(r, &file.bytes[seen * rec..(seen + 1) * rec], "record {seen} of {rec} B in chunks of {chunk}");
                seen += 1;
                Ok(())
            })
            .expect("streamed");
            assert_eq!((n, seen), (40, 40), "records of {rec} B in chunks of {chunk}");
        }
    }
}

#[test]
fn d327_a_carry_the_allocator_refuses_is_the_statements_out_of_memory() {
    let file = Chunks { bytes: vec![7u8; 3000], chunk: 1000 };
    // past isize::MAX: the reservation's capacity overflow, the refusal path without exhausting the machine
    let rec = isize::MAX as usize + 1;
    let got = stream_runs(&file, 0, rec, &mut |_, _| panic!("no whole record of {rec} B"));
    assert!(matches!(got, Err(Error::OutOfMemory { bytes, .. }) if bytes == rec as u64), "{got:?}");
}
