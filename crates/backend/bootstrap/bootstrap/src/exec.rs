
use std::ffi::CString;

use datum::Datum;
use mcx::Mcx;
use types_core::{
    InvalidOid, Oid, BOOTSTRAP_SUPERUSERID, C_COLLATION_OID, PG_CATALOG_NAMESPACE,
    RELPERSISTENCE_PERMANENT,
};
use types_error::{PgError, PgResult, DEBUG4, WARNING};
use types_nodes::rawnodes::{IndexElem, IndexStmt, SortByDir, SortByNulls};
use types_nodes::{Node, NodeList};
use types_rel::{Relation, RELKIND_RELATION};
use types_storage::lock::NoLock;
use types_tuple::{FormData_pg_attribute, InvalidCompressionMethod, NameData};

use crate::bki::{Column, Nullness, Stmt};
use crate::typinfo::{TypMap, TYP_INFO};

const GLOBALTABLESPACE_OID: Oid = 1664;
const HEAP_TABLE_AM_OID: Oid = 2;
const ANUM_PG_TYPE_OID: i32 = 1;
const ANUM_PG_TYPE_TYPNAME: i32 = 2;
const ANUM_PG_TYPE_TYPLEN: i32 = 5;
const ANUM_PG_TYPE_TYPBYVAL: i32 = 6;
const ANUM_PG_TYPE_TYPDELIM: i32 = 11;
const ANUM_PG_TYPE_TYPELEM: i32 = 14;
const ANUM_PG_TYPE_TYPINPUT: i32 = 16;
const ANUM_PG_TYPE_TYPOUTPUT: i32 = 17;
const ANUM_PG_TYPE_TYPALIGN: i32 = 23;
const ANUM_PG_TYPE_TYPSTORAGE: i32 = 24;
const ANUM_PG_TYPE_TYPCOLLATION: i32 = 29;

fn err(msg: impl Into<String>) -> Box<PgError> {
    Box::new(PgError::error(msg.into()))
}

fn str_in<'mcx>(mcx: Mcx<'mcx>, s: &str) -> PgResult<&'mcx str> {
    let v = mcx::slice_in(mcx, s.as_bytes())?;
    // SAFETY: verbatim copy of a &str.
    Ok(unsafe { core::str::from_utf8_unchecked(v.leak()) })
}

fn copy_byref_datum<'mcx>(mcx: Mcx<'mcx>, d: Datum, typlen: i16) -> PgResult<Datum> {
    if d.as_usize() == 0 {
        return Err(err("input function returned a NULL pointer for a non-null value"));
    }
    let bytes: &[u8] = if typlen > 0 {
        // SAFETY: a non-null pointer to typlen readable bytes (the input
        // function's fixed-width result).
        unsafe { core::slice::from_raw_parts(d.as_usize() as *const u8, typlen as usize) }
    } else if typlen == -1 {
        // SAFETY: a non-null varlena pointer with a readable header.
        let v = unsafe { types_fmgr::datum_varlena_packed(d, mcx)? };
        v.image()
    } else {
        return Err(err(format!("unsupported typlen {typlen} in bootstrap")));
    };
    let copied = mcx::slice_in(mcx, bytes)?;
    Ok(Datum::from_usize(copied.leak().as_ptr() as usize))
}

fn range_var(name: &str) -> rel_vocab::RangeVar<'_> {
    rel_vocab::RangeVar {
        catalogname: None,
        schemaname: None,
        relname: name,
        inh: true,
        relpersistence: RELPERSISTENCE_PERMANENT,
        location: -1,
    }
}

pgsync::process_global! {
    static TYP: pgsync::Mutex<Option<Vec<TypMap>>> = pgsync::Mutex::new(None);
}

fn typ_lookup<R>(f: impl FnOnce(Option<&Vec<TypMap>>) -> R) -> R {
    let g = TYP.lock().unwrap_or_else(|e| e.into_inner());
    f(g.as_ref())
}

