// pl_comp.c + the pl_funcs.c namespace stack, phase-1 subset.
// Unported louds (named at their sites): %ROWTYPE, cword %TYPE beyond
// var/2-3-part type names, record %TYPE, composite- and domain-typed
// variables, trigger compile, polymorphic argument resolution,
// VARIADIC arg mode.
use types_core::{Oid, OidIsValid};
use types_error::{
    PgError, PgResult, ERRCODE_FEATURE_NOT_SUPPORTED, ERRCODE_UNDEFINED_OBJECT, ERROR,
};

use crate::ast::*;
use crate::scanner::{CwordRes, IdentifierLookup, PLcword, PLwdatum, PLword, WordRes, WordResolver};

// plpgsql.h PLPGSQL_RESOLVE_*; the plpgsql.variable_conflict GUC row
// (guc_tables) stores the same values.
pub const PLPGSQL_RESOLVE_ERROR: i32 = guc_tables::consts::PLPGSQL_RESOLVE_ERROR;
pub const PLPGSQL_RESOLVE_VARIABLE: i32 = guc_tables::consts::PLPGSQL_RESOLVE_VARIABLE;
pub const PLPGSQL_RESOLVE_COLUMN: i32 = guc_tables::consts::PLPGSQL_RESOLVE_COLUMN;

const TYPTYPE_BASE: i8 = b'b' as i8;
const TYPTYPE_COMPOSITE: i8 = b'c' as i8;
const TYPSTORAGE_PLAIN: i8 = b'p' as i8;
const TYPTYPE_DOMAIN: i8 = b'd' as i8;
const TYPTYPE_PSEUDO: i8 = b'p' as i8;

// Expression identity (C: the PLpgSQL_expr pointer, freed with the function's
// memory context). Ids released by a dead function are reissued.
struct ExprIdPool {
    next: u32,
    free: Vec<u32>,
}

std::thread_local! {
    static EXPR_IDS: core::cell::RefCell<ExprIdPool> =
        const { core::cell::RefCell::new(ExprIdPool { next: 1, free: Vec::new() }) };
}

// plpgsql_free_function_memory: the ids are reusable once no plan-table
// entry keys them (exec.rs free_function_plans).
pub(crate) fn alloc_expr_id() -> u32 {
    EXPR_IDS.with(|p| {
        let mut p = p.borrow_mut();
        p.free.pop().unwrap_or_else(|| {
            let v = p.next;
            p.next = v.checked_add(1).expect("plpgsql expression ids exhausted");
            v
        })
    })
}

pub fn release_expr_ids(ids: &[u32]) {
    EXPR_IDS.with(|p| p.borrow_mut().free.extend_from_slice(ids));
}

/// D4312: the thread's exit drain (exec.rs release_plpgsql_state_at_exit) dropped everything an id keys, so the pool
/// starts over: a retained standby's next task reissues ids from 1 instead of counting on toward the plan slab's bound.
pub(crate) fn reset_expr_ids() {
    EXPR_IDS.with(|p| *p.borrow_mut() = ExprIdPool { next: 1, free: Vec::new() });
}

impl Drop for CompState {
    fn drop(&mut self) {
        if !self.expr_ids.is_empty() {
            release_expr_ids(&self.expr_ids);
        }
    }
}

// PLPGSQL_XCHECK_* (plpgsql.h:1194-1199).
pub const XCHECK_SHADOWVAR: u32 = 1 << 1;
pub const XCHECK_TOOMANYROWS: u32 = 1 << 2;
pub const XCHECK_STRICTMULTIASSIGNMENT: u32 = 1 << 3;
pub const XCHECK_ALL: u32 = u32::MAX;

pub struct CompState {
    pub datums: Vec<PlDatum>,
    pub ns: Vec<NsItem>,
    pub ns_top: i32,
    pub identifier_lookup: IdentifierLookup,
    pub expr_ids: Vec<u32>,
    pub nstatements: u32,
    pub resolve_option: i32,
    pub print_strict_params: bool,
    pub dump_exec_tree: bool,
    // Promoted from the GUCs only at CREATE FUNCTION time (pl_comp.c:249-250);
    // inline blocks and non-validator recompiles keep 0 (pl_comp.c:796-797).
    pub extra_warnings: u32,
    pub extra_errors: u32,
    pub out_param_varno: Dno,
    datums_last: usize,
}

