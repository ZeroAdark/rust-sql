//! Sort-key record (spec §9): parts record their sort key; ordering is a
//! format property feeding sort elision and aggregation-in-order.

use crate::class::CollationClass;
use crate::wire::{put_u16, put_u32, put_u8, Cur};
use crate::{FormatError, FormatResult};

pub const SORT_KEY_ENTRY_LEN: usize = 8;

/// SortKeyRecord `flags`: a path table follows the entries (additive; a
/// reader that predates it refuses the record on its length check).
pub const SORTKEYF_PATHS: u16 = 1 << 0;

/// `SortKeyEntry::pad` as the key kind: 0 = the column itself; this = the
/// text value of a jsonb path on the column (`->` steps, final `->>`).
pub const SORT_KEY_KIND_TEXT_PATH: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SortDir {
    Asc = 0,
    Desc = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NullsOrder {
    First = 0,
    Last = 1,
}

/// One sort-key column (8 B). Wire order == declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct SortKeyEntry {
    pub attno: u32,
    pub dir: u8,
    pub nulls: u8,
    pub collation_class: u8,
    pub pad: u8,
}

/// The SortKey section body: `{ nkeys: u16, flags: u16, pad: u32 }` +
/// entries. `nkeys == 0` = unordered part. With [`SORTKEYF_PATHS`], one path
/// per TEXT_PATH entry follows, in entry order: `{ nsteps: u16 }` then per
/// step `{ len: u16, key bytes }`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SortKeyRecord {
    pub keys: Vec<SortKeyEntry>,
    /// Per entry, its jsonb object-key path (empty for a column key).
    /// Empty vector = every key is a column.
    pub paths: Vec<Vec<Vec<u8>>>,
}

impl SortKeyRecord {
    /// Entry `i`'s path (empty = the column itself).
    pub fn path(&self, i: usize) -> &[Vec<u8>] {
        self.paths.get(i).map(Vec::as_slice).unwrap_or(&[])
    }
}

impl SortKeyRecord {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.keys.len() * SORT_KEY_ENTRY_LEN);
        let has_paths = (0..self.keys.len()).any(|i| !self.path(i).is_empty());
        put_u16(&mut out, self.keys.len() as u16);
        put_u16(&mut out, if has_paths { SORTKEYF_PATHS } else { 0 });
        put_u32(&mut out, 0);
        for k in &self.keys {
            put_u32(&mut out, k.attno);
            put_u8(&mut out, k.dir);
            put_u8(&mut out, k.nulls);
            put_u8(&mut out, k.collation_class);
            put_u8(&mut out, k.pad);
        }
        if has_paths {
            for (i, k) in self.keys.iter().enumerate() {
                if k.pad == SORT_KEY_KIND_TEXT_PATH {
                    let p = self.path(i);
                    put_u16(&mut out, p.len() as u16);
                    for step in p {
                        put_u16(&mut out, step.len() as u16);
                        out.extend_from_slice(step);
                    }
                }
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> FormatResult<SortKeyRecord> {
        let mut c = Cur::new(bytes);
        let nkeys = c.u16("SortKeyRecord")?;
        let flags = c.u16("SortKeyRecord")?;
        let _pad = c.u32("SortKeyRecord")?;
        if flags & !SORTKEYF_PATHS != 0 {
            return Err(FormatError::Corrupt {
                at: "SortKeyRecord flags",
            });
        }
        let has_paths = flags & SORTKEYF_PATHS != 0;
        if (has_paths && c.remaining() < nkeys as usize * SORT_KEY_ENTRY_LEN)
            || (!has_paths && c.remaining() != nkeys as usize * SORT_KEY_ENTRY_LEN)
        {
            return Err(FormatError::Corrupt {
                at: "SortKeyRecord length",
            });
        }
        let mut keys = Vec::with_capacity(nkeys as usize);
        for _ in 0..nkeys {
            let e = SortKeyEntry {
                attno: c.u32("SortKeyEntry")?,
                dir: c.u8("SortKeyEntry")?,
                nulls: c.u8("SortKeyEntry")?,
                collation_class: c.u8("SortKeyEntry")?,
                pad: c.u8("SortKeyEntry")?,
            };
            if e.dir > 1 || e.nulls > 1 || e.pad > SORT_KEY_KIND_TEXT_PATH || (e.pad != 0 && !has_paths) {
                return Err(FormatError::Corrupt {
                    at: "SortKeyEntry dir/nulls",
                });
            }
            CollationClass::from_u8(e.collation_class)?;
            keys.push(e);
        }
        let mut paths = Vec::new();
        if has_paths {
            for k in &keys {
                let mut p = Vec::new();
                if k.pad == SORT_KEY_KIND_TEXT_PATH {
                    let nsteps = c.u16("SortKeyRecord path")?;
                    if nsteps == 0 {
                        return Err(FormatError::Corrupt {
                            at: "SortKeyRecord empty path",
                        });
                    }
                    for _ in 0..nsteps {
                        let len = c.u16("SortKeyRecord path step")? as usize;
                        p.push(crate::try_to_vec(c.take(len, "SortKeyRecord path step")?, "sortkey")?);
                    }
                }
                paths.push(p);
            }
            if c.remaining() != 0 {
                return Err(FormatError::Corrupt {
                    at: "SortKeyRecord length",
                });
            }
        }
        Ok(SortKeyRecord { keys, paths })
    }
}
