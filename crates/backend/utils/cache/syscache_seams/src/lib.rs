use types_core::Oid;
use types_error::PgResult;
use types_storage::PgClassShape;
use types_tuple::{HeapTupleData, PgTypeShape};

seam_core::seam!(
    pub fn search_syscache_exists_reloid(reloid: Oid) -> PgResult<bool>
);

seam_core::seam!(
    pub fn init_catalog_cache_phase2() -> PgResult<()>
);

seam_core::seam!(
    pub fn search_syscache_exists_procoid(funcid: Oid) -> PgResult<bool>
);

seam_core::seam!(
    pub fn search_syscache_exists_attnum(relid: Oid, attnum: i16) -> PgResult<bool>
);

seam_core::seam!(
    pub fn search_syscache_exists_databaseoid(dboid: Oid) -> PgResult<bool>
);

seam_core::seam!(
    pub fn search_syscache_exists_tablespaceoid(tblspcoid: Oid) -> PgResult<bool>
);

seam_core::seam!(
    pub fn sys_cache_invalidate(cache_id: i32, hash_value: u32) -> PgResult<()>
);

seam_core::seam!(
    pub fn relation_invalidates_snapshots_only(relid: Oid) -> bool
);

seam_core::seam!(
    pub fn lookup_pg_class_by_relid(relid: Oid) -> PgResult<Option<PgClassShape>>
);