impl CompState {
    pub fn new() -> CompState {
        CompState {
            datums: Vec::new(),
            ns: Vec::new(),
            ns_top: -1,
            identifier_lookup: IdentifierLookup::Normal,
            expr_ids: Vec::new(),
            nstatements: 0,
            resolve_option: PLPGSQL_RESOLVE_ERROR,
            print_strict_params: false,
            dump_exec_tree: false,
            extra_warnings: 0,
            extra_errors: 0,
            out_param_varno: -1,
            datums_last: 0,
        }
    }

    // plpgsql_add_initdatums (pl_comp.c): datums added since the last call.
    pub fn add_initdatums(&mut self) -> Vec<Dno> {
        let mut out = Vec::new();
        for i in self.datums_last..self.datums.len() {
            if matches!(self.datums[i], PlDatum::Var(_) | PlDatum::Rec(_)) {
                out.push(i as Dno);
            }
        }
        self.datums_last = self.datums.len();
        out
    }

    // plpgsql_ns_push with the label-kind marker in itemno.
    pub fn ns_push_label(&mut self, label: Option<&str>, label_kind: i32) {
        self.ns.push(NsItem {
            itemtype: NsType::Label,
            itemno: label_kind,
            name: label.unwrap_or("").to_string(),
            prev: self.ns_top,
        });
        self.ns_top = self.ns.len() as i32 - 1;
    }

    // plpgsql_ns_find_nearest_loop.
    pub fn ns_has_loop_label(&self, mut cur: i32) -> bool {
        while cur >= 0 {
            let item = &self.ns[cur as usize];
            if item.itemtype == NsType::Label && item.itemno == crate::gram::LABEL_LOOP {
                return true;
            }
            cur = item.prev;
        }
        false
    }

    pub fn new_expr_id(&mut self) -> u32 {
        let id = alloc_expr_id();
        self.expr_ids.push(id);
        id
    }

    // plpgsql_ns_pop.
    pub fn ns_pop(&mut self) {
        let mut cur = self.ns_top;
        while cur >= 0 {
            let item = &self.ns[cur as usize];
            let prev = item.prev;
            if item.itemtype == NsType::Label {
                self.ns_top = prev;
                return;
            }
            cur = prev;
        }
        panic!("ns_pop: stack underflow");
    }

    // plpgsql_ns_additem.
    pub fn ns_additem(&mut self, itemtype: NsType, itemno: i32, name: &str) {
        self.ns.push(NsItem {
            itemtype,
            itemno,
            name: name.to_string(),
            prev: self.ns_top,
        });
        self.ns_top = self.ns.len() as i32 - 1;
    }

    // plpgsql_ns_lookup; returns (ns index, names_used).
    pub fn ns_lookup(
        &self,
        mut cur: i32,
        localmode: bool,
        name1: &str,
        name2: Option<&str>,
        name3: Option<&str>,
    ) -> Option<(i32, i32)> {
        while cur >= 0 {
            let mut i = cur;
            loop {
                let item = &self.ns[i as usize];
                if item.itemtype == NsType::Label {
                    break;
                }
                if item.name == name1
                    && (name2.is_none() || item.itemtype != NsType::Var)
                {
                    return Some((i, 1));
                }
                i = item.prev;
            }
            let label = &self.ns[i as usize];
            if let Some(n2) = name2 {
                if label.name == name1 {
                    let mut j = cur;
                    loop {
                        let item = &self.ns[j as usize];
                        if item.itemtype == NsType::Label {
                            break;
                        }
                        if item.name == n2
                            && (name3.is_none() || item.itemtype != NsType::Var)
                        {
                            return Some((j, 2));
                        }
                        j = item.prev;
                    }
                }
            }
            if localmode {
                break;
            }
            cur = label.prev;
        }
        None
    }

    // plpgsql_ns_lookup_label.
    pub fn ns_lookup_label(&self, mut cur: i32, name: &str) -> Option<i32> {
        while cur >= 0 {
            let item = &self.ns[cur as usize];
            if item.itemtype == NsType::Label && item.name == name {
                return Some(cur);
            }
            cur = item.prev;
        }
        None
    }

