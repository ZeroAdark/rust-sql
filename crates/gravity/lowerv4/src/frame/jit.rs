//! The JIT map's tags (gravity_ir `JitTags`, gravity_backend perfmap mode `tags`): each range of a pipeline function named
//! `<body>.<part>[.<mechanism>]`. The body is the form the code runs in (`fn` the function's own code, `row` the row loop,
//! `block` the 64-row block path, `flush` the held rows' flush, `copy` a stored row's entry); the part a step `s<j>`, the
//! end `end` or a phase of the function (`src.open`, `window`, `exit`); the mechanism its type's last two path segments.
//! Off (`FunctionBuilder::jit_on` false) one branch per call, no name built.

use core::fmt;
use gravity_ir::builder::FunctionBuilder;

/// The instructions added from here on carry `<body>.<part>[.<mech>]`; returns the tag before (`restore_tag`).
#[inline]
pub(crate) fn tag(b: &mut FunctionBuilder<'_>, body: &str, part: fmt::Arguments<'_>, mech: &'static str) -> u32 {
    if !b.jit_on() {
        return 0;
    }
    let dot = if mech.is_empty() { "" } else { "." };
    b.set_tag(format_args!("{body}.{part}{dot}{}", Short(mech)))
}

/// A `type_name` without its generic arguments: its last two path segments, joined by '.'.
struct Short(&'static str);

impl fmt::Display for Short {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let base = self.0.split('<').next().unwrap_or(self.0);
        let mut segs = base.rsplit("::");
        let last = segs.next().unwrap_or(base);
        match segs.next() {
            Some(prev) => write!(f, "{prev}.{last}"),
            None => f.write_str(last),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn short_names() {
        assert_eq!(super::Short("gravity_lowerv4::step::filter::Filter<'g, X<Y>>").to_string(), "filter.Filter");
        assert_eq!(super::Short("Plain").to_string(), "Plain");
        assert_eq!(super::Short("").to_string(), "");
    }
}