seam_core::seam!(
    pub fn pg_class_shape(tuple: &HeapTupleData<'_>) -> PgClassShape
);

seam_core::seam!(
    pub fn pg_attribute_attrelid(tuple: &HeapTupleData<'_>) -> Oid
);

seam_core::seam!(
    pub fn pg_index_indexrelid(tuple: &HeapTupleData<'_>) -> Oid
);

seam_core::seam!(
    // Some(conrelid) iff contype == CONSTRAINT_FOREIGN && OidIsValid(conrelid).
    pub fn pg_constraint_fk_target(tuple: &HeapTupleData<'_>) -> Option<Oid>
);

seam_core::seam!(
    pub fn lookup_pg_type_shape(typid: Oid) -> PgResult<Option<PgTypeShape>>
);

#[derive(Clone, Copy, Debug)]
pub struct PgSequenceForm {
    pub seqtypid: Oid,
    pub seqstart: i64,
    pub seqincrement: i64,
    pub seqmax: i64,
    pub seqmin: i64,
    pub seqcache: i64,
    pub seqcycle: bool,
}

seam_core::seam!(
    pub fn lookup_pg_sequence_form(relid: Oid) -> PgResult<Option<PgSequenceForm>>
);

seam_core::seam!(
    pub fn lookup_authid_rolname<'mcx>(
        mcx: mcx::Mcx<'mcx>,
        roleid: Oid,
    ) -> PgResult<Option<mcx::PgString<'mcx>>>
);

seam_core::seam!(
    pub fn lookup_authid_rolname_data(roleid: Oid) -> PgResult<Option<types_tuple::NameData>>
);

seam_core::seam!(
    pub fn lookup_authid_by_rolname(rolname: &str) -> PgResult<Option<(Oid, bool)>>
);

#[derive(Clone, Copy, Debug)]
pub struct AuthIdSessionShape {
    pub roleid: Oid,
    pub rolname: types_tuple::NameData,
    pub rolsuper: bool,
    pub rolcanlogin: bool,
    pub rolconnlimit: i32,
}

seam_core::seam!(
    pub fn lookup_authid_session_by_rolname(
        rolname: &str,
    ) -> PgResult<Option<AuthIdSessionShape>>
);

seam_core::seam!(
    pub fn lookup_authid_session_by_oid(roleid: Oid) -> PgResult<Option<AuthIdSessionShape>>
);

#[derive(Debug)]
pub struct AuthIdPasswordShape<'mcx> {
    pub rolpassword: Option<mcx::PgString<'mcx>>,
    pub rolvaliduntil: Option<types_core::TimestampTz>,
}

seam_core::seam!(
    pub fn lookup_authid_rolpassword<'mcx>(
        mcx: mcx::Mcx<'mcx>,
        rolname: &str,
    ) -> PgResult<Option<AuthIdPasswordShape<'mcx>>>
);

use datum::Datum;
use mcx::{Mcx, PgString, PgVec};
use types_core::AttrNumber;
use types_tuple::NameData;


#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgAmopShape {
    pub amopstrategy: i16,
    pub amopsortfamily: Oid,
    pub amoplefttype: Oid,
    pub amoprighttype: Oid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgAmopMemberShape {
    pub amopfamily: Oid,
    pub amoplefttype: Oid,
    pub amoprighttype: Oid,
    pub amopstrategy: i16,
    pub amopmethod: Oid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgAmprocMemberShape {
    pub amprocrighttype: Oid,
    pub amprocnum: i16,
    pub amproc: Oid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgAttributeLsShape {
    pub attname: NameData,
    pub atttypid: Oid,
    pub atttypmod: i32,
    pub attcollation: Oid,
    pub attgenerated: i8,
    // SearchSysCache2(ATTNUM) returns dropped columns too; only the
    // SearchSysCacheAttName/AttNum callers filter attisdropped, so the row
    // carries the flag and the caller decides.
    pub attisdropped: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgCollationShape {
    pub collname: NameData,
    pub collnamespace: Oid,
    pub collisdeterministic: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgConstraintShape {
    pub conname: NameData,
    pub contype: i8,
    pub conindid: Oid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgConstraintDescShape {
    pub conname: NameData,
    pub connamespace: Oid,
    pub conrelid: Oid,
    pub contypid: Oid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgOpclassShape {
    pub opcmethod: Oid,
    pub opcfamily: Oid,
    pub opcintype: Oid,
    pub opckeytype: Oid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgOpfamilyShape {
    pub opfmethod: Oid,
    pub opfname: NameData,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgOperatorShape {
    pub oprnamespace: Oid,
    pub oprleft: Oid,
    pub oprright: Oid,
    pub oprresult: Oid,
    pub oprcom: Oid,
    pub oprnegate: Oid,
    pub oprcode: Oid,
    pub oprrest: Oid,
    pub oprjoin: Oid,
    pub oprcanmerge: bool,
    pub oprcanhash: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgProcShape {
    pub pronamespace: Oid,
    pub prorettype: Oid,
    pub provariadic: Oid,
    pub prosupport: Oid,
    pub prolang: Oid,
    pub pronargs: i16,
    pub prokind: i8,
    pub provolatile: i8,
    pub proparallel: i8,
    pub proretset: bool,
    pub proisstrict: bool,
    pub proleakproof: bool,
    pub prosecdef: bool,
    pub proconfig_isnull: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgProcFmgrShape {
    pub prolang: Oid,
    pub prorettype: Oid,
    pub pronargs: i16,
    pub proisstrict: bool,
    pub proretset: bool,
    pub prosecdef: bool,
    pub proconfig_isnull: bool,
    pub xmin: types_core::TransactionId,
    pub tid: types_tuple::ItemPointerData,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PgProcSecdefShape {
    pub proowner: Oid,
    pub prosecdef: bool,
    // "name=value" proconfig entries.
    pub proconfig: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgLanguageFmgrShape {
    pub lanplcallfoid: Oid,
    pub laninline: Oid,
    pub lanvalidator: Oid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgClassLsShape {
    pub relnamespace: Oid,
    pub reltype: Oid,
    pub relam: Oid,
    pub reltablespace: Oid,
    pub relnatts: i16,
    pub relkind: i8,
    pub relpersistence: i8,
    pub relispartition: bool,
    pub relhassubclass: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgTransformShape {
    pub trffromsql: Oid,
    pub trftosql: Oid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgTypeElementShape {
    pub typelem: Oid,
    pub typsubscript: Oid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgTypeBaseShape {
    pub typtype: i8,
    pub typbasetype: Oid,
    pub typtypmod: i32,
    pub typelem: Oid,
    pub typsubscript: Oid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgTypeIoShape {
    pub oid: Oid,
    pub typinput: Oid,
    pub typoutput: Oid,
    pub typreceive: Oid,
    pub typsend: Oid,
    pub typmodin: Oid,
    pub typmodout: Oid,
    pub typelem: Oid,
    pub typlen: i16,
    pub typbyval: bool,
    pub typalign: i8,
    pub typdelim: i8,
    pub typisdefined: bool,
}

#[derive(Debug)]
pub struct PgTypeDefaultShape<'mcx> {
    pub typdefaultbin: Option<PgString<'mcx>>,
    pub typdefault: Option<PgString<'mcx>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgRangeShape {
    pub rngsubtype: Oid,
    pub rngmultitypid: Oid,
    pub rngcollation: Oid,
    pub rngsubopc: Oid,
    pub rngcanonical: Oid,
    pub rngsubdiff: Oid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgIndexLsShape {
    pub indnatts: i16,
    pub indnkeyatts: i16,
    pub indisreplident: bool,
    pub indisvalid: bool,
    pub indisclustered: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgStatisticSlotShape {
    pub stakind: [i16; 5],
    pub staop: [Oid; 5],
    pub stacoll: [Oid; 5],
}

seam_core::seam!(
    // purpose is AMOP_SEARCH b's' / AMOP_ORDER b'o'.
    pub fn lookup_pg_amop_by_operator(
        opno: Oid,
        purpose: u8,
        opfamily: Oid,
    ) -> PgResult<Option<PgAmopShape>>
);

seam_core::seam!(
    pub fn lookup_pg_amop_by_strategy(
        opfamily: Oid,
        lefttype: Oid,
        righttype: Oid,
        strategy: i16,
    ) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn lookup_pg_amop_members_by_operator<'mcx>(
        mcx: Mcx<'mcx>,
        opno: Oid,
    ) -> PgResult<PgVec<'mcx, PgAmopMemberShape>>
);

seam_core::seam!(
    pub fn lookup_pg_amproc(
        opfamily: Oid,
        lefttype: Oid,
        righttype: Oid,
        procnum: i16,
    ) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn lookup_pg_amproc_members<'mcx>(
        mcx: Mcx<'mcx>,
        opfamily: Oid,
        lefttype: Oid,
    ) -> PgResult<PgVec<'mcx, PgAmprocMemberShape>>
);

seam_core::seam!(
    pub fn lookup_pg_attribute_shape(
        relid: Oid,
        attnum: AttrNumber,
    ) -> PgResult<Option<PgAttributeLsShape>>
);

seam_core::seam!(
    pub fn lookup_pg_attribute_attnum_by_name(relid: Oid, attname: &str) -> PgResult<AttrNumber>
);

seam_core::seam!(
    pub fn pg_attribute_attoptions<'mcx>(
        mcx: Mcx<'mcx>,
        relid: Oid,
        attnum: i16,
    ) -> PgResult<Option<Option<Datum>>>
);

seam_core::seam!(
    pub fn pg_tablespace_spcoptions<'mcx>(
        mcx: Mcx<'mcx>,
        spcid: Oid,
    ) -> PgResult<Option<Option<PgVec<'mcx, u8>>>>
);

seam_core::seam!(
    pub fn lookup_pg_cast_oid(sourcetypeid: Oid, targettypeid: Oid) -> PgResult<Oid>
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgCastShape {
    pub oid: Oid,
    pub castfunc: Oid,
    pub castcontext: i8,
    pub castmethod: i8,
}

seam_core::seam!(
    pub fn lookup_pg_cast_shape(
        sourcetypeid: Oid,
        targettypeid: Oid,
    ) -> PgResult<Option<PgCastShape>>
);

seam_core::seam!(
    pub fn lookup_pg_operator_oid_exact(
        opername: &str,
        oprleft: Oid,
        oprright: Oid,
        oprnamespace: Oid,
    ) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn lookup_pg_operator_candidates<'mcx>(
        mcx: Mcx<'mcx>,
        opername: &str,
        oprleft: Oid,
        oprright: Oid,
    ) -> PgResult<PgVec<'mcx, (Oid, Oid)>>
);

seam_core::seam!(
    pub fn pg_operator_name_candidates_exist(opername: &str, oprkind: i8) -> PgResult<bool>
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgOperatorNameCandidate {
    pub oid: Oid,
    pub oprnamespace: Oid,
    pub oprkind: i8,
    pub oprleft: Oid,
    pub oprright: Oid,
}

seam_core::seam!(
    pub fn lookup_pg_operator_name_candidates<'mcx>(
        mcx: Mcx<'mcx>,
        opername: &str,
    ) -> PgResult<PgVec<'mcx, PgOperatorNameCandidate>>
);

seam_core::seam!(
    pub fn lookup_pg_collation_shape(colloid: Oid) -> PgResult<Option<PgCollationShape>>
);

seam_core::seam!(
    pub fn pg_operator_oprnamensp(opno: Oid) -> PgResult<Option<(NameData, Oid)>>
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgTsObjectRow {
    pub name: NameData,
    pub namespace_oid: Oid,
}

seam_core::seam!(
    pub fn lookup_pg_ts_config_oid_by_name_nsp(cfgname: &str, cfgnamespace: Oid) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn lookup_pg_ts_config_row(cfgid: Oid) -> PgResult<Option<PgTsObjectRow>>
);

seam_core::seam!(
    pub fn lookup_pg_ts_dict_oid_by_name_nsp(dictname: &str, dictnamespace: Oid) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn lookup_pg_conversion_oid_by_name_nsp(conname: &str, connamespace: Oid) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn lookup_pg_statistic_ext_oid_by_name_nsp(stxname: &str, stxnamespace: Oid) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn lookup_pg_ts_dict_row(dictid: Oid) -> PgResult<Option<PgTsObjectRow>>
);


seam_core::seam!(
    pub fn lookup_pg_constraint_shape(conoid: Oid) -> PgResult<Option<PgConstraintShape>>
);

seam_core::seam!(
    pub fn lookup_pg_constraint_desc_shape(conoid: Oid) -> PgResult<Option<PgConstraintDescShape>>
);

seam_core::seam!(
    pub fn pg_type_name_namespace(typid: Oid) -> PgResult<Option<(NameData, Oid)>>
);

seam_core::seam!(
    pub fn lookup_pg_language_name(langoid: Oid) -> PgResult<Option<NameData>>
);

seam_core::seam!(
    pub fn lookup_pg_opclass_shape(opclass: Oid) -> PgResult<Option<PgOpclassShape>>
);

seam_core::seam!(
    pub fn pg_opclass_name_namespace_method(
        opclass: Oid,
    ) -> PgResult<Option<(types_tuple::NameData, Oid, Oid)>>
);

seam_core::seam!(
    pub fn lookup_pg_opclass_oid_by_name(amid: Oid, opcname: &str, opcnamespace: Oid) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn lookup_pg_opfamily_shape(opfid: Oid) -> PgResult<Option<PgOpfamilyShape>>
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgAmopRow {
    pub amopfamily: Oid,
    pub amoplefttype: Oid,
    pub amoprighttype: Oid,
    pub amopstrategy: i16,
    pub amoppurpose: i8,
    pub amopopr: Oid,
    pub amopsortfamily: Oid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgAmprocRow {
    pub amprocfamily: Oid,
    pub amproclefttype: Oid,
    pub amprocrighttype: Oid,
    pub amprocnum: i16,
    pub amproc: Oid,
}

seam_core::seam!(
    pub fn lookup_pg_amop_rows<'mcx>(
        mcx: Mcx<'mcx>,
        opfamily: Oid,
    ) -> PgResult<(PgVec<'mcx, PgAmopRow>, bool)>
);

seam_core::seam!(
    pub fn lookup_pg_amproc_rows<'mcx>(
        mcx: Mcx<'mcx>,
        opfamily: Oid,
    ) -> PgResult<(PgVec<'mcx, PgAmprocRow>, bool)>
);

seam_core::seam!(
    pub fn lookup_pg_opclass_rows_by_am<'mcx>(
        mcx: Mcx<'mcx>,
        amoid: Oid,
    ) -> PgResult<PgVec<'mcx, (Oid, Oid, Oid, bool, NameData)>>
);

seam_core::seam!(
    pub fn pg_opclass_opcname(opclass: Oid) -> PgResult<Option<NameData>>
);

seam_core::seam!(
    pub fn lookup_pg_opfamily_oid_exact(amoid: Oid, opfname: &str, nsp: Oid) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn lookup_pg_operator_shape(opno: Oid) -> PgResult<Option<PgOperatorShape>>
);

seam_core::seam!(
    pub fn pg_operator_oprname(opno: Oid) -> PgResult<Option<NameData>>
);

seam_core::seam!(
    pub fn lookup_pg_proc_fmgr(funcid: Oid) -> PgResult<Option<PgProcFmgrShape>>
);

seam_core::seam!(
    pub fn lookup_pg_proc_secdef(funcid: Oid) -> PgResult<Option<PgProcSecdefShape>>
);

seam_core::seam!(
    pub fn lookup_pg_language_fmgr(langoid: Oid) -> PgResult<Option<PgLanguageFmgrShape>>
);

seam_core::seam!(
    pub fn lookup_pg_proc_prosrc<'mcx>(
        mcx: mcx::Mcx<'mcx>,
        funcid: Oid,
    ) -> PgResult<Option<mcx::PgString<'mcx>>>
);

seam_core::seam!(
    pub fn lookup_pg_proc_probin<'mcx>(
        mcx: mcx::Mcx<'mcx>,
        funcid: Oid,
    ) -> PgResult<Option<mcx::PgString<'mcx>>>
);

seam_core::seam!(
    pub fn lookup_pg_proc_shape(funcid: Oid) -> PgResult<Option<PgProcShape>>
);

seam_core::seam!(
    pub fn pg_proc_proname(funcid: Oid) -> PgResult<Option<NameData>>
);

seam_core::seam!(
    pub fn lookup_pg_proc_signature<'mcx>(
        mcx: Mcx<'mcx>,
        funcid: Oid,
    ) -> PgResult<Option<(Oid, PgVec<'mcx, Oid>)>>
);

#[derive(Debug)]
pub struct PgProcResultArraysShape<'mcx> {
    pub proallargtypes: Option<PgVec<'mcx, Oid>>,
    pub proargmodes: Option<PgVec<'mcx, i8>>,
    pub proargnames: Option<PgVec<'mcx, PgString<'mcx>>>,
}

seam_core::seam!(
    pub fn pg_proc_result_arrays<'mcx>(
        mcx: Mcx<'mcx>,
        funcid: Oid,
    ) -> PgResult<Option<PgProcResultArraysShape<'mcx>>>
);

seam_core::seam!(
    pub fn lookup_pg_class_relid_by_name(relname: &[u8], relnamespace: Oid) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn lookup_pg_type_oid_by_name(typname: &str, typnamespace: Oid) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn lookup_pg_class_ls_shape(relid: Oid) -> PgResult<Option<PgClassLsShape>>
);

seam_core::seam!(
    pub fn pg_class_relname(relid: Oid) -> PgResult<Option<NameData>>
);

seam_core::seam!(
    pub fn pg_class_reloftype(relid: Oid) -> PgResult<Option<Oid>>
);

seam_core::seam!(
    pub fn pg_class_relrewrite(relid: Oid) -> PgResult<Option<Oid>>
);

seam_core::seam!(
    pub fn lookup_pg_transform_shape(typid: Oid, langid: Oid) -> PgResult<Option<PgTransformShape>>
);

seam_core::seam!(
    pub fn pg_type_isdefined(typid: Oid) -> PgResult<Option<bool>>
);

seam_core::seam!(
    pub fn pg_type_typtype(typid: Oid) -> PgResult<Option<i8>>
);

seam_core::seam!(
    pub fn pg_type_category(typid: Oid) -> PgResult<Option<(i8, bool)>>
);

seam_core::seam!(
    pub fn pg_type_typrelid(typid: Oid) -> PgResult<Option<Oid>>
);

seam_core::seam!(
    pub fn pg_type_typnamespace(typid: Oid) -> PgResult<Option<Oid>>
);

seam_core::seam!(
    pub fn pg_type_element_shape(typid: Oid) -> PgResult<Option<PgTypeElementShape>>
);

seam_core::seam!(
    pub fn pg_type_typarray(typid: Oid) -> PgResult<Option<Oid>>
);

seam_core::seam!(
    pub fn pg_type_base_shape(typid: Oid) -> PgResult<Option<PgTypeBaseShape>>
);

seam_core::seam!(
    pub fn pg_type_io_shape(typid: Oid) -> PgResult<Option<PgTypeIoShape>>
);

seam_core::seam!(
    pub fn pg_type_default_strings<'mcx>(
        mcx: Mcx<'mcx>,
        typid: Oid,
    ) -> PgResult<Option<PgTypeDefaultShape<'mcx>>>
);

seam_core::seam!(
    // inner None = attisnull, outer None = no such proc.
    pub fn pg_proc_proargdefaults<'mcx>(
        mcx: Mcx<'mcx>,
        funcid: Oid,
    ) -> PgResult<Option<Option<PgString<'mcx>>>>
);

seam_core::seam!(
    pub fn pg_constraint_primary_key_attnos<'mcx>(
        mcx: Mcx<'mcx>,
        relid: Oid,
        deferrable_ok: bool,
    ) -> PgResult<Option<(PgVec<'mcx, i16>, Oid)>>
);

seam_core::seam!(
    pub fn lookup_pg_range_shape(range_oid: Oid) -> PgResult<Option<PgRangeShape>>
);

seam_core::seam!(
    pub fn lookup_pg_range_by_multirange(multirange_oid: Oid) -> PgResult<Option<Oid>>
);

seam_core::seam!(
    pub fn lookup_pg_index_ls_shape(index_oid: Oid) -> PgResult<Option<PgIndexLsShape>>
);

seam_core::seam!(
    // PRECONDITION: index < indclass->dim1.
    pub fn pg_index_indclass_element(index_oid: Oid, index: i32) -> PgResult<Option<Oid>>
);

seam_core::seam!(
    // PRECONDITION: index < indoption->dim1.
    pub fn pg_index_indoption_element(index_oid: Oid, index: i32) -> PgResult<Option<i16>>
);

seam_core::seam!(
    pub fn pg_am_amtype(amoid: Oid) -> PgResult<Option<i8>>
);

seam_core::seam!(
    pub fn pg_am_amhandler(amoid: Oid) -> PgResult<Option<Oid>>
);

seam_core::seam!(
    pub fn pg_am_amname(amoid: Oid) -> PgResult<Option<String>>
);

seam_core::seam!(
    pub fn lookup_pg_publication_oid(pubname: &str) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn pg_publication_pubname(pubid: Oid) -> PgResult<Option<NameData>>
);

seam_core::seam!(
    pub fn lookup_pg_subscription_oid(dbid: Oid, subname: &str) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn pg_subscription_subname(subid: Oid) -> PgResult<Option<NameData>>
);

seam_core::seam!(
    pub fn pg_statistic_stawidth(relid: Oid, attnum: AttrNumber, inh: bool) -> PgResult<Option<i32>>
);

pub struct PgProcCostShape {
    pub procost: f32,
    pub prorows: f32,
    pub prosupport: Oid,
}

seam_core::seam!(
    pub fn pg_proc_cost_shape(funcid: Oid) -> PgResult<Option<PgProcCostShape>>
);

pub struct PgStatisticShape {
    pub stanullfrac: f32,
    pub stawidth: i32,
    pub stadistinct: f32,
}

seam_core::seam!(
    pub fn lookup_pg_statistic_shape(
        relid: Oid,
        attnum: AttrNumber,
        inh: bool,
    ) -> PgResult<Option<PgStatisticShape>>
);

seam_core::seam!(
    pub fn pg_statistic_slot_shape(tuple: &HeapTupleData<'_>) -> PgStatisticSlotShape
);

seam_core::seam!(
    pub fn pg_statistic_slot_array_image<'mcx>(
        mcx: Mcx<'mcx>,
        tuple: &HeapTupleData<'_>,
        attnum: i32,
    ) -> PgResult<PgVec<'mcx, u8>>
);

// Byref `values` datums point into the fetched values_image, whose heap buffer is stable across moves of this struct.
pub struct PgStatisticSlotData<'mcx> {
    pub kind: i16,
    pub staop: Oid,
    pub stacoll: Oid,
    mcx: Mcx<'mcx>,
    // MAXALIGN'd heap-tuple image of the pg_statistic row (t_len bytes used).
    image: Option<(&'mcx [u64], u32)>,
    pos: i32,
    images: core::cell::OnceCell<PgStatisticSlotImages<'mcx>>,
    values: core::cell::OnceCell<PgVec<'mcx, Datum>>,
    numbers: core::cell::OnceCell<PgVec<'mcx, f32>>,
}

pub struct PgStatisticSlotImages<'mcx> {
    pub valuetype: Oid,
    pub stakind: i16,
    pub staop: Oid,
    pub values_image: PgVec<'mcx, u8>,
    pub numbers_image: PgVec<'mcx, u8>,
}

impl<'mcx> PgStatisticSlotData<'mcx> {
    pub fn lazy(
        kind: i16,
        staop: Oid,
        stacoll: Oid,
        mcx: Mcx<'mcx>,
        image: Option<(&'mcx [u64], u32)>,
        pos: i32,
    ) -> Self {
        PgStatisticSlotData {
            kind,
            staop,
            stacoll,
            mcx,
            image,
            pos,
            images: core::cell::OnceCell::new(),
            values: core::cell::OnceCell::new(),
            numbers: core::cell::OnceCell::new(),
        }
    }

    pub fn from_decoded(
        kind: i16,
        staop: Oid,
        stacoll: Oid,
        valuetype: Oid,
        values: PgVec<'mcx, Datum>,
        numbers: PgVec<'mcx, f32>,
        values_image: PgVec<'mcx, u8>,
    ) -> Self {
        let mcx = *values.allocator();
        let s = PgStatisticSlotData::lazy(kind, staop, stacoll, mcx, None, 0);
        let _ = s.images.set(PgStatisticSlotImages {
            valuetype,
            stakind: kind,
            staop,
            values_image,
            numbers_image: PgVec::new_in(mcx),
        });
        let _ = s.values.set(values);
        let _ = s.numbers.set(numbers);
        s
    }

    fn images(&self) -> PgResult<&PgStatisticSlotImages<'mcx>> {
        if let Some(i) = self.images.get() {
            return Ok(i);
        }
        let empty = || PgStatisticSlotImages {
            valuetype: types_core::InvalidOid,
            stakind: 0,
            staop: types_core::InvalidOid,
            values_image: PgVec::new_in(self.mcx),
            numbers_image: PgVec::new_in(self.mcx),
        };
        let img = match self.image {
            None => empty(),
            Some((image, t_len)) => {
                let img =
                    lookup_pg_statistic_slot_images::call(self.mcx, image, t_len, self.pos)?;
                // A kind/staop mismatch can only be a decode fault: never decode arrays under a stale kind/type.
                if img.stakind != self.kind || img.staop != self.staop {
                    empty()
                } else {
                    img
                }
            }
        };
        Ok(self.images.get_or_init(|| img))
    }

    /// By-ref stavalues Datums borrow this bundle's values_image: reading a
    /// copied-out Datum past the bundle's life is the detoast drop-order UAF.
    pub fn values(&self) -> PgResult<&[Datum]> {
        if let Some(v) = self.values.get() {
            return Ok(v);
        }
        let img = self.images()?;
        let v = decode_pg_statistic_values::call(self.mcx, img.valuetype, &img.values_image)?;
        Ok(self.values.get_or_init(|| v))
    }

    pub fn numbers(&self) -> PgResult<&[f32]> {
        if let Some(n) = self.numbers.get() {
            return Ok(n);
        }
        let img = self.images()?;
        let n = decode_pg_statistic_numbers::call(self.mcx, &img.numbers_image)?;
        Ok(self.numbers.get_or_init(|| n))
    }

    pub fn values_image(&self) -> PgResult<&[u8]> {
        Ok(&self.images()?.values_image)
    }

    pub fn valuetype(&self) -> PgResult<Oid> {
        Ok(self.images()?.valuetype)
    }
}

seam_core::seam!(
    pub fn lookup_pg_statistic_slot_images<'mcx>(
        mcx: Mcx<'mcx>,
        image: &[u64],
        t_len: u32,
        pos: i32,
    ) -> PgResult<PgStatisticSlotImages<'mcx>>
);

seam_core::seam!(
    // Byref datums point into `image`.
    pub fn decode_pg_statistic_values<'mcx>(
        mcx: Mcx<'mcx>,
        valuetype: Oid,
        image: &[u8],
    ) -> PgResult<PgVec<'mcx, Datum>>
);

seam_core::seam!(
    pub fn decode_pg_statistic_numbers<'mcx>(
        mcx: Mcx<'mcx>,
        image: &[u8],
    ) -> PgResult<PgVec<'mcx, f32>>
);

pub struct PgStatisticBundle<'mcx> {
    pub stanullfrac: f32,
    pub stawidth: i32,
    pub stadistinct: f32,
    pub slots: PgVec<'mcx, PgStatisticSlotData<'mcx>>,
}

// Drops reclaim only arena bytes (PgVecs and OnceCell contents are all mcx-backed),
// so forgetting them loses nothing beyond the arena the planning cycle resets anyway.
mcx::forget_safe_struct!(
    PgStatisticSlotImages<'_> { valuetype, stakind, staop, values_image, numbers_image },
    PgStatisticSlotData<'_> { kind, staop, stacoll, mcx, image, pos, images, values, numbers },
    PgStatisticBundle<'_> { stanullfrac, stawidth, stadistinct, slots },
);

mcx::forget_safe_nodrop!(PgOperatorShape, PgAmopShape, PgAmopMemberShape);

seam_core::seam!(
    pub fn lookup_pg_statistic_bundle<'mcx>(
        mcx: Mcx<'mcx>,
        relid: Oid,
        attnum: AttrNumber,
        inh: bool,
    ) -> PgResult<Option<PgStatisticBundle<'mcx>>>
);

seam_core::seam!(
    // A missing pg_attribute row is an error.
    pub fn lookup_pg_attribute_stattarget(relid: Oid, attnum: AttrNumber) -> PgResult<Option<i16>>
);

seam_core::seam!(
    pub fn pg_type_typanalyze(typid: Oid) -> PgResult<Oid>
);

pub struct StatExtForm<'mcx> {
    pub stxrelid: Oid,
    pub keys: PgVec<'mcx, i16>,
    pub kinds: PgVec<'mcx, u8>,
    pub stattarget: i32,
    pub has_exprs: bool,
}

seam_core::seam!(
    pub fn statext_form<'mcx>(mcx: Mcx<'mcx>, statoid: Oid) -> PgResult<Option<StatExtForm<'mcx>>>
);

seam_core::seam!(
    pub fn statext_data_kinds(statoid: Oid, inh: bool) -> PgResult<Option<(bool, bool, bool, bool)>>
);

seam_core::seam!(
    pub fn statext_data_blob<'mcx>(
        mcx: Mcx<'mcx>,
        statoid: Oid,
        inh: bool,
        anum: i32,
    ) -> PgResult<Option<Option<PgVec<'mcx, u8>>>>
);

seam_core::seam!(
    pub fn statext_exprs_src<'mcx>(
        mcx: Mcx<'mcx>,
        statoid: Oid,
    ) -> PgResult<Option<PgString<'mcx>>>
);

seam_core::seam!(
    // Slots decode eagerly — the lazy refetch key targets pg_statistic rows, which these are not.
    pub fn statext_expressions_load<'mcx>(
        mcx: Mcx<'mcx>,
        statoid: Oid,
        inh: bool,
        idx: i32,
    ) -> PgResult<Option<PgStatisticBundle<'mcx>>>
);