    // plpgsql_build_datatype (via lsyscache instead of an open pg_type tuple).
    pub fn build_datatype(
        typoid: Oid,
        typmod: i32,
        collation: Oid,
        origtypname: Option<OrigTypeName>,
    ) -> PgResult<PlType> {
        let (typlen, typbyval) = lsyscache::typ::get_typlenbyval(typoid)?;
        let typtype = lsyscache::typ::get_typtype(typoid)?;
        let typcollation = lsyscache::typ::get_typcollation(typoid)?;
        // pl_comp.c:2021-2023: caller collation only overrides a collatable type.
        let coll = if OidIsValid(collation) && OidIsValid(typcollation) {
            collation
        } else {
            typcollation
        };
        let (typinput, typioparam) = lsyscache::typ::getTypeInputInfo(typoid)?;
        // pl_comp.c:2024-2042: a true, toastable array (int2vector and
        // oidvector are plain-storage), or a varlena domain over one.
        let typisarray = if typtype == TYPTYPE_BASE {
            OidIsValid(lsyscache::typ::get_element_type(typoid)?)
                && lsyscache::typ::get_typstorage(typoid)? != TYPSTORAGE_PLAIN
        } else if typtype == TYPTYPE_DOMAIN {
            typlen == -1
                && lsyscache::typ::get_typstorage(typoid)? != TYPSTORAGE_PLAIN
                && OidIsValid(lsyscache::typ::get_base_element_type(typoid)?)
        } else {
            false
        };
        const RECORDOID: Oid = 2249;
        // pl_comp.c:1990-2016 typtype switch: composite (and RECORD) are
        // TTYPE_REC; a domain is TTYPE_REC iff its base type is a rowtype
        // (type_is_rowtype on typbasetype), else scalar.
        let ttype = if typoid == RECORDOID || typtype == TYPTYPE_COMPOSITE {
            TypeKind::Rec
        } else if typtype == TYPTYPE_DOMAIN
            && lsyscache::typ::type_is_rowtype(lsyscache::typ::getBaseType(typoid)?)?
        {
            TypeKind::Rec
        } else if typtype == TYPTYPE_PSEUDO {
            TypeKind::Pseudo
        } else {
            TypeKind::Scalar
        };
        let rec_ident = if ttype == TypeKind::Rec && typoid != RECORDOID {
            let tcache = composite_typcache_entry(typoid)?;
            Some(std::rc::Rc::new(RecTypeIdent {
                origtypname,
                typoid: std::cell::Cell::new(typoid),
                tupdesc_id: std::cell::Cell::new(tcache.tupdesc_identifier()),
                tcache: std::cell::RefCell::new(std::rc::Rc::downgrade(&tcache)),
            }))
        } else {
            None
        };
        Ok(PlType {
            typoid,
            ttype,
            typlen,
            typbyval,
            typtype,
            collation: coll,
            typisarray,
            atttypmod: typmod,
            typinput,
            typioparam,
            rec_ident,
        })
    }

    // plpgsql_build_variable, scalar arm; Row/Rec arms are separate builders.
    pub fn build_variable(
        &mut self,
        refname: &str,
        lineno: i32,
        datatype: PlType,
        add2namespace: bool,
    ) -> PgResult<Dno> {
        if datatype.ttype == TypeKind::Rec {
            let rectypeid = datatype.typoid;
            return Ok(self.build_rec_typed(refname, lineno, rectypeid, Some(datatype), add2namespace));
        }
        if datatype.ttype == TypeKind::Pseudo {
            return Err(comp_err(
                ERRCODE_FEATURE_NOT_SUPPORTED,
                format!(
                    "variable \"{refname}\" has pseudo-type {}",
                    format_type::format_type_be(datatype.typoid)?
                ),
            ));
        }
        let dno = self.datums.len() as Dno;
        self.datums.push(PlDatum::Var(PlVar {
            dno,
            refname: refname.to_string(),
            lineno,
            datatype,
            isconst: false,
            notnull: false,
            default_val: None,
            promise: PROMISE_NONE,
            cursor_explicit_expr: None,
            cursor_explicit_argrow: -1,
            cursor_options: 0,
        }));
        if add2namespace {
            self.ns_additem(NsType::Var, dno, refname);
        }
        Ok(dno)
    }

    pub fn build_rec(&mut self, refname: &str, lineno: i32, add2namespace: bool) -> Dno {
        const RECORDOID: Oid = 2249;
        self.build_rec_typed(refname, lineno, RECORDOID, None, add2namespace)
    }

    // plpgsql_build_record.
    pub fn build_rec_typed(
        &mut self,
        refname: &str,
        lineno: i32,
        rectypeid: Oid,
        datatype: Option<PlType>,
        add2namespace: bool,
    ) -> Dno {
        let dno = self.datums.len() as Dno;
        self.datums.push(PlDatum::Rec(PlRec {
            dno,
            refname: refname.to_string(),
            lineno,
            rectypeid,
            datatype,
            isconst: false,
            notnull: false,
            default_val: None,
        }));
        if add2namespace {
            self.ns_additem(NsType::Rec, dno, refname);
        }
        dno
    }

