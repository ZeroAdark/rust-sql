//! stringinfo.c's growth model for the SQL/XML mapping builders: the error
//! from enlargeStringInfo fires at the same append, with the same DETAIL, as C.

use core::fmt::{self, Write};

use ::types_error::{PgError, PgResult, ERRCODE_OUT_OF_MEMORY};

const STRINGINFO_DEFAULT_SIZE: usize = 1024;

/// The `maxlen` C would hold for a buffer of the same history.
#[derive(Clone, Copy, Debug)]
pub struct StringInfoGrowth {
    maxlen: usize,
    limit: usize,
}

impl Default for StringInfoGrowth {
    fn default() -> Self {
        Self::with_limit(::mcx::MAX_ALLOC_SIZE)
    }
}

impl StringInfoGrowth {
    pub fn with_limit(limit: usize) -> Self {
        StringInfoGrowth { maxlen: STRINGINFO_DEFAULT_SIZE.min(limit), limit }
    }

    pub fn maxlen(&self) -> usize {
        self.maxlen
    }

    /// `enlargeStringInfo(str, needed)` for a buffer holding `len` bytes.
    pub fn enlarge(&mut self, len: usize, needed: usize) -> PgResult<()> {
        if needed >= self.limit.saturating_sub(len) {
            return Err(crate::stringinfo_limit_error(len, needed));
        }
        let needed = needed + len + 1;
        if needed <= self.maxlen {
            return Ok(());
        }
        let mut newlen = 2 * self.maxlen;
        while needed > newlen {
            newlen *= 2;
        }
        self.maxlen = newlen.min(self.limit);
        Ok(())
    }

    /// appendBinaryStringInfo / appendStringInfoString.
    pub fn before_append(&mut self, len: usize, datalen: usize) -> PgResult<()> {
        self.enlarge(len, datalen)
    }

    /// appendStringInfoChar.
    pub fn before_char(&mut self, len: usize) -> PgResult<()> {
        if len + 1 >= self.maxlen {
            self.enlarge(len, 1)?;
        }
        Ok(())
    }

    /// appendStringInfo's retry loop over appendStringInfoVA / pvsnprintf for
    /// a formatted piece of `nprinted` bytes.
    pub fn before_format(&mut self, len: usize, nprinted: usize) -> PgResult<()> {
        loop {
            let avail = self.maxlen - len;
            if avail < 16 {
                self.enlarge(len, 32)?;
                continue;
            }
            if nprinted < avail {
                return Ok(());
            }
            if nprinted > self.limit - 1 {
                return Err(Box::new(PgError::error("out of memory").with_sqlstate(ERRCODE_OUT_OF_MEMORY)));
            }
            self.enlarge(len, nprinted + 1)?;
        }
    }
}

struct Count(usize);

impl Write for Count {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0 += s.len();
        Ok(())
    }
}

/// A String-backed `StringInfo` whose capacity follows C's `maxlen`.
#[derive(Default)]
pub struct StringInfo {
    data: String,
    growth: StringInfoGrowth,
}

impl StringInfo {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limit(limit: usize) -> Self {
        StringInfo { data: String::new(), growth: StringInfoGrowth::with_limit(limit) }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.data
    }

    pub fn into_string(self) -> String {
        self.data
    }

    fn grow_to_maxlen(&mut self) -> PgResult<()> {
        let want = self.growth.maxlen().saturating_sub(self.data.len() + 1);
        if self.data.capacity() - self.data.len() < want {
            self.data
                .try_reserve_exact(want)
                .map_err(|_| ::mcx::oom_named("StringInfo", self.growth.maxlen()))?;
        }
        Ok(())
    }

    pub fn append_str(&mut self, s: &str) -> PgResult<()> {
        self.growth.before_append(self.data.len(), s.len())?;
        self.grow_to_maxlen()?;
        self.data.push_str(s);
        Ok(())
    }

    pub fn append_char(&mut self, c: char) -> PgResult<()> {
        debug_assert!(c.is_ascii());
        self.growth.before_char(self.data.len())?;
        self.grow_to_maxlen()?;
        self.data.push(c);
        Ok(())
    }

    pub fn append_fmt(&mut self, args: fmt::Arguments<'_>) -> PgResult<()> {
        let mut n = Count(0);
        let _ = n.write_fmt(args);
        self.growth.before_format(self.data.len(), n.0)?;
        self.grow_to_maxlen()?;
        let _ = self.data.write_fmt(args);
        Ok(())
    }
}
