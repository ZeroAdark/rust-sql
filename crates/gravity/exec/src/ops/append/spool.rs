//! The spooled Append at the entry (rung12-subplan.md "Eighth landing", need 15): every arm program's rows pushed into one statement-held spool — rows as `gravity_lower::spool::encode_row` stores them, whole rows per run,
//! runs written as the driver's `run_put` writes them — that the consumer program's `Materialize` slot reads as its run stream (`into_parts`). The columns are `appendspool::Spooled.cols`.
use std::sync::Arc;

use pgsync::Mutex;

use gravity_driver::{BorrowedBatch, Error, Flow, Result, RunReader, Sink, SpillFile, SpillSet, MAX_RUNS, UNSUPPORTED_SPILL};

/// A run closes at the first row that would pass this (a wider row is a run alone; the reader's buffer bounds a row).
pub const RUN_BYTES: usize = 1 << 16;

pub struct ArmSpool {
    pub(super) cols: Vec<Option<i16>>,
    set: Option<Box<dyn SpillSet>>,
    file: Box<dyn SpillFile>,
    pub(super) run: Vec<u8>,
    nruns: u32,
    pub(super) rows: u64,
}

impl ArmSpool {
    /// A spool over the session's spill files (fail-closed without a factory, as the driver's runs).
    pub fn new(cols: &[Option<i16>]) -> Result<ArmSpool> {
        let mut set = gravity_driver::mem::spill::new_set()?;
        let file = set.open("armspool", 0, 0, MAX_RUNS)?;
        Ok(ArmSpool::over(cols, Some(set), file))
    }

    /// A spool held in memory (the rig's, and a union the entry knows to be small).
    pub fn in_memory(cols: &[Option<i16>]) -> ArmSpool {
        ArmSpool::over(cols, None, Box::new(MemRuns(Arc::new(Mutex::new(Vec::new()))))) // alloc: Exec
    }

    fn over(cols: &[Option<i16>], set: Option<Box<dyn SpillSet>>, file: Box<dyn SpillFile>) -> ArmSpool {
        ArmSpool { cols: cols.to_vec(), set, file, run: Vec::with_capacity(RUN_BYTES), nruns: 0, rows: 0 } // alloc: Exec
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    pub(super) fn put(&mut self) -> Result<()> {
        if self.run.is_empty() {
            return Ok(());
        }
        if self.nruns >= MAX_RUNS {
            return Err(Error::Unsupported(UNSUPPORTED_SPILL));
        }
        self.file.write_epoch(&[(self.nruns, self.run.as_slice())])?;
        self.nruns += 1;
        self.run.clear();
        Ok(())
    }

    /// After the last arm: the last run put; the set (kept alive beside the file), the run file and its run count for the consumer's stream.
    pub fn into_parts(mut self) -> Result<(Option<Box<dyn SpillSet>>, Box<dyn SpillFile>, u32)> {
        self.put()?;
        Ok((self.set, self.file, self.nruns))
    }
}

impl Sink for ArmSpool {
    fn push(&mut self, b: BorrowedBatch<'_>) -> Result<Flow> {
        let natts = self.cols.len();
        if b.natts != natts {
            return Err(Error::host("gravity: an arm's row width is not the spool's"));
        }
        self.push_rows(&b)?;
        Ok(Flow::Continue)
    }
}

/// The in-memory run file: run `i` is partition `i`.
struct MemRuns(Arc<Mutex<Vec<Vec<u8>>>>);

struct MemReader(Arc<Mutex<Vec<Vec<u8>>>>, usize, usize);

impl SpillFile for MemRuns {
    fn write_epoch(&mut self, parts: &[(u32, &[u8])]) -> Result<()> {
        let mut runs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for (part, bytes) in parts {
            if runs.len() <= *part as usize {
                runs.resize_with(*part as usize + 1, Vec::new); // alloc: Exec
            }
            runs[*part as usize].extend_from_slice(bytes); // alloc: Exec
        }
        Ok(())
    }

    fn read_part(&self, part: u32, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let runs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match runs.get(part as usize) {
            Some(r) if !r.is_empty() => sink(r),
            _ => Ok(()),
        }
    }

    fn open_part(&self, part: u32) -> Result<Option<Box<dyn RunReader>>> {
        let runs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match runs.get(part as usize) {
            Some(r) if !r.is_empty() => {}
            _ => return Ok(None),
        }
        Ok(Some(Box::new(MemReader(self.0.clone(), part as usize, 0)))) // alloc: Exec
    }

    fn spilled_bytes(&self) -> u64 {
        0
    }
}

impl RunReader for MemReader {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let runs = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let r = &runs[self.1];
        let n = buf.len().min(r.len() - self.2);
        buf[..n].copy_from_slice(&r[self.2..self.2 + n]);
        self.2 += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_lower::rerun::spool::encode_row;