    pub fn build_row(&mut self, refname: &str, lineno: i32, varnos: Vec<Dno>) -> Dno {
        let fieldnames = varnos
            .iter()
            .map(|&v| self.datums[v as usize].refname().to_string())
            .collect();
        self.build_row_named(refname, lineno, varnos, fieldnames)
    }

    // A scalar INTO/FOR target list: fields named as written (pl_gram.y
    // NameOfDatum), which may be an alias or a qualified name.
    pub fn build_row_named(
        &mut self,
        refname: &str,
        lineno: i32,
        varnos: Vec<Dno>,
        fieldnames: Vec<String>,
    ) -> Dno {
        let dno = self.datums.len() as Dno;
        self.datums.push(PlDatum::Row(PlRow {
            dno,
            refname: refname.to_string(),
            lineno,
            fieldnames,
            varnos,
        }));
        dno
    }

    // plpgsql_build_recfield: reuse an existing recfield for the same name.
    pub fn build_recfield(&mut self, recno: Dno, fieldname: &str) -> Dno {
        for d in &self.datums {
            if let PlDatum::RecField(f) = d {
                if f.recparentno == recno && f.fieldname == fieldname {
                    return f.dno;
                }
            }
        }
        let dno = self.datums.len() as Dno;
        self.datums.push(PlDatum::RecField(PlRecField {
            dno,
            recparentno: recno,
            fieldname: fieldname.to_string(),
        }));
        dno
    }

    // plpgsql_parse_wordtype (pl_comp.c:1514, %TYPE on a bare variable
    // name): a VAR answers with its datatype, a REC with rec->datatype —
    // None for a record built without a PLpgSQL_type (trigger NEW/OLD), in
    // which case C returns NULL and read_datatype falls through to the
    // plain type-name path. Only an unknown name is an error.
    pub fn parse_wordtype(&self, ident: &str) -> PgResult<Option<PlType>> {
        if let Some((idx, _)) = self.ns_lookup(self.ns_top, false, ident, None, None) {
            let item = &self.ns[idx as usize];
            if item.itemtype == NsType::Var {
                if let PlDatum::Var(v) = &self.datums[item.itemno as usize] {
                    return Ok(Some(v.datatype.clone()));
                }
            }
            if item.itemtype == NsType::Rec {
                if let PlDatum::Rec(r) = &self.datums[item.itemno as usize] {
                    return Ok(r.datatype.clone());
                }
            }
        }
        Err(comp_err(
            ERRCODE_UNDEFINED_OBJECT,
            format!("variable \"{ident}\" does not exist"),
        ))
    }

    // plpgsql_parse_wordrowtype (pl_comp.c:1667).
    pub fn parse_wordrowtype(&self, ident: &str) -> PgResult<PlType> {
        let class_oid = catalog_namespace::RelnameGetRelid(ident)?;
        if !OidIsValid(class_oid) {
            return Err(comp_err(
                types_error::ERRCODE_UNDEFINED_TABLE,
                format!("relation \"{ident}\" does not exist"),
            ));
        }
        Self::rowtype_of(class_oid, ident, OrigTypeName::Names(vec![ident.to_string()]))
    }

    // plpgsql_parse_cwordrowtype (pl_comp.c:1704): the RangeVar is built as
    // makeRangeVarFromNameList does, so RangeVarGetRelid sees a catalog name.
    pub fn parse_cwordrowtype(&self, idents: &[String]) -> PgResult<PlType> {
        let (catalogname, schemaname, relname) = match idents {
            [s, r] => (None, Some(s.as_str()), r.as_str()),
            [c, s, r] => (Some(c.as_str()), Some(s.as_str()), r.as_str()),
            _ => {
                return Err(comp_err(
                    types_error::ERRCODE_SYNTAX_ERROR,
                    format!(
                        "improper relation name (too many dotted names): {}",
                        idents.join(".")
                    ),
                ));
            }
        };
        let rv = rel_vocab::RangeVar {
            catalogname,
            schemaname,
            relname,
            inh: true,
            relpersistence: b'p',
            location: -1,
        };
        let class_oid = catalog_namespace::RangeVarGetRelid(&rv, types_rel::NoLock, false)?;
        Self::rowtype_of(class_oid, relname, OrigTypeName::Names(::mcx::std_vec_from_slice("plpgsql", &idents)?))
    }