seam_core::seam!(
    pub fn pg_namespace_nspname(nspid: Oid) -> PgResult<Option<NameData>>
);

seam_core::seam!(
    pub fn relation_has_sys_cache(relid: Oid) -> bool
);

seam_core::seam!(
    // rel OR supporting-index oid.
    pub fn relation_supports_sys_cache(relid: Oid) -> bool
);

#[derive(Clone, Copy, Debug)]
pub struct PgTypeTypcacheShape {
    pub typname: NameData,
    pub typlen: i16,
    pub typbyval: bool,
    pub typalign: i8,
    pub typstorage: i8,
    pub typtype: i8,
    pub typisdefined: bool,
    pub typrelid: Oid,
    pub typsubscript: Oid,
    pub typelem: Oid,
    pub typarray: Oid,
    pub typcollation: Oid,
}

seam_core::seam!(
    pub fn lookup_pg_type_typcache_shape(typid: Oid) -> PgResult<Option<PgTypeTypcacheShape>>
);

#[derive(Clone, Copy, Debug)]
pub struct PgTypeDomainShape {
    pub typname: NameData,
    pub typnamespace: Oid,
    pub typtype: i8,
    pub typnotnull: bool,
    pub typbasetype: Oid,
}

seam_core::seam!(
    pub fn pg_type_domain_shape(typid: Oid) -> PgResult<Option<PgTypeDomainShape>>
);

