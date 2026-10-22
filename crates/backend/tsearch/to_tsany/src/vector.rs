use ::adt_tsvector_core::layout::{shortalign, TsVecBuilder, MAXNUMPOS, MAXSTRLEN, MAXSTRPOS};
use ::mcx::{Mcx, PgVec};
use ::ts_parse::{limitpos, ParsedText, ParsedWord, MAXENTRYPOS};
use ::types_error::{PgError, PgResult, ERRCODE_PROGRAM_LIMIT_EXCEEDED};

fn compare_word(a: &ParsedWord<'_>, b: &ParsedWord<'_>) -> core::cmp::Ordering {
    a.word.cmp(b.word).then(a.pos.cmp(&b.pos))
}

// uniqueWORD: sort + merge duplicate lexemes, folding positions into apos.
pub fn unique_words<'mcx>(mcx: Mcx<'mcx>, prs: &mut ParsedText<'mcx>) -> PgResult<()> {
    if prs.words.is_empty() {
        return Ok(());
    }
    let words = &mut prs.words;
    words.sort_unstable_by(compare_word);

    let mut apos: PgVec<'mcx, u16> = PgVec::new_in(mcx);
    let mut res = 0usize;
    for i in 0..words.len() {
        let w = words[i];
        let pos = limitpos(w.pos as u32);
        if res > 0 && words[res - 1].word == w.word {
            let last = *apos.last().expect("merged word has positions");
            if apos.len() < MAXNUMPOS - 1 && last != (MAXENTRYPOS - 1) as u16 && last != pos {
                apos.push(pos);
            }
        } else {
            if res > 0 {
                words[res - 1].apos = core::mem::replace(&mut apos, PgVec::new_in(mcx)).leak();
            }
            apos.push(pos);
            words[res] = w;
            res += 1;
        }
    }
    words[res - 1].apos = apos.leak();
    words.truncate(res);
    Ok(())
}

// make_tsvector: dedup + flat image build (4-byte zero header for stamping).
pub fn make_tsvector<'mcx>(mcx: Mcx<'mcx>, prs: &mut ParsedText<'mcx>) -> PgResult<PgVec<'mcx, u8>> {
    unique_words(mcx, prs)?;

    let mut lenstr = 0usize;
    for w in prs.words.iter() {
        // upstream e251350573e2 (18.6): Harden tsvector code against overflows.
        let toklen = w.word.len();
        if toklen == 0 || toklen > MAXSTRLEN {
            return Err(PgError::error(format!(
                "lexeme is too long for tsvector ({toklen} bytes, max {MAXSTRLEN} bytes)"
            ))
            .with_sqlstate(ERRCODE_PROGRAM_LIMIT_EXCEEDED)
            .into());
        }
        lenstr += toklen;
        if !w.apos.is_empty() {
            lenstr = shortalign(lenstr);
            lenstr += 2 + w.apos.len() * 2;
        }
    }
    if lenstr > MAXSTRPOS {
        return Err(PgError::error(format!(
            "string is too long for tsvector ({lenstr} bytes, max {MAXSTRPOS} bytes)"
        ))
        .with_sqlstate(ERRCODE_PROGRAM_LIMIT_EXCEEDED)
        .into());
    }

    let mut b = TsVecBuilder::with_capacity(mcx, prs.words.len(), lenstr)?;
    for w in prs.words.iter() {
        b.push(w.word, w.apos)?;
    }
    b.finish(mcx)
}
