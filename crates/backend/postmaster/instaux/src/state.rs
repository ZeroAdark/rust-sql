use std::sync::atomic::Ordering::Relaxed;

use types_error::PgResult;

use crate::shared::SHARED;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Persisted {
    pub epoch: u64,
    pub fenced: bool,
    pub reason: String,
}

pub(crate) fn path() -> String {
    guc_tables::backing::pgrust_instance_aux_state_file().unwrap_or_default()
}

pub(crate) fn parse(text: &str) -> Persisted {
    let mut p = Persisted::default();
    for line in text.lines() {
        match line.split_once(' ') {
            Some(("epoch", v)) => p.epoch = v.trim().parse().unwrap_or(0),
            Some(("fenced", v)) => p.fenced = v.trim() == "1",
            Some(("reason", v)) => p.reason = v.to_string(),
            _ => {}
        }
    }
    p
}

pub(crate) fn render(p: &Persisted) -> String {
    let reason: String = p.reason.chars().filter(|c| !c.is_control()).collect();
    format!("epoch {}\nfenced {}\nreason {}\n", p.epoch, p.fenced as u8, reason)
}

pub(crate) fn current() -> Persisted {
    Persisted {
        epoch: SHARED.epoch.load(Relaxed),
        fenced: init_small::instaux::fenced(),
        reason: pgsync::lock(&SHARED.fence_reason).clone(),
    }
}

pub(crate) fn adopt(p: &Persisted) {
    SHARED.epoch.store(p.epoch, Relaxed);
    init_small::instaux::set_fenced(p.fenced);
    *pgsync::lock(&SHARED.fence_reason) = p.reason.clone();
}

pub(crate) fn load() -> Persisted {
    crate::files::read_all(&path()).map(|b| parse(&String::from_utf8_lossy(&b))).unwrap_or_default()
}

pub(crate) fn store(p: &Persisted) -> PgResult<()> {
    crate::files::write_durable(&path(), render(p).as_bytes())?;
    adopt(p);
    Ok(())
}
