
use ::datum::Datum;

use crate::rep::ColRep;
use crate::selection::Selection;
use crate::validity::Validity;

#[derive(Debug)]
pub struct Column {
    pub rep: ColRep,
    pub base: ColRep,
    pub datums: Vec<Datum>,
    pub validity: Validity,
    pub text_span: Option<crate::TextSpan>,
}

impl Column {
    pub fn new(base: ColRep) -> Column {
        debug_assert!(base.is_base(), "Column::new wants a base tag, got {base:?}");
        Column { rep: base, base, datums: Vec::new(), validity: Validity::new(), text_span: None }
    }

    pub fn ensure_rows(&mut self, nrows: usize) {
        if self.datums.len() < nrows {
            self.datums.resize(nrows, Datum::null());
        }
        if self.validity.words.len() < nrows.div_ceil(64) {
            self.validity.words.resize(nrows.div_ceil(64), 0);
        }
    }

    pub fn set_const(&mut self, value: Datum, is_null: bool, nrows: usize) {
        self.ensure_rows(nrows.max(1));
        self.datums[0] = value;
        if is_null {
            self.validity.reset_all_null(nrows.max(1));
        } else {
            self.validity.reset_all_valid(nrows.max(1));
        }
        self.rep = ColRep::Const;
    }

    pub fn gather_back(&mut self, nrows: usize) {
        match self.rep {
            ColRep::DictCodes(dc) => {
                self.ensure_rows(nrows);
                // SAFETY: the publish contract (claim-scoped codes + live
                // dict provider; all-valid column per the zero-null-proof
                // rule) — asserted by the producer at publish time.
                let codes = unsafe { dc.codes(nrows) };
                let space = unsafe { dc.dict().space() };
                debug_assert!(
                    space.base_rep().same_base_class(self.base),
                    "dict base_rep {:?} not the column's base class {:?}",
                    space.base_rep(),
                    self.base
                );
                for row in 0..nrows {
                    if self.validity.is_valid(row) {
                        self.datums[row] = space.entry_datum(codes[row]);
                    } else {
                        self.datums[row] = Datum::null();
                    }
                }
                self.rep = space.base_rep();
            }
            ColRep::StrView(_) => {
                debug_assert!(
                    matches!(self.base, ColRep::Varlena { .. }),
                    "StrView overlay on a non-varlena base {:?}",
                    self.base
                );
                self.rep = ColRep::Varlena { inline_proven: true };
            }
            ColRep::Const => {
                self.ensure_rows(nrows);
                let value = self.datums[0];
                let valid = self.validity.is_valid(0);
                for cell in &mut self.datums[..nrows] {
                    *cell = value;
                }
                if valid {
                    self.validity.reset_all_valid(nrows);
                } else {
                    self.validity.reset_all_null(nrows);
                }
                self.rep = self.base;
            }
            _ => debug_assert!(self.rep.is_base()),
        }
    }
}

#[derive(Debug, Default)]
pub struct Batch {
    pub cols: Vec<Column>,
    pub nrows: u32,
    pub sel: Selection,
}

impl Batch {
    pub fn new() -> Batch {
        Batch { cols: Vec::new(), nrows: 0, sel: Selection::new() }
    }

    pub fn begin(&mut self) {
        self.nrows = 0;
        self.sel.clear();
        for col in &mut self.cols {
            col.rep = col.base;
            col.text_span = None;
        }
    }

    pub fn sel_all(&mut self) {
        self.sel.fill_identity(self.nrows);
    }
}