seam_core::seam!(
    pub fn syscache_hash_value_typeoid(typid: Oid) -> PgResult<u32>
);

seam_core::seam!(
    pub fn syscache_hash_value_procoid(funcid: Oid) -> PgResult<u32>
);

seam_core::seam!(
    pub fn lookup_pg_namespace_oid_by_name(nspname: &str) -> PgResult<Oid>
);

#[derive(Debug)]
pub struct PgCollationLocaleRow<'mcx> {
    pub collname: NameData,
    pub collnamespace: Oid,
    pub collprovider: u8,
    pub collisdeterministic: bool,
    pub collencoding: i32,
    pub collcollate: Option<PgString<'mcx>>,
    pub collctype: Option<PgString<'mcx>>,
    pub colllocale: Option<PgString<'mcx>>,
    pub collicurules: Option<PgString<'mcx>>,
    pub collversion: Option<PgString<'mcx>>,
}

seam_core::seam!(
    pub fn lookup_pg_collation_locale_row<'mcx>(
        mcx: Mcx<'mcx>,
        collid: Oid,
    ) -> PgResult<Option<PgCollationLocaleRow<'mcx>>>
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgCollationNameEncNspRow {
    pub oid: Oid,
    pub collprovider: u8,
}

seam_core::seam!(
    pub fn lookup_pg_collation_by_name_enc_nsp(
        collname: &str,
        encoding: i32,
        collnamespace: Oid,
    ) -> PgResult<Option<PgCollationNameEncNspRow>>
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PgAggregateShape {
    pub aggkind: i8,
    pub aggnumdirectargs: i16,
    pub aggtransfn: Oid,
    pub aggfinalfn: Oid,
    pub aggcombinefn: Oid,
    pub aggserialfn: Oid,
    pub aggdeserialfn: Oid,
    pub aggmtransfn: Oid,
    pub aggminvtransfn: Oid,
    pub aggmfinalfn: Oid,
    pub aggfinalextra: bool,
    pub aggmfinalextra: bool,
    pub aggfinalmodify: i8,
    pub aggmfinalmodify: i8,
    pub aggsortop: Oid,
    pub aggtranstype: Oid,
    pub aggtransspace: i32,
    pub aggmtranstype: Oid,
}

seam_core::seam!(
    pub fn lookup_pg_aggregate_shape(aggfnoid: Oid) -> PgResult<Option<PgAggregateShape>>
);

seam_core::seam!(
    pub fn pg_aggregate_agginitval<'mcx>(
        mcx: Mcx<'mcx>,
        aggfnoid: Oid,
    ) -> PgResult<Option<Option<PgString<'mcx>>>>
);

