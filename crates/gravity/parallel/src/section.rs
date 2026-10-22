use gravity_program::{End, Parallel, Program};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section {
    pub pipeline: usize,
    pub width: u16,
    pub driving_site: i16,
    pub relation: u32,
    /// The kernel is called once per ordinal with `SECTION_END` after the claims (item 20).
    pub finish: bool,
}

impl Section {
    pub fn plan(program: &Program, pipeline: usize, granted: u16, sink_has_legs: bool) -> Option<Section> {
        let p = program.pipelines[pipeline];
        if granted < 2 || p.parallel != Parallel::Eligible || p.driving_site < 0 {
            return None;
        }
        // D2998: an EXISTS InitPlan's pipeline ends in its cell, not the sink: no leg takes a row of it
        if p.ends_in == End::Sink && !sink_has_legs && !gravity_driver::exists_cell(program, pipeline) {
            return None;
        }
        Some(Section { pipeline, width: granted, driving_site: p.driving_site, relation: program.sites[p.driving_site as usize].relation, finish: p.finish })
    }
}
