//! Cron schedules: the parsed form of `cron.job.schedule` and the

pub mod entry;
#[cfg(test)]
mod tests;

pub use entry::parse_cron_entry;

/// Field bounds (cron.h FIRST_*/LAST_*).
pub const FIRST_MINUTE: i32 = 0;
pub const LAST_MINUTE: i32 = 59;
pub const FIRST_HOUR: i32 = 0;
pub const LAST_HOUR: i32 = 23;
pub const FIRST_DOM: i32 = 1;
pub const LAST_DOM: i32 = 31;
pub const FIRST_MONTH: i32 = 1;
pub const LAST_MONTH: i32 = 12;
pub const FIRST_DOW: i32 = 0;
pub const LAST_DOW: i32 = 7;
pub const MINUTE_COUNT: i64 = 60;

/// `entry.flags` bits (cron.h).
pub const DOM_STAR: u8 = 0x01;
pub const DOW_STAR: u8 = 0x02;
pub const WHEN_REBOOT: u8 = 0x04;
pub const MIN_STAR: u8 = 0x08;
pub const HR_STAR: u8 = 0x10;
pub const DOM_LAST: u8 = 0x20;

/// One parsed schedule (cron.h `entry`, minus the Vixie fields pg_cron never
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub minute: u64,
    pub hour: u64,
    pub dom: u64,
    pub month: u64,
    pub dow: u64,
    pub flags: u8,
    /// pg_cron extension: `N second[s]` schedules. Non-zero means the cron
    pub seconds_interval: u32,
}

/// The broken-down local time ShouldRunTask tests against (pg_tm fields,
#[derive(Clone, Copy, Debug)]
pub struct LocalTime {
    pub minute: i32,
    pub hour: i32,
    pub day_of_month: i32,
    pub month: i32,
    pub day_of_week: i32,
}

#[inline]
fn bit(bits: u64, i: i32) -> bool {
    (0..64).contains(&i) && (bits >> i) & 1 == 1
}

impl Entry {
    pub fn is_interval(&self) -> bool {
        self.seconds_interval > 0
    }

    pub fn is_at_restart(&self) -> bool {
        self.flags & WHEN_REBOOT != 0
    }

    /// `ShouldRunTask` (pg_cron.c:997) minus the clock: does this schedule
    pub fn matches(
        &self,
        now: LocalTime,
        is_lastdom: bool,
        dom_dow_and: bool,
        do_wild: bool,
        do_nonwild: bool,
    ) -> bool {
        let thisdom = bit(self.dom, now.day_of_month - FIRST_DOM)
            || (is_lastdom && self.flags & DOM_LAST != 0);
        let thisdow = bit(self.dow, now.day_of_week - FIRST_DOW);
        let day_ok = if dom_dow_and || self.flags & (DOM_STAR | DOW_STAR) != 0 {
            thisdom && thisdow
        } else {
            thisdom || thisdow
        };
        if !(bit(self.minute, now.minute - FIRST_MINUTE)
            && bit(self.hour, now.hour - FIRST_HOUR)
            && bit(self.month, now.month - FIRST_MONTH)
            && day_ok)
        {
            return false;
        }
        let wild = self.flags & (MIN_STAR | HR_STAR) != 0;
        (do_nonwild && !wild) || (do_wild && wild)
    }
}

/// `ParseSchedule` (job_metadata.c:1525): a crontab line first, then the
pub fn parse_schedule(text: &str) -> Option<Entry> {
    if let Some(e) = parse_cron_entry(text) {
        return Some(e);
    }
    try_parse_interval(text).map(|secs| Entry { seconds_interval: secs, ..Entry::default() })
}

/// `TryParseInterval` (job_metadata.c:1560): sscanf(" %u secon%c%c %c")
pub fn try_parse_interval(text: &str) -> Option<u32> {
    let lower = text.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut i = 0;
    let ws = |c: u8| c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' || c == 0x0b || c == 0x0c;
    while i < b.len() && ws(b[i]) {
        i += 1;
    }
    let mut negative = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        negative = b[i] == b'-';
        i += 1;
    }
    let start = i;
    let mut value: u64 = 0;
    let mut overflow = false;
    while i < b.len() && b[i].is_ascii_digit() {
        value = value.saturating_mul(10).saturating_add((b[i] - b'0') as u64);
        overflow |= value > u32::MAX as u64;
        i += 1;
    }
    if i == start {
        return None;
    }
    while i < b.len() && ws(b[i]) {
        i += 1;
    }
    if !b[i..].starts_with(b"secon") {
        return None;
    }
    i += 5;
    if b.get(i) != Some(&b'd') {
        return None;
    }
    i += 1;
    let plural = b.get(i).copied();
    let num_parts = match plural {
        None => 2,
        Some(_) => {
            i += 1;
            while i < b.len() && ws(b[i]) {
                i += 1;
            }
            if i < b.len() { 4 } else { 3 }
        }
    };
    let in_range = !negative && !overflow && (1..60).contains(&(value as u32));
    match num_parts {
        2 => in_range.then_some(value as u32),
        3 if plural == Some(b's') => in_range.then_some(value as u32),
        _ => None,
    }
}