pub fn boot_get_type_io_data(typid: Oid) -> PgResult<bootstrap_seams::BootTypeIoData> {
    typ_lookup(|typ| {
        if let Some(list) = typ {
            let t = list.iter().find(|t| t.oid == typid).ok_or_else(|| err(format!("type OID {typid} not found in Typ list")))?;
            return Ok(bootstrap_seams::BootTypeIoData {
                typlen: t.len,
                typbyval: t.byval,
                typalign: t.align,
                typdelim: t.delim,
                typioparam: if t.elem != InvalidOid { t.elem } else { t.oid },
                typinput: t.input,
                typoutput: t.output,
            });
        }
        let t = TYP_INFO.iter().find(|t| t.oid == typid).ok_or_else(|| err(format!("type OID {typid} not found in TypInfo")))?;
        Ok(bootstrap_seams::BootTypeIoData {
            typlen: t.len,
            typbyval: t.byval,
            typalign: t.align,
            typdelim: b',' as i8,
            typioparam: if t.elem != InvalidOid { t.elem } else { t.oid },
            typinput: t.inproc,
            typoutput: t.outproc,
        })
    })
}

pub struct Boot<'mcx> {
    mcx: Mcx<'mcx>,
    reldesc: Option<Relation<'mcx>>,
    attrs: Vec<FormData_pg_attribute>,
    inputs: std::collections::BTreeMap<Oid, types_fmgr::FmgrInfo>,
    values: Vec<Datum>,
    nulls: Vec<bool>,
}

impl<'mcx> Boot<'mcx> {
    pub fn new(mcx: Mcx<'mcx>) -> Self {
        Boot { mcx, reldesc: None, attrs: Vec::new(), inputs: std::collections::BTreeMap::new(), values: Vec::new(), nulls: Vec::new() }
    }

    pub fn run(&mut self, stmt: &Stmt) -> PgResult<()> {
        let _ = elog::elog(
            DEBUG4,
            match stmt {
                Stmt::Open(n) => format!("boot stmt: open {n}"),
                Stmt::Close(n) => format!("boot stmt: close {n}"),
                Stmt::Create { name, oid, .. } => format!("boot stmt: create {name} {oid}"),
                Stmt::Insert(v) => format!("boot stmt: insert ({} values)", v.len()),
                Stmt::DeclareIndex { name, oid, on, .. } => {
                    format!("boot stmt: declare index {name} {oid} on {on}")
                }
                Stmt::DeclareToast { toast_oid, index_oid, on } => {
                    format!("boot stmt: declare toast {toast_oid} {index_oid} on {on}")
                }
                Stmt::BuildIndices => "boot stmt: build indices".to_string(),
            },
        );
        match stmt {
            Stmt::Open(name) => self.openrel(name),
            Stmt::Close(name) => self.closerel(Some(name)),
            Stmt::Create { name, oid, bootstrap, shared, rowtype_oid, columns } => {
                self.create(name, *oid, *bootstrap, *shared, *rowtype_oid, columns)
            }
            Stmt::Insert(vals) => self.insert(vals),
            Stmt::DeclareIndex { unique, name, oid, on, using, params } => {
                self.declare_index(*unique, name, *oid, on, using, params)
            }
            Stmt::DeclareToast { toast_oid, index_oid, on } => {
                catalog_toasting::BootstrapToastTable(self.mcx, on, *toast_oid, *index_oid)
            }
            Stmt::BuildIndices => self.build_indices(),
        }
    }

    pub fn cleanup(&mut self) -> PgResult<()> {
        if self.reldesc.is_some() {
            self.closerel(None)?;
        }
        Ok(())
    }


    fn openrel(&mut self, relname: &str) -> PgResult<()> {
        if typ_lookup(|t| t.is_none()) {
            self.populate_typ_list()?;
        }
        if self.reldesc.is_some() {
            self.closerel(None)?;
        }
        let _ = elog::elog(DEBUG4, format!("open relation {relname}"));
        let rel = table::table_openrv(self.mcx, &range_var(relname), NoLock)?;
        let desc = rel.descr();
        self.attrs.clear();
        self.attrs.extend_from_slice(&desc.attrs[..desc.natts as usize]);
        self.reldesc = Some(rel);
        Ok(())
    }

    fn closerel(&mut self, relname: Option<&str>) -> PgResult<()> {
        match (self.reldesc.take(), relname) {
            (None, Some(n)) => Err(err(format!("close of {n} before any relation was opened"))),
            (None, None) => Err(err("no open relation to close")),
            (Some(rel), name) => {
                if let Some(n) = name {
                    if rel.name() != n {
                        return Err(err(format!("close of {n} when {} was expected", rel.name())));
                    }
                }
                let _ = elog::elog(DEBUG4, format!("close relation {}", rel.name()));
                rel.close(NoLock)
            }
        }
    }