    fn runs_of(s: ArmSpool) -> (Vec<Vec<u8>>, u32) {
        let (_, file, nruns) = s.into_parts().unwrap();
        let mut runs: Vec<Vec<u8>> = Vec::new(); // alloc: Cold
        for i in 0..nruns {
            let mut run: Vec<u8> = Vec::new(); // alloc: Cold
            let mut rd = file.open_part(i).unwrap().expect("a put run holds rows");
            let mut buf = [0u8; 1000];
            loop {
                let n = rd.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                run.extend_from_slice(&buf[..n]);
            }
            runs.push(run);
        }
        (runs, nruns)
    }

    #[test]
    fn by_value_rows_and_nulls_are_encode_rows() {
        let mut s = ArmSpool::in_memory(&[None, None, None]);
        let (values, nulls) = ([7u64, 0, u64::MAX, 1, 2, 3], [0u8, 1, 0, 0, 0, 1]);
        s.push(BorrowedBatch { natts: 3, rows: 2, values: &values, isnull: &nulls, region: &[] }).unwrap();
        assert_eq!(s.rows(), 2);
        let mut want: Vec<u8> = Vec::new(); // alloc: Cold
        encode_row(&mut want, &[7, 0, u64::MAX], &[0, 1, 0], &[None, None, None]);
        encode_row(&mut want, &[1, 2, 0], &[0, 0, 1], &[None, None, None]);
        let (runs, nruns) = runs_of(s);
        assert_eq!(nruns, 1);
        assert_eq!(runs[0], want);
    }

    #[test]
    fn by_reference_images_are_offsets_from_the_row() {
        // a 1-byte-header varlena of 5 bytes, a 4-byte-header varlena of 13 bytes, a fixed 16-byte image
        let short = [(5u8 << 1) | 1, b'a', b'b', b'c', b'd'];
        let mut long = [0u8; 13];
        long[..4].copy_from_slice(&(13u32 << 2).to_ne_bytes());
        long[4..].copy_from_slice(b"123456789");
        let fixed = [9u8; 16];
        let mut s = ArmSpool::in_memory(&[Some(-1), None, Some(-1), Some(16)]);
        let values = [short.as_ptr() as u64, 42, long.as_ptr() as u64, fixed.as_ptr() as u64, 0, 43, long.as_ptr() as u64, 0];
        let nulls = [0u8, 0, 0, 0, 1, 0, 0, 1];
        s.push(BorrowedBatch { natts: 4, rows: 2, values: &values, isnull: &nulls, region: &[] }).unwrap();
        let mut want: Vec<u8> = Vec::new(); // alloc: Cold
        encode_row(&mut want, &[0, 42, 0, 0], &[0, 0, 0, 0], &[Some(&short[..]), None, Some(&long[..]), Some(&fixed[..])]);
        encode_row(&mut want, &[0, 43, 0, 0], &[1, 0, 0, 1], &[None, None, Some(&long[..]), None]);
        let (runs, _) = runs_of(s);
        assert_eq!(runs[0], want);
    }

    #[test]
    fn runs_hold_whole_rows_and_concatenate_to_the_oracle() {
        let mut s = ArmSpool::in_memory(&[None, None]);
        let mut want: Vec<u8> = Vec::new(); // alloc: Cold
        let n = 3 * RUN_BYTES as u64 / 32;
        for r in 0..n {
            s.push(BorrowedBatch { natts: 2, rows: 1, values: &[r, !r], isnull: &[0, 0], region: &[] }).unwrap();
            encode_row(&mut want, &[r, !r], &[0, 0], &[None, None]);
        }
        let (runs, nruns) = runs_of(s);
        assert!(nruns >= 3, "{nruns} runs");
        for run in runs.iter() {
            assert!(run.len() <= RUN_BYTES && run.len() % 32 == 0, "{}", run.len());
        }
        assert_eq!(runs.concat(), want);
    }

    #[test]
    fn zero_column_rows_are_their_length_words() {
        let mut s = ArmSpool::in_memory(&[]);
        s.push(BorrowedBatch { natts: 0, rows: 3, values: &[], isnull: &[], region: &[] }).unwrap();
        let mut want: Vec<u8> = Vec::new(); // alloc: Cold
        for _ in 0..3 {
            encode_row(&mut want, &[], &[], &[]);
        }
        assert_eq!(s.rows(), 3);
        assert_eq!(runs_of(s).0[0], want);
    }

    #[test]
    fn a_wrong_width_is_an_error_and_an_empty_spool_has_no_runs() {
        let mut s = ArmSpool::in_memory(&[None]);
        assert!(s.push(BorrowedBatch { natts: 2, rows: 1, values: &[1, 2], isnull: &[0, 0], region: &[] }).is_err());
        assert_eq!(runs_of(s).1, 0);
    }
}