seam_core::seam!(
    pub fn pg_aggregate_aggminitval<'mcx>(
        mcx: Mcx<'mcx>,
        aggfnoid: Oid,
    ) -> PgResult<Option<Option<PgString<'mcx>>>>
);

#[derive(Debug)]
pub struct PgProcCandidate<'mcx> {
    pub oid: Oid,
    pub pronamespace: Oid,
    pub pronargs: i16,
    pub pronargdefaults: i16,
    pub provariadic: Oid,
    pub proargtypes: PgVec<'mcx, Oid>,
}

seam_core::seam!(
    pub fn lookup_pg_proc_name_candidates<'mcx>(
        mcx: Mcx<'mcx>,
        proname: &str,
    ) -> PgResult<PgVec<'mcx, PgProcCandidate<'mcx>>>
);

#[derive(Clone, Copy)]
pub struct PgEnumShape {
    pub oid: Oid,
    pub enumtypid: Oid,
    pub enumlabel: NameData,
    pub xmin: types_core::TransactionId,
    pub xmin_committed: bool,
}

seam_core::seam!(
    pub fn lookup_pg_enum_by_oid(enum_oid: Oid) -> PgResult<Option<PgEnumShape>>
);

seam_core::seam!(
    pub fn lookup_pg_enum_by_typid_label(typid: Oid, label: &str) -> PgResult<Option<PgEnumShape>>
);