    fn define_attr(&mut self, attnum: usize, col: &Column) -> PgResult<()> {
        if self.reldesc.is_some() {
            let _ = elog::elog(WARNING, "no open relations allowed with CREATE command");
            self.closerel(None)?;
        }
        if self.attrs.len() <= attnum {
            while self.attrs.len() <= attnum {
                self.attrs.push(FormData_pg_attribute::default());
            }
        }
        let mut a = FormData_pg_attribute::default();
        a.attname = NameData::default();
        a.attname.namestrcpy(&col.name);
        a.attnum = attnum as i16 + 1;
        let (typid, len, byval, align, storage, collation, elem) = self.gettype(&col.typ)?;
        a.atttypid = typid;
        a.attlen = len;
        a.attbyval = byval;
        a.attalign = align;
        a.attstorage = storage;
        a.attcompression = InvalidCompressionMethod;
        a.attcollation = collation;
        a.attndims = if elem != InvalidOid && len < 0 { 1 } else { 0 };
        if a.attcollation != InvalidOid {
            a.attcollation = C_COLLATION_OID;
        }
        a.atttypmod = -1;
        a.attislocal = true;
        match col.nullness {
            Nullness::ForceNotNull => a.attnotnull = true,
            Nullness::ForceNull => a.attnotnull = false,
            Nullness::Auto => {
                if a.attlen > 0 {
                    let prefix_ok = self.attrs[..attnum].iter().all(|p| p.attlen > 0 && p.attnotnull);
                    if prefix_ok {
                        a.attnotnull = true;
                    }
                }
            }
        }
        self.attrs[attnum] = a;
        Ok(())
    }

    fn create(&mut self, name: &str, oid: Oid, bootstrap: bool, shared: bool, rowtype_oid: Oid, columns: &[Column]) -> PgResult<()> {
        if self.reldesc.is_some() {
            let _ = elog::elog(DEBUG4, "create: warning, open relation exists, closing first");
            self.closerel(None)?;
        }
        self.attrs.clear();
        for (i, col) in columns.iter().enumerate() {
            self.define_attr(i, col)?;
        }
        self.attrs.truncate(columns.len());
        let _ = elog::elog(
            DEBUG4,
            format!(
                "creating{}{} relation {name} {oid}",
                if bootstrap { " bootstrap" } else { "" },
                if shared { " shared" } else { "" }
            ),
        );
        let tupdesc = tupdesc::CreateTupleDesc(self.mcx, &self.attrs)?;
        let mapped = bootstrap || shared;
        let tablespace = if shared { GLOBALTABLESPACE_OID } else { 0 };
        if bootstrap {
            let (rel, _relfrozenxid, _relminmxid) = catalog_heap::heap_create(
                self.mcx,
                name.as_bytes(),
                PG_CATALOG_NAMESPACE,
                tablespace,
                oid,
                InvalidOid,
                types_core::InvalidRelFileNumber,
                HEAP_TABLE_AM_OID,
                &tupdesc,
                RELKIND_RELATION,
                RELPERSISTENCE_PERMANENT,
                shared,
                mapped,
                true,
                true,
            )?;
            self.reldesc = Some(Relation::open_rc(rel, None));
            let _ = elog::elog(DEBUG4, "bootstrap relation created");
        } else {
            let id = catalog_heap::heap_create_with_catalog(
                self.mcx,
                &catalog_heap::HeapCreateParams {
                    relname: name,
                    relid: oid,
                    reltypeid: rowtype_oid,
                    relnamespace: PG_CATALOG_NAMESPACE,
                    reltablespace: tablespace,
                    ownerid: BOOTSTRAP_SUPERUSERID,
                    accessmtd: HEAP_TABLE_AM_OID,
                    relkind: RELKIND_RELATION,
                    relpersistence: RELPERSISTENCE_PERMANENT,
                    reloftype: InvalidOid,
                    shared,
                    mapped,
                    allow_system_table_mods: true,
                    reloptions: None,
                    relrewrite: InvalidOid,
                    use_user_acl: false,
                    is_internal: true,
                },
                &tupdesc,
            )?;
            let _ = elog::elog(DEBUG4, format!("relation created with OID {id}"));
        }
        Ok(())
    }


