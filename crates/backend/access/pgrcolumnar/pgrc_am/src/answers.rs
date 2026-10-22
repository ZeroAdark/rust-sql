//! The part-grain answers of a generation as the catalog holds them, every part's part-grain Stats record served
//! by `(attno, part)` from memory — the footer-answered classes and the part-grain zone verdicts read nothing. Two
//! forms: the generation's stats runs and loose parts (P-1: `pgrc_write::stats`, split in place through a manifest
//! position index, no column is assembled), or one column payload per column (`pgrc_format::answers`: a pre-P-1
//! sections file's `SECTION_ANSWERS` sections, or a completion's walk).

use pgrc_format::answers::{ColumnAnswers, DIGEST_LEN};
use pgrc_format::part::SectionKind;
use pgrc_format::sections::{SectionsFile, SECTION_ANSWERS};
use pgrc_format::FormatResult;
use pgrc_read::OpenPart;
use pgrc_write::stats::StatsState;
use std::sync::Arc;
use types_error::PgResult;

pub struct Answers {
    parts: usize,
    form: Form,
}

enum Form {
    Columns {
        /// `(attno, payload)` in section order.
        cols: Vec<(u32, Vec<u8>)>,
        /// attno → column index + 1 (0 = the column has no answers).
        index: Vec<u32>,
    },
    Runs {
        state: Arc<StatsState>,
        /// Manifest position → (source, index) in `state` (`StatsState::index`).
        order: Vec<(u32, u32)>,
        /// The columns answered, ascending (`build_from_parts`'s rule: the first part's).
        attnos: Vec<u32>,
    },
}

impl Answers {
    pub fn new(parts: usize, cols: Vec<(u32, Vec<u8>)>) -> FormatResult<Answers> {
        let mut index = Vec::new(); // alloc: Cold
        for (i, (attno, payload)) in cols.iter().enumerate() {
            ColumnAnswers::parse(payload, parts)?;
            let a = *attno as usize;
            if index.len() <= a {
                pgrc_format::try_resize(&mut index, a + 1, 0, "answers")?;
            }
            index[a] = i as u32 + 1;
        }
        Ok(Answers { parts, form: Form::Columns { cols, index } })
    }

    /// A generation's answers over its stats (P-1): O(parts) for the index, no record copied. `Err` when `state`
    /// does not cover `m` (the catalog checked it at publish or load).
    pub fn from_state(state: Arc<StatsState>, m: &pgrc_format::manifest::Manifest) -> Result<Answers, pgrc_write::stats::OrderError> {
        let order = state.try_index(m)?;
        let mut attnos = state.answer_attnos(&order);
        attnos.sort_unstable();
        Ok(Answers { parts: order.len(), form: Form::Runs { state, order, attnos } })
    }

    /// The answers sections of a sections file for `parts` parts; `None` when it carries none.
    pub fn from_file(file: &SectionsFile, parts: usize) -> FormatResult<Option<Answers>> {
        let mut cols = Vec::new(); // alloc: Cold
        for (attno, i) in file.all(SECTION_ANSWERS) {
            cols.push((attno, pgrc_format::try_to_vec(file.section(i)?, "answers")?));
        }
        if cols.is_empty() {
            return Ok(None);
        }
        Answers::new(parts, cols).map(Some)
    }