#[derive(Clone, Copy)]
pub struct PgTsParserShape {
    pub prsstart: Oid,
    pub prstoken: Oid,
    pub prsend: Oid,
    pub prsheadline: Oid,
    pub prslextype: Oid,
}

seam_core::seam!(
    pub fn lookup_pg_ts_parser_shape(prsid: Oid) -> PgResult<Option<PgTsParserShape>>
);

pub struct PgTsDictShape<'mcx> {
    pub dictname: NameData,
    pub dictnamespace: Oid,
    pub dicttemplate: Oid,
    // dictinitoption text payload (detoasted, header stripped).
    pub dictinitoption: Option<mcx::PgVec<'mcx, u8>>,
}

seam_core::seam!(
    pub fn lookup_pg_ts_dict_shape<'mcx>(
        mcx: mcx::Mcx<'mcx>,
        dictid: Oid,
    ) -> PgResult<Option<PgTsDictShape<'mcx>>>
);

#[derive(Clone, Copy)]
pub struct PgTsTemplateShape {
    pub tmplinit: Oid,
    pub tmpllexize: Oid,
}

seam_core::seam!(
    pub fn lookup_pg_ts_template_shape(tmplid: Oid) -> PgResult<Option<PgTsTemplateShape>>
);

#[derive(Clone, Copy)]
pub struct PgTsConfigShape {
    pub cfgparser: Oid,
    pub cfgnamespace: Oid,
    pub cfgname: NameData,
}

