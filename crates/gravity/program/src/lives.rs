//! Breaker lives (lowerv3 design §2.3, LEDGER rev 2 §4.2): one life per producing pipeline, named by its index; every consumer records the life it reads.
use gravity_abi::breaker::Budget;

use crate::program::{Pipeline, Program};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LifeId(pub u16);

/// What a life's records draw: a share of the statement's grant, or a fixed program-constant size.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LifeBytes {
    Share,
    Fixed(u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Life {
    pub id: LifeId,
    pub slot: u8,
    pub producer: usize,
    pub last_consumer: usize,
    pub parallel: bool,
    pub bytes: LifeBytes,
}

/// Whether pipeline `k` reads `life` (its declared `consumes_from`; a pipeline without it reads the nearest earlier producer of the slot).
pub fn reads(ps: &[Pipeline], k: usize, life: LifeId) -> bool {
    let producer = life.0 as usize;
    let Some(slot) = ps.get(producer).and_then(|p| p.produces()) else { return false };
    let p = &ps[k];
    match p.consumes.iter().position(|&s| s == slot) {
        Some(j) if !p.consumes_from.is_empty() => p.consumes_from[j] == life,
        Some(_) => k > producer && (producer + 1..k).all(|m| ps[m].produces() != Some(slot)),
        None => false,
    }
}

impl Program {
    /// Every breaker life in producer order.
    pub fn lives(&self) -> impl Iterator<Item = Life> + '_ {
        self.pipelines.iter().enumerate().filter_map(move |(i, p)| {
            let b = p.breaker()?;
            let id = LifeId(i as u16);
            let last_consumer = self.consumers_of(id).last().unwrap_or(i);
            let bytes = match b.budget {
                Budget::Share | Budget::Pool => LifeBytes::Share,
                Budget::Fixed(n) => LifeBytes::Fixed(n),
            };
            Some(Life { id, slot: b.slot, producer: i, last_consumer, parallel: p.is_eligible(), bytes })
        })
    }

    pub fn consumers_of(&self, life: LifeId) -> impl Iterator<Item = usize> + '_ {
        (life.0 as usize + 1..self.pipelines.len()).filter(move |&k| reads(&self.pipelines, k, life))
    }

    /// The life pipeline `k` reads through slot `slot`.
    pub fn life_of(&self, k: usize, slot: u8) -> Option<LifeId> {
        let p = &self.pipelines[k];
        let j = p.consumes.iter().position(|&s| s == slot)?;
        match p.consumes_from.get(j) {
            Some(&l) => Some(l),
            None => self.producer_of_slot(k, slot).map(|i| LifeId(i as u16)),
        }
    }
}
