
#[derive(Clone, Copy, Debug)]
pub struct BootTypeIoData {
    pub typlen: i16,
    pub typbyval: bool,
    pub typalign: i8,
    pub typdelim: i8,
    pub typioparam: types_core::Oid,
    pub typinput: types_core::Oid,
    pub typoutput: types_core::Oid,
}

seam_core::seam!(
    pub fn boot_get_type_io_data(typid: types_core::Oid) -> types_error::PgResult<BootTypeIoData>
);