seam_core::seam!(
    pub fn lookup_pg_ts_config_shape(cfgid: Oid) -> PgResult<Option<PgTsConfigShape>>
);

seam_core::seam!(
    pub fn lookup_pg_ts_config_oid_by_name(cfgname: &str, cfgnamespace: Oid) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn lookup_pg_ts_dict_oid_by_name(dictname: &str, dictnamespace: Oid) -> PgResult<Oid>
);

#[derive(Clone, Copy)]
pub struct PgTsConfigMapShape {
    pub maptokentype: i32,
    pub mapseqno: i32,
    pub mapdict: Oid,
}

seam_core::seam!(
    // Sorted (maptokentype, mapseqno) — the TSConfigMapIndexId scan order ts_cache relies on.
    pub fn pg_ts_config_map_shapes<'mcx>(
        mcx: mcx::Mcx<'mcx>,
        cfgid: Oid,
    ) -> PgResult<mcx::PgVec<'mcx, PgTsConfigMapShape>>
);

/// V4R-453 (D1409): a pg_type row as a gravity pool claim is armed with it: the fields lsyscache's type readers
/// (`get_typlenbyvalalign`, `get_element_type`, `get_array_type`, `getBaseTypeAndTypmod`) project, captured on the leader
/// (`capture_type_row`) when a hosted aggregate's arena is built. A pool thread has no catcache; its claim reads these.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ArmedTypeRow {
    pub oid: Oid,
    pub typlen: i16,
    pub typbyval: bool,
    pub typalign: i8,
    pub typstorage: i8,
    pub typtype: i8,
    pub typrelid: Oid,
    pub typsubscript: Oid,
    pub typelem: Oid,
    pub typarray: Oid,
    pub typcollation: Oid,
    pub typbasetype: Oid,
    pub typtypmod: i32,
    /// A range type's pg_range row (get_range_subtype / _collation / _multirange); a multirange's range type
    /// (get_multirange_range); None / 0 for any other type.
    pub range: Option<PgRangeShape>,
    pub multirange_range: Oid,
}