    fn rowtype_of(class_oid: Oid, relname: &str, origtypname: OrigTypeName) -> PgResult<PlType> {
        let typoid = lsyscache::relation::get_rel_type_id(class_oid)?;
        if !OidIsValid(typoid) {
            return Err(comp_err(
                types_error::ERRCODE_WRONG_OBJECT_TYPE,
                format!("relation \"{relname}\" does not have a composite type"),
            ));
        }
        Self::build_datatype(typoid, -1, types_core::InvalidOid, Some(origtypname))
    }

    // plpgsql_parse_cwordtype: block-qualified var %TYPE, else table.column
    // (or schema.table.column) %TYPE from pg_attribute.
    pub fn parse_cwordtype(&self, idents: &[String]) -> PgResult<Option<PlType>> {
        let (schemaname, relname, fldname) = match idents.len() {
            2 => {
                if let Some((idx, nnames)) =
                    self.ns_lookup(self.ns_top, false, &idents[0], Some(&idents[1]), None)
                {
                    let item = &self.ns[idx as usize];
                    if item.itemtype == NsType::Var {
                        if let PlDatum::Var(v) = &self.datums[item.itemno as usize] {
                            return Ok(Some(v.datatype.clone()));
                        }
                    }
                    // pl_comp.c:1610: a block-qualified record's datatype,
                    // NULL for a datatype-less rec (trigger NEW/OLD) so the
                    // caller falls through to plain type-name parsing.
                    if item.itemtype == NsType::Rec && nnames == 2 {
                        if let PlDatum::Rec(r) = &self.datums[item.itemno as usize] {
                            return Ok(r.datatype.clone());
                        }
                    }
                }
                (None, idents[0].as_str(), idents[1].as_str())
            }
            3 => (Some(idents[0].as_str()), idents[1].as_str(), idents[2].as_str()),
            _ => {
                return Err(comp_err(
                    types_error::ERRCODE_SYNTAX_ERROR,
                    format!("improper qualified name (too many dotted names): {}", idents.join(".")),
                ));
            }
        };
        let rv = rel_vocab::RangeVar {
            catalogname: None,
            schemaname,
            relname,
            inh: true,
            relpersistence: b'p',
            location: -1,
        };
        let class_oid = catalog_namespace::RangeVarGetRelid(&rv, types_rel::NoLock, false)?;
        // SearchSysCacheAttName excludes dropped columns (pl_comp.c:1626).
        const ANUM_PG_ATTRIBUTE_ATTNUM: i32 = 5;
        let Some(atttup) = cache_syscache::SearchSysCacheAttName(class_oid, fldname)? else {
            return Err(comp_err(
                types_error::ERRCODE_UNDEFINED_COLUMN,
                format!("column \"{fldname}\" of relation \"{relname}\" does not exist"),
            ));
        };
        let attnum = cache_syscache::SysCacheGetAttrNotNull(
            cache_syscache::ATTNAME,
            &atttup,
            ANUM_PG_ATTRIBUTE_ATTNUM,
        )?
        .as_i16();
        cache_syscache::ReleaseSysCache(atttup);
        // C holds the pg_attribute tuple SearchSysCacheAttName just returned,
        // so this re-probe is a seam artifact; the C frame's own miss arm is
        // `elog(ERROR, "cache lookup failed for type %u")` (pl_comp.c:1637),
        // i.e. a catchable XX000, never an abort.  Keep the standard
        // attribute-lookup wording and the same catchable level.
        let shape = syscache_seams::lookup_pg_attribute_shape::call(class_oid, attnum)?
            .ok_or_else(|| {
                comp_err(
                    types_error::ERRCODE_INTERNAL_ERROR,
                    format!("cache lookup failed for attribute {attnum} of relation {class_oid}"),
                )
            })?;
        Self::build_datatype(shape.atttypid, shape.atttypmod, shape.attcollation, None).map(Some)
    }
}

impl Default for CompState {
    fn default() -> Self {
        Self::new()
    }
}

#[cold]
// A domain over a composite resolves to its base type's entry.
pub(crate) fn composite_typcache_entry(typoid: Oid) -> PgResult<std::rc::Rc<typcache::TypeCacheEntry>> {
    let mut entry = typcache::lookup_type_cache(
        typoid,
        typcache::TYPECACHE_TUPDESC | typcache::TYPECACHE_DOMAIN_BASE_INFO,
    )?;
    let base = entry.domain_base_type();
    if OidIsValid(base) {
        entry = typcache::lookup_type_cache(base, typcache::TYPECACHE_TUPDESC)?;
    }
    if entry.tupdesc().is_none() {
        return Err(comp_err(
            types_error::ERRCODE_WRONG_OBJECT_TYPE,
            format!("type {} is not composite", format_type::format_type_be(typoid)?),
        ));
    }
    Ok(entry)
}