    fn insert(&mut self, vals: &[Option<String>]) -> PgResult<()> {
        let numattr = self.attrs.len();
        if vals.len() != numattr {
            return Err(err(format!(
                "incorrect number of columns in row (expected {numattr}, got {})",
                vals.len()
            )));
        }
        let Some(rel) = self.reldesc.as_ref() else {
            return Err(Box::new(PgError::new(types_error::FATAL, "relation not open")));
        };
        self.values.clear();
        self.nulls.clear();
        let desc = rel.descr();
        for (i, v) in vals.iter().enumerate() {
            let att = desc.attr(i);
            match v {
                None => {
                    if att.attnotnull {
                        return Err(err(format!(
                            "NULL value specified for not-null column \"{}\" of relation \"{}\"",
                            String::from_utf8_lossy(att.attname.name_str()),
                            rel.name()
                        )));
                    }
                    self.values.push(Datum::null());
                    self.nulls.push(true);
                }
                Some(text) => {
                    let io = boot_get_type_io_data(att.atttypid)?;
                    let flinfo = match self.inputs.entry(io.typinput) {
                        std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                        std::collections::btree_map::Entry::Vacant(v) => v.insert(fmgr_seams::fmgr_info::call(io.typinput)?),
                    };
                    let cstr = CString::new(text.as_bytes())
                        .map_err(|_| err("embedded NUL in BKI value"))?;
                    let d = types_fmgr::input_function_call(flinfo, Some(cstr.as_c_str()), io.typioparam, -1, self.mcx)?;
                    // C's input functions palloc their results in the caller's
                    // context; pgrust's may hand back FmgrInfo-scratch pointers
                    // (namein), so every by-reference value is copied into the
                    // interpreter context before the tuple is formed.
                    let d = if io.typbyval { d } else { copy_byref_datum(self.mcx, d, io.typlen)? };
                    self.values.push(d);
                    self.nulls.push(false);
                }
            }
        }
        let tupdesc = tupdesc::CreateTupleDesc(self.mcx, &self.attrs)?;
        let mut tuple = heaptuple::heap_form_tuple(self.mcx, &tupdesc, &self.values, &self.nulls)?;
        heapam::simple_heap_insert(rel, tuple.as_tuple_mut())?;
        Ok(())
    }


    fn populate_typ_list(&mut self) -> PgResult<()> {
        let mcx = self.mcx;
        let rel = table::table_open(mcx, types_core::TYPE_RELATION_ID, NoLock)?;
        let desc = rel.descr();
        let mut list = Vec::new();
        let mut scan = genam::systable_beginscan(mcx, &rel, InvalidOid, false, None, &[])?;
        while let Some(tup) = genam::systable_getnext(mcx, &mut scan)? {
            let att = |n: i32| -> Datum {
                let mut isnull = false;
                // SAFETY: a pg_type row under pg_type's descriptor; these
                // columns are fixed-width NOT NULL.
                unsafe { types_tuple::heap_getattr(tup, n, desc, &mut isnull) }
            };
            let name_d = att(ANUM_PG_TYPE_TYPNAME);
            // SAFETY: a NameData column: NAMEDATALEN NUL-padded bytes.
            let bytes = unsafe {
                core::slice::from_raw_parts(name_d.as_usize() as *const u8, types_core::fmgr::NAMEDATALEN as usize)
            };
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            list.push(TypMap {
                oid: att(ANUM_PG_TYPE_OID).as_oid(),
                name: String::from_utf8_lossy(&bytes[..end]).into_owned(),
                len: att(ANUM_PG_TYPE_TYPLEN).as_i16(),
                byval: att(ANUM_PG_TYPE_TYPBYVAL).as_bool(),
                align: att(ANUM_PG_TYPE_TYPALIGN).as_char(),
                storage: att(ANUM_PG_TYPE_TYPSTORAGE).as_char(),
                collation: att(ANUM_PG_TYPE_TYPCOLLATION).as_oid(),
                elem: att(ANUM_PG_TYPE_TYPELEM).as_oid(),
                delim: att(ANUM_PG_TYPE_TYPDELIM).as_char(),
                input: att(ANUM_PG_TYPE_TYPINPUT).as_oid(),
                output: att(ANUM_PG_TYPE_TYPOUTPUT).as_oid(),
            });
        }
        genam::systable_endscan(mcx, scan)?;
        rel.close(NoLock)?;
        *TYP.lock().unwrap_or_else(|e| e.into_inner()) = Some(list);
        Ok(())
    }