    /// The column's view (the columns form): every payload was validated once at `new`, so a lookup only splits it.
    fn col(&self, attno: u32) -> Option<ColumnAnswers<'_>> {
        let Form::Columns { cols, index } = &self.form else { return None };
        let i = *index.get(attno as usize)?;
        if i == 0 {
            return None;
        }
        Some(ColumnAnswers::validated(&cols[i as usize - 1].1, self.parts))
    }

    /// The runs form's record and digest of `part` for an answered `attno`.
    fn run_answer(&self, attno: u32, part: usize) -> Option<(&[u8], Option<&[u8]>)> {
        let Form::Runs { state, order, attnos } = &self.form else { return None };
        attnos.binary_search(&attno).ok()?;
        Some(state.part_answer(*order.get(part)?, attno))
    }

    /// Part `part`'s part-grain Stats record for `attno`; `None` when the part or the column carries none.
    pub fn part_record(&self, attno: u32, part: usize) -> Option<&[u8]> {
        match &self.form {
            Form::Columns { .. } => self.col(attno)?.stats_body(part),
            Form::Runs { .. } => self.run_answer(attno, part).map(|a| a.0).filter(|b| !b.is_empty()),
        }
    }

    pub fn digest(&self, attno: u32, part: usize) -> Option<&[u8]> {
        match &self.form {
            Form::Columns { .. } => self.col(attno)?.digest(part),
            Form::Runs { .. } => self.run_answer(attno, part)?.1.filter(|d| d.iter().any(|&b| b != 0)),
        }
    }

    pub fn has_column(&self, attno: u32) -> bool {
        match &self.form {
            Form::Columns { index, .. } => index.get(attno as usize).is_some_and(|&i| i != 0),
            Form::Runs { attnos, .. } => attnos.binary_search(&attno).is_ok(),
        }
    }

    pub fn parts(&self) -> usize {
        self.parts
    }

    /// The columns form's payloads (a root-less generation's, the seed of the publisher's legacy run, OC-5); `None`
    /// for the runs form, whose stats the publisher takes whole.
    pub fn columns(&self) -> Option<&[(u32, Vec<u8>)]> {
        match &self.form {
            Form::Columns { cols, .. } => Some(cols),
            Form::Runs { .. } => None,
        }
    }

    pub fn bytes(&self) -> u64 {
        match &self.form {
            Form::Columns { cols, .. } => cols.iter().map(|(_, p)| p.len() as u64).sum(),
            Form::Runs { state, order, .. } => state.answer_bytes() + 8 * order.len() as u64,
        }
    }
}

/// The answers of `parts` built from the parts themselves (the walk a
/// generation without a sections file pays once): the columns are those the
/// first part carries Stats sections for; each body's part-grain record.
pub fn build_from_parts(parts: &[&OpenPart]) -> PgResult<Vec<(u32, Vec<u8>)>> {
    let Some(first) = parts.first() else { return Ok(Vec::new()) };
    let mut attnos: Vec<u32> = first.sections().iter().filter(|e| e.kind == SectionKind::Stats as u16 && e.path_ord == 0).map(|e| e.attno).collect(); // alloc: Cold
    attnos.sort_unstable();
    attnos.dedup();
    let mut out = crate::alloc::try_vec(attnos.len(), "answers")?; // alloc: Cold
    for attno in attnos {
        let mut bodies: Vec<Vec<u8>> = crate::alloc::try_vec(parts.len(), "answers")?; // alloc: Cold
        let mut digests: Vec<Option<[u8; DIGEST_LEN]>> = crate::alloc::try_vec(parts.len(), "answers")?; // alloc: Cold
        for part in parts {
            bodies.push(match part.find_section(SectionKind::Stats, attno, 0) {
                Some(idx) => crate::alloc::try_to_vec(pgrc_write::bankplane::part_record(&crate::footer::meta_section_body(part, idx)?), "answers")?,
                None => Vec::new(),
            });
            digests.push(match part.find_section(SectionKind::PartDigest, attno, 0) {
                Some(idx) => <[u8; DIGEST_LEN]>::try_from(crate::footer::meta_section_body(part, idx)?.as_slice()).ok(),
                None => None,
            });
        }
        let refs: Vec<&[u8]> = bodies.iter().map(|b| b.as_slice()).collect(); // alloc: Cold
        out.push((attno, pgrc_format::answers::encode_column(&refs, &digests).map_err(crate::format_error)?));
    }
    Ok(out)
}
