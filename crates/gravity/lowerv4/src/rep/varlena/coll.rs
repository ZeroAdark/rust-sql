//! Road `key.coll` (H21, G06: collation images; L3 straight: C bytes + the comparator, L6 the ICU prefix word)
//! (cards/M6.md, M6-changes.md §1, M6-plan §3.1). Owner M6-TEXT.
//! Straight form (M6 Decision 2). Hash: a collation whose equal bytes are its equal values (`KeyColl.bytes_eq`: C, POSIX,
//! every libc and deterministic ICU one) declines, so rep.varlena.hash takes the payload's exact Fp128; a nondeterministic
//! one is the Fp128 of the provider's sort key (`text_strnxfrm`, PG's `hashtext` under such a collation: two strings the
//! collation calls equal have one sort key), exact. Order: a bytes-ordered collation declines (rep.varlena.order's bytes
//! prefix, TailForm::Bytes); an ICU one (`pg_strnxfrm_prefix_enabled`) is the comparator form over its sort key's first 8
//! bytes (`m6_coll_prefix`, PostgreSQL's abbreviated key, D1582), the comparator deciding equal words; every other (libc)
//! is the comparator form, prefix 0 (Decision 5: never strxfrm for libc).
use gravity_decompose::catalog::typefacts::{KeyColl, TypeFacts};
use gravity_ir::{HostFlags, Type, Value};
use gravity_kernels::host_abi::text_strnxfrm;
use gravity_kernels::ops::agg::trans::entries::m6_coll_prefix;

use crate::ops::Table;
use crate::rep::{self, CellRef, KeyWord, Lx, Scope, Words};

use crate::rep::varkey::{TailForm, VarKeyImpl, VarKeyMech};
use crate::rep::varlena::hash::{out2, payload, ptr, raise_on, release};

crate::strategies! {
    /// Collation sort-key images (strnxfrm).
    KEY_COLL = 83, "key.coll", VarlenaKey;
}

pub(crate) const ROWS: Table = Table { strategies: &[(KEY_COLL, &IMPL)], ..crate::ops::NONE };

const IMPL: VarKeyImpl = VarKeyImpl(&Coll);

struct Coll;

impl VarKeyMech for Coll {
    fn hash(&self, lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, coll: KeyColl, _: Scope) -> Option<Words> {
        if coll.bytes_eq {
            return None;
        }
        let (p, null) = rep::datum(lx, c, tf);
        let zero = lx.b.iconst(Type::I64, 0);
        let (l_val, l_out) = (lx.b.create_block(&[]), lx.b.create_block(&[Type::I64, Type::I64]));
        lx.b.cond_br(null, (l_out, &[zero, zero]), (l_val, &[]));
        lx.b.switch_to(l_val);
        let p = ptr(lx.b, p);
        let (bytes, len, mark) = payload(lx, p, tf);
        let (out, oid) = (out2(lx.b), lx.b.iconst(Type::I64, coll.oid as i64));
        let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
        let ctx = lx.frame().ctx;
        let f = lx.b.declare_host_once("text_strnxfrm", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::Ptr], Some(Type::I64), text_strnxfrm as *const () as usize, flags);
        let st = lx.b.call_host(f, &[ctx, bytes, len, oid, out], Some(Type::I64)).expect("text_strnxfrm returns its status");
        raise_on(lx, st);
        let (lo, hi) = (lx.b.load(Type::I64, out, 0), lx.b.load(Type::I64, out, 8));
        release(lx, mark);
        lx.b.br(l_out, &[lo, hi]);
        lx.b.switch_to(l_out);
        let o = lx.b.block_params(l_out);
        Some(Words { w: [Some(o[0]), Some(o[1])], exact: true })
    }

    fn order(&self, lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, coll: KeyColl) -> Option<(Value, TailForm)> {
        // bytes order: rep.varlena.order's bytes prefix (declined here); so is an equality word under a collation whose
        // equal values are its equal bytes (`KeyWord::Equal`: equal values, equal prefixes; a changed prefix, a changed value)
        if coll.bytes_order || (lx.key_word == KeyWord::Equal && coll.bytes_eq) {
            return None;
        }
        let tail = TailForm::Collated { coll: coll.oid };
        // D1582: an ICU collation's word is its sort key's first 8 bytes (`m6_coll_prefix`, PostgreSQL's abbreviated key);
        // any other (libc: Decision 5, never a strxfrm) is 0, every compare the comparator's. A function without a raise
        // block keeps the 0 word (never a refusal), and so does a bounded top's run (`KeyWord::Plain`: PostgreSQL's bounded
        // sort takes no abbreviated key).
        if lx.key_word == KeyWord::Plain || lx.raise.is_none() || !gravity_fn::typesem::collation::prefix_enabled(coll.oid) {
            return Some((lx.b.iconst(Type::I64, 0), tail));
        }
        let (p, null) = rep::datum(lx, c, tf);
        let zero = lx.b.iconst(Type::I64, 0);
        let (l_val, l_out) = (lx.b.create_block(&[]), lx.b.create_block(&[Type::I64]));
        lx.b.cond_br(null, (l_out, &[zero]), (l_val, &[]));
        lx.b.switch_to(l_val);
        let p = ptr(lx.b, p);
        let (bytes, len, mark) = payload(lx, p, tf);
        let (out, oid) = (out2(lx.b), lx.b.iconst(Type::I64, coll.oid as i64));
        let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
        let f = lx.b.declare_host_once("m6_coll_prefix", &[Type::Ptr, Type::I64, Type::I64, Type::Ptr], Some(Type::I64), m6_coll_prefix as *const () as usize, flags);
        let st = lx.b.call_host(f, &[bytes, len, oid, out], Some(Type::I64)).expect("m6_coll_prefix returns its status");
        raise_on(lx, st);
        let w = lx.b.load(Type::I64, out, 0);
        release(lx, mark);
        lx.b.br(l_out, &[w]);
        lx.b.switch_to(l_out);
        Some((lx.b.block_params(l_out)[0], tail))
    }
}