    fn gettype(&mut self, name: &str) -> PgResult<(Oid, i16, bool, i8, i8, Oid, Oid)> {
        let from_list = |list: &Vec<TypMap>| {
            list.iter().find(|t| t.name == name).map(|t| (t.oid, t.len, t.byval, t.align, t.storage, t.collation, t.elem))
        };
        if typ_lookup(|t| t.is_some()) {
            if let Some(r) = typ_lookup(|t| t.and_then(from_list)) {
                return Ok(r);
            }
            self.populate_typ_list()?;
            return typ_lookup(|t| t.and_then(from_list)).ok_or_else(|| err(format!("unrecognized type \"{name}\"")));
        }
        if let Some(t) = TYP_INFO.iter().find(|t| t.name == name) {
            return Ok((t.oid, t.len, t.byval, t.align, t.storage, t.collation, t.elem));
        }
        let _ = elog::elog(DEBUG4, format!("external type: {name}"));
        self.populate_typ_list()?;
        self.gettype(name)
    }


    fn declare_index(&mut self, unique: bool, name: &str, oid: Oid, on: &str, using: &str, params: &[(String, String)]) -> PgResult<()> {
        let mcx = self.mcx;
        let _ = elog::elog(DEBUG4, format!("creating {}index \"{name}\"", if unique { "unique " } else { "" }));
        let mut index_params = NodeList::nil();
        for (col, opclass) in params {
            let mut opclass_list = NodeList::with_capacity(mcx, 1)?;
            opclass_list.lappend(mcx, Node::mk_string(mcx, str_in(mcx, opclass)?)?)?;
            let elem = IndexElem {
                name: Some(str_in(mcx, col)?),
                expr: None,
                indexcolname: None,
                collation: NodeList::nil(),
                opclass: opclass_list,
                opclassopts: NodeList::nil(),
                ordering: SortByDir::SORTBY_DEFAULT,
                nulls_ordering: SortByNulls::SORTBY_NULLS_DEFAULT,
            };
            index_params.lappend(mcx, Node::mk(mcx, elem)?)?;
        }
        let rv = types_nodes::primnodes::RangeVar {
            catalogname: None,
            schemaname: None,
            relname: Some(str_in(mcx, on)?),
            inh: true,
            relpersistence: RELPERSISTENCE_PERMANENT,
            alias: None,
            location: -1,
        };
        let rv_ref = Node::mk(mcx, rv)?.as_variant::<types_nodes::primnodes::RangeVar>().expect("fresh RangeVar node");
        let stmt = IndexStmt {
            idxname: Some(str_in(mcx, name)?),
            relation: Some(rv_ref),
            accessMethod: Some(str_in(mcx, using)?),
            tableSpace: None,
            indexParams: index_params,
            indexIncludingParams: NodeList::nil(),
            options: NodeList::nil(),
            whereClause: None,
            excludeOpNames: NodeList::nil(),
            idxcomment: None,
            indexOid: InvalidOid,
            oldNumber: types_core::InvalidRelFileNumber,
            oldCreateSubid: types_core::InvalidSubTransactionId,
            oldFirstRelfilelocatorSubid: types_core::InvalidSubTransactionId,
            unique,
            nulls_not_distinct: false,
            primary: false,
            isconstraint: false,
            iswithoutoverlaps: false,
            deferrable: false,
            initdeferred: false,
            transformed: false,
            concurrent: false,
            if_not_exists: false,
            reset_default_tblspc: false,
        };
        let relation_id = catalog_namespace::RangeVarGetRelid(&range_var(on), NoLock, false)?;
        indexcmds::DefineIndex(mcx, relation_id, &stmt, oid, InvalidOid, InvalidOid, false, false, false, true, false)?;
        Ok(())
    }

    fn build_indices(&mut self) -> PgResult<()> {
        let mcx = self.mcx;
        for (heap_oid, index_oid) in catalog_index::take_bootstrap_indexes().into_iter().rev() {
            let heap = table::table_open(mcx, heap_oid, NoLock)?;
            let ind = indexam::index_open(mcx, index_oid, NoLock)?;
            let mut info = execindexing::BuildIndexInfo(mcx, &ind)?;
            catalog_index::index_build(mcx, &heap, &ind, &mut info, false, false)?;
            ind.close(NoLock)?;
            heap.close(NoLock)?;
        }
        Ok(())
    }
}