std::thread_local! {
    static ARMED_TYPE_ROWS: core::cell::Cell<&'static [ArmedTypeRow]> = const { core::cell::Cell::new(&[]) };
    /// B2 (workersess): the statement's rows a claim is armed with for its hosted scalar sites, read after the arena's.
    static STATEMENT_TYPE_ROWS: core::cell::Cell<&'static [ArmedTypeRow]> = const { core::cell::Cell::new(&[]) };
}

/// B2: arm this thread's claim with the statement's rows (an empty slice disarms; a plain store, safe in an unwind).
pub fn arm_statement_type_rows(rows: &'static [ArmedTypeRow]) {
    let _ = STATEMENT_TYPE_ROWS.try_with(|c| c.set(rows));
}

/// B2: whether `capture_type_row`'s seams are all installed (a backend; a lowering test binary may install only some).
pub fn type_rows_capturable() -> bool {
    lookup_pg_type_typcache_shape::is_installed() && pg_type_base_shape::is_installed() && lookup_pg_range_shape::is_installed() && lookup_pg_range_by_multirange::is_installed()
}

/// The leader's projection of type `oid` (None: no such type), through the same seams lsyscache and typcache read.
pub fn capture_type_row(oid: Oid) -> PgResult<Option<ArmedTypeRow>> {
    let (Some(t), Some(b)) = (lookup_pg_type_typcache_shape::call(oid)?, pg_type_base_shape::call(oid)?) else { return Ok(None) };
    Ok(Some(ArmedTypeRow {
        oid,
        typlen: t.typlen,
        typbyval: t.typbyval,
        typalign: t.typalign,
        typstorage: t.typstorage,
        typtype: t.typtype,
        typrelid: t.typrelid,
        typsubscript: t.typsubscript,
        typelem: t.typelem,
        typarray: t.typarray,
        typcollation: t.typcollation,
        typbasetype: b.typbasetype,
        typtypmod: b.typtypmod,
        range: if t.typtype == b'r' as i8 { lookup_pg_range_shape::call(oid)? } else { None },
        multirange_range: if t.typtype == b'm' as i8 { lookup_pg_range_by_multirange::call(oid)?.unwrap_or(0) } else { 0 },
    }))
}

/// Arm this thread with a claim's rows (an empty slice disarms). A plain thread-local store: no allocation, no panic, so
/// a claim guard's drop may call it during an unwind.
pub fn arm_type_rows(rows: &'static [ArmedTypeRow]) {
    let _ = ARMED_TYPE_ROWS.try_with(|c| c.set(rows));
}

/// The armed row of type `oid` on this thread, if a claim armed one (one thread-local read when none is armed).
#[inline]
pub fn armed_type_row(oid: Oid) -> Option<&'static ArmedTypeRow> {
    let rows = ARMED_TYPE_ROWS.try_with(core::cell::Cell::get).unwrap_or(&[]);
    let stmt = STATEMENT_TYPE_ROWS.try_with(core::cell::Cell::get).unwrap_or(&[]);
    if rows.is_empty() && stmt.is_empty() {
        return None;
    }
    rows.iter().find(|r| r.oid == oid).or_else(|| stmt.iter().find(|r| r.oid == oid))
}