pub fn comp_err(code: types_error::SqlState, msg: String) -> Box<PgError> {
    Box::new(
        elog::ereport(ERROR)
            .errcode(code)
            .errmsg(msg)
            .into_error(),
    )
}

impl WordResolver for CompState {
    fn parse_word(&mut self, word1: &str, yytxt: &str, lookup: bool) -> PgResult<WordRes> {
        if lookup && self.identifier_lookup == IdentifierLookup::Normal {
            if let Some((idx, _)) = self.ns_lookup(self.ns_top, false, word1, None, None) {
                let item = &self.ns[idx as usize];
                match item.itemtype {
                    NsType::Var | NsType::Rec => {
                        return Ok(WordRes::Datum(PLwdatum {
                            dno: item.itemno,
                            ident: word1.to_string(),
                            quoted: yytxt.starts_with('"'),
                            idents: Vec::new(),
                        }));
                    }
                    _ => {}
                }
            }
        }
        Ok(WordRes::Word(PLword {
            ident: word1.to_string(),
            quoted: yytxt.starts_with('"'),
        }))
    }

    fn parse_dblword(&mut self, word1: &str, word2: &str) -> PgResult<CwordRes> {
        let idents = vec![word1.to_string(), word2.to_string()];
        if self.identifier_lookup != IdentifierLookup::Declare {
            if let Some((idx, nnames)) = self.ns_lookup(self.ns_top, false, word1, Some(word2), None)
            {
                let item = &self.ns[idx as usize];
                match item.itemtype {
                    NsType::Var => {
                        return Ok(CwordRes::Datum(PLwdatum {
                            dno: item.itemno,
                            ident: String::new(),
                            quoted: false,
                            idents,
                        }));
                    }
                    NsType::Rec => {
                        let dno = if nnames == 1 {
                            let recno = item.itemno;
                            self.build_recfield(recno, word2)
                        } else {
                            item.itemno
                        };
                        return Ok(CwordRes::Datum(PLwdatum {
                            dno,
                            ident: String::new(),
                            quoted: false,
                            idents,
                        }));
                    }
                    _ => {}
                }
            }
        }
        Ok(CwordRes::Cword(PLcword { idents }))
    }

    fn parse_tripword(&mut self, word1: &str, word2: &str, word3: &str) -> PgResult<CwordRes> {
        if self.identifier_lookup != IdentifierLookup::Declare {
            if let Some((idx, nnames)) =
                self.ns_lookup(self.ns_top, false, word1, Some(word2), Some(word3))
            {
                let item = &self.ns[idx as usize];
                if item.itemtype == NsType::Rec {
                    let recno = item.itemno;
                    // pl_comp.c:1463-1475: nnames==1 is rec.field.subfield —
                    // RECFIELD on word2, only two idents claimed; nnames==2 is
                    // label.rec.field — RECFIELD on word3.
                    let (dno, idents) = if nnames == 1 {
                        (
                            self.build_recfield(recno, word2),
                            vec![word1.to_string(), word2.to_string()],
                        )
                    } else {
                        (
                            self.build_recfield(recno, word3),
                            vec![word1.to_string(), word2.to_string(), word3.to_string()],
                        )
                    };
                    return Ok(CwordRes::Datum(PLwdatum {
                        dno,
                        ident: String::new(),
                        quoted: false,
                        idents,
                    }));
                }
            }
        }
        Ok(CwordRes::Cword(PLcword {
            idents: vec![word1.to_string(), word2.to_string(), word3.to_string()],
        }))
    }

    fn identifier_lookup(&self) -> IdentifierLookup {
        self.identifier_lookup
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // plpgsql_misc: a three-part %ROWTYPE name is RangeVarGetRelid's
    // cross-database error, not a local 42601.
    #[test]
    fn three_part_rowtype_name_is_a_cross_database_reference() {
        dbcommands_seams::get_database_name::set(|_| Ok(Some("testdb".to_string())));
        let comp = CompState::new();
        let idents = ["foo".to_string(), "bar".to_string(), "baz".to_string()];
        let err = comp.parse_cwordrowtype(&idents).unwrap_err();
        assert_eq!(err.sqlstate, types_error::ERRCODE_FEATURE_NOT_SUPPORTED);
        assert_eq!(
            err.message,
            "cross-database references are not implemented: \"foo.bar.baz\""
        );
    }

    // pl_comp.c:1514 plpgsql_parse_wordtype: a REC namespace entry answers
    // with rec->datatype, which is NULL for a record built without a
    // PLpgSQL_type (the trigger NEW/OLD records, plpgsql_build_record(...,
    // NULL, RECORDOID, ...)); read_datatype then falls through to the plain
    // type-name path ("NEW%TYPE" -> the core parser's syntax error). Only a
    // name that is not a variable at all is "variable does not exist".
    #[test]
    fn wordtype_on_a_datatype_less_record_is_not_an_undefined_variable() {
        let mut comp = CompState::new();
        comp.ns_push_label(Some("trg"), crate::gram::LABEL_BLOCK);
        comp.build_rec("new", 0, true);
        let r = comp.parse_wordtype("new");
        assert!(
            r.is_ok(),
            "C returns NULL (no error) for a datatype-less record: {:?}",
            r.err().map(|e| (e.sqlstate, e.message))
        );
        assert!(matches!(r, Ok(None)), "NULL datatype = no %TYPE resolution");
        let err = comp.parse_wordtype("nosuchvar").unwrap_err();
        assert_eq!(err.sqlstate, ERRCODE_UNDEFINED_OBJECT);
        assert_eq!(err.message, "variable \"nosuchvar\" does not exist");
    }

    // pl_funcs.c:716 plpgsql_free_function_memory: an expression's identity
    // dies with its function, so a backend compiling DO blocks forever never
    // runs out of ids; an aborted compile releases its ids the same way.
    #[test]
    fn expression_ids_of_freed_functions_are_reissued() {
        let mut comp = CompState::new();
        let first: Vec<u32> = (0..3).map(|_| comp.new_expr_id()).collect();
        let ids = std::mem::take(&mut comp.expr_ids);
        drop(comp);
        crate::exec::free_function_plans(&ids);
        let mut comp = CompState::new();
        let mut again: Vec<u32> = (0..3).map(|_| comp.new_expr_id()).collect();
        again.sort_unstable();
        assert_eq!(again, first);
        let fresh = comp.new_expr_id();
        assert!(!first.contains(&fresh));
        drop(comp);
        let mut comp = CompState::new();
        let mut after_abort: Vec<u32> = (0..4).map(|_| comp.new_expr_id()).collect();
        after_abort.sort_unstable();
        let mut expected = first.clone();
        expected.push(fresh);
        assert_eq!(after_abort, expected);
        let live: Vec<u32> = (0..2).map(|_| comp.new_expr_id()).collect();
        assert!(live.iter().all(|id| !after_abort.contains(id)));
    }
}

// plpgsql_mark_local_assignment_targets (pl_funcs.c): only reached when the
// function has an exception block; otherwise every assignment stays local.
pub(crate) fn mark_local_assignment_targets(func: &mut PlFunction) -> PgResult<()> {
    if !block_has_exceptions(&func.action) {
        return Ok(());
    }
    let mut args = ::mcx::std_vec_with_capacity("plpgsql", func.fn_argvarnos.len())?;
    args.extend(
        func.fn_argvarnos
            .iter()
            .zip(func.fn_arg_is_input.iter().chain(std::iter::repeat(&true)))
            .filter_map(|(&dno, &is_input)| is_input.then_some(dno))
            .take(func.fn_nargs as usize),
    );
    mark_block(&mut func.action, &args)
}

fn block_has_exceptions(block: &PlBlock) -> bool {
    block.exceptions.is_some() || stmts_have_exceptions(&block.body)
}

fn stmts_have_exceptions(stmts: &[PlStmt]) -> bool {
    stmts.iter().any(|s| {
        let mut found = false;
        for_each_child_body(s, &mut |body| found |= stmts_have_exceptions(body));
        found || matches!(s, PlStmt::Block(b) if block_has_exceptions(b))
    })
}

fn for_each_child_body(s: &PlStmt, f: &mut dyn FnMut(&[PlStmt])) {
    match s {
        PlStmt::If { then_body, elsifs, else_body, .. } => {
            f(then_body);
            for (_, b) in elsifs {
                f(b);
            }
            if let Some(b) = else_body {
                f(b);
            }
        }
        PlStmt::Case { whens, else_stmts, .. } => {
            for (_, b) in whens {
                f(b);
            }
            f(else_stmts);
        }
        PlStmt::Loop { body, .. }
        | PlStmt::While { body, .. }
        | PlStmt::ForI { body, .. }
        | PlStmt::ForS { body, .. }
        | PlStmt::ForC { body, .. }
        | PlStmt::DynForS { body, .. }
        | PlStmt::ForEachA { body, .. } => f(body),
        PlStmt::Block(b) => {
            if let Some(ex) = &b.exceptions {
                for e in &ex.exc_list {
                    f(&e.action);
                }
            }
        }
        _ => {}
    }
}

fn mark_block(block: &mut PlBlock, local: &[Dno]) -> PgResult<()> {
    if let Some(ex) = &mut block.exceptions {
        mark_stmts(&mut block.body, &[])?;
        for e in &mut ex.exc_list {
            mark_stmts(&mut e.action, &[])?;
        }
        Ok(())
    } else {
        let mut inner =
            ::mcx::std_vec_with_capacity("plpgsql", local.len() + block.initvarnos.len())?;
        inner.extend_from_slice(local);
        inner.extend_from_slice(&block.initvarnos);
        mark_stmts(&mut block.body, &inner)
    }
}

fn mark_stmts(stmts: &mut [PlStmt], local: &[Dno]) -> PgResult<()> {
    for s in stmts {
        match s {
            PlStmt::Block(b) => mark_block(b, local)?,
            PlStmt::Assign { expr, .. } => {
                if expr.target_param >= 0 {
                    expr.target_is_local = local.contains(&expr.target_param);
                }
            }
            PlStmt::If { then_body, elsifs, else_body, .. } => {
                mark_stmts(then_body, local)?;
                for (_, b) in elsifs {
                    mark_stmts(b, local)?;
                }
                if let Some(b) = else_body {
                    mark_stmts(b, local)?;
                }
            }
            PlStmt::Case { whens, else_stmts, .. } => {
                for (_, b) in whens {
                    mark_stmts(b, local)?;
                }
                mark_stmts(else_stmts, local)?;
            }
            PlStmt::Loop { body, .. }
            | PlStmt::While { body, .. }
            | PlStmt::ForI { body, .. }
            | PlStmt::ForS { body, .. }
            | PlStmt::ForC { body, .. }
            | PlStmt::DynForS { body, .. }
            | PlStmt::ForEachA { body, .. } => mark_stmts(body, local)?,
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod local_target_tests {
    use super::*;

    fn assign(varno: Dno) -> PlStmt {
        PlStmt::Assign {
            lineno: 1,
            varno,
            expr: PlExpr {
                query: "1".into(),
                parse_mode: parser_seams::RawParseMode::RAW_PARSE_PLPGSQL_ASSIGN1,
                ns: 0,
                expr_id: 0,
                target_param: varno,
                target_is_local: true,
            },
        }
    }

    fn block(initvarnos: Vec<Dno>, body: Vec<PlStmt>, handler: Option<Vec<PlStmt>>) -> PlBlock {
        PlBlock {
            lineno: 1,
            label: None,
            body,
            initvarnos,
            exceptions: handler.map(|action| ExceptionBlock {
                sqlstate_varno: -1,
                sqlerrm_varno: -1,
                exc_list: vec![PlException { lineno: 1, conditions: Vec::new(), action }],
            }),
        }
    }

    fn locals(stmts: &[PlStmt], out: &mut Vec<(Dno, bool)>) {
        for s in stmts {
            match s {
                PlStmt::Assign { varno, expr, .. } => out.push((*varno, expr.target_is_local)),
                PlStmt::Block(b) => {
                    locals(&b.body, out);
                    if let Some(ex) = &b.exceptions {
                        for e in &ex.exc_list {
                            locals(&e.action, out);
                        }
                    }
                }
                PlStmt::Loop { body, .. } => locals(body, out),
                _ => {}
            }
        }
    }

    // pl_funcs.c mark_stmt: an exception block resets the local set for its
    // body and handlers; a plain block adds its own variables.
    #[test]
    fn assignment_targets_are_local_up_to_the_nearest_exception_block() {
        let inner = block(vec![2], vec![assign(2), assign(1), assign(0)], None);
        let guarded = block(
            vec![1],
            vec![assign(1), PlStmt::Block(inner)],
            Some(vec![assign(1), assign(0)]),
        );
        let mut action = block(
            vec![1],
            vec![assign(0), assign(1), PlStmt::Loop { lineno: 1, label: None, body: vec![assign(1)] }, PlStmt::Block(guarded)],
            None,
        );
        assert!(block_has_exceptions(&action));
        mark_block(&mut action, &[0]).unwrap();
        let mut got = Vec::new();
        locals(&action.body, &mut got);
        assert_eq!(
            got,
            vec![
                (0, true),
                (1, true),
                (1, true),
                (1, false),
                (2, true),
                (1, false),
                (0, false),
                (1, false),
                (0, false),
            ]
        );
    }
}
