//! The key-image home (C5 (b), reports/BUG-RECURRENCE-20261011.md §3; lint RX): how a key's words are built from its
//! value, decided here once and read by the producer (a scan's lane, a packed pgrc lane, a hashed emit) and the
//! consumer (a group, a set operation, a join). The plan-time half of keysem: decompose reads no gravity_fn, so the
//! image types and the decisions over the plan-time catalog (`Facts`, `TypeFacts`) live here, beside the key answers in
//! keysem/mod.rs; gravity_fn::typesem is the catalog-reading half the host answers `Facts` from. lowerv4 and hosts read
//! the decisions through this module.
//!
//! Moved here unchanged (C5 (b) step 1): `WordImage` / `FixedNorm` and their methods (catalog/typefacts.rs), `KeyImage`
//! (ops/shared/sort.rs) and `KeyImage::of` (ops/agg/mod.rs), `key_of` and `opaque`'s image (typefacts.rs),
//! `order_image` (ops/sort/mod.rs), a join key pair's image (`pair`, `images_pair`, `one_integer`, `same_rep`,
//! `name_text`: join.rs) and `KeyEq` (catalog/mod.rs). The old paths re-export them.
use gravity_abi::breaker::KeyKind;
use gravity_program::NumTypmod;
use types_core::eq_alike::EqAlike;

use crate::catalog::typefacts::{self, KeyColl, TypeFacts};
use crate::catalog::Facts;
use crate::join::{JoinDict, JoinKey, Side};
use crate::ops::shared::agg::KeyDecl;
use crate::ops::shared::sort::NO_SCALE;

/// D2163 (`Facts::key_eq`): what a grouping key compares by under its plan's equality operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEq {
    /// The key type's own `=`: its canonical image keys it.
    Own,
    /// `byteaeq` over a varlena (a user family of byte comparators): its payload bytes key it.
    Bytes,
    /// Any other built-in procedure (int4ne under =!): no image stands for it.
    Other,
    /// User code, or no catalog to tell (a fixture): what it compares is not known here; a key over it keeps the form it
    /// had before D2640 (D1901 refuses a multirange one).
    User,
}

/// How a key / bucket image is built from a type's bytes: a closed set a new type picks from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WordImage {
    Raw { signed: bool },
    /// The float's bits with -0 folded to +0 and every NaN to one pattern.
    Float { f32: bool },
    /// The bytes' fingerprint; `text` for collatable strings (a user collation re-reads it), bpchar trims trailing spaces.
    Bytes { text: bool, bpchar_trim: bool },
    Bits,
    Fixed { norm: FixedNorm },
    /// The i64 mantissa at the cell's scale when its `NumTypmod` are narrow, else `NumNormalized` (D197).
    NumMantissa,
    NumNormalized,
    Enum,
    /// The fingerprint of the type's canonical image (its hash opclass), a collision resolved by `eq`: the one lossy image.
    Canonical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FixedNorm {
    Raw,
    Interval,
    Timetz,
}

impl WordImage {
    /// The image decides equality by itself (a full value word or a full-strength fp128): no exact check on a hit.
    pub const fn equality(self) -> bool {
        !matches!(self, WordImage::Canonical)
    }

    /// How equal values of a type with this image print (D2865: `types_core::eq_alike`, the rule the row executor's
    /// hashed DISTINCT reads too); the test `eq_alike_is_the_shared_table` pins every row against its table.
    pub const fn eq_alike(self) -> EqAlike {
        match self {
            WordImage::Raw { .. } | WordImage::Bytes { text: false, .. } | WordImage::Bits | WordImage::Enum => EqAlike::Image,
            WordImage::Fixed { norm: FixedNorm::Raw | FixedNorm::Timetz } => EqAlike::Image,
            WordImage::Fixed { .. } | WordImage::Canonical => EqAlike::Apart,
            WordImage::Bytes { text: true, bpchar_trim } => EqAlike::Text { pad: bpchar_trim },
            WordImage::NumMantissa | WordImage::NumNormalized => EqAlike::Numeric,
            WordImage::Float { .. } => EqAlike::Float,
        }
    }

    /// Whether the type's comparison reads its values' stored bytes as they are (OID-19, D767): false only for a
    /// pad-insensitive image, whose comparison trims trailing blanks first (bpcharcmp after bcTruelen), so the stored
    /// bytes do not order its values ('a  ' sorts after 'a' || chr(9) by bytes, before it by the type's `<`). A dictionary
    /// code is the in-part rank of the stored bytes, so only such a type's extremes may fold by code (decompose's
    /// `code_arg`, class.minmax_code); the other folds them through the type's own comparison (class.text).
    pub const fn orders_as_stored(self) -> bool {
        !matches!(self, WordImage::Bytes { bpchar_trim: true, .. })
    }

    /// The datum's own bits (a by-value integer-like type, a date, a timestamp, an oid; signed or not).
    pub const fn is_word(self) -> bool {
        matches!(self, WordImage::Raw { .. })
    }

    /// The datum's own bits, a sign-extended integer (int2 / int4 / int8, date, time, timestamp, money).
    pub const fn is_signed_word(self) -> bool {
        matches!(self, WordImage::Raw { signed: true })
    }

    /// The datum's own bits, unsigned (bool, char, oid and the reg* types, xid, tid, pg_lsn, macaddr).
    pub const fn is_unsigned_word(self) -> bool {
        matches!(self, WordImage::Raw { signed: false })
    }

    /// An enum: equal by its oid word, ordered by `pg_enum.enumsortorder`.
    pub const fn is_enum(self) -> bool {
        matches!(self, WordImage::Enum)
    }

    /// A float's folded bits.
    pub const fn is_float(self) -> bool {
        matches!(self, WordImage::Float { .. })
    }

    /// A float's width: `Some(true)` for float4, `Some(false)` for float8, `None` for any other image.
    pub const fn float32(self) -> Option<bool> {
        match self {
            WordImage::Float { f32 } => Some(f32),
            _ => None,
        }
    }

    /// A collatable string's bytes (text, varchar, name, bpchar).
    pub const fn is_text(self) -> bool {
        matches!(self, WordImage::Bytes { text: true, .. })
    }

    /// A collatable string's trimming: `Some(true)` for bpchar (trailing blanks trimmed), `Some(false)` for the other
    /// strings, `None` for any other image.
    pub const fn text_trim(self) -> Option<bool> {
        match self {
            WordImage::Bytes { text: true, bpchar_trim } => Some(bpchar_trim),
            _ => None,
        }
    }

    /// A byte string's bytes as stored, untrimmed (text, varchar, name, bytea, inet / cidr; not bpchar).
    pub const fn untrimmed_bytes(self) -> bool {
        matches!(self, WordImage::Bytes { bpchar_trim: false, .. })
    }

    /// The numeric type's class (a mantissa at a narrow scale, else the normalised digits: `key_form`).
    pub const fn is_numeric(self) -> bool {
        matches!(self, WordImage::NumMantissa)
    }

    /// A numeric's image in either form: its mantissa or its normalised digits.
    pub const fn is_num_form(self) -> bool {
        matches!(self, WordImage::NumMantissa | WordImage::NumNormalized)
    }

    /// The datum's own bits: `Some(signed)` (a sign-extended integer, else unsigned), `None` for any other image.
    pub const fn int_signed(self) -> Option<bool> {
        match self {
            WordImage::Raw { signed } => Some(signed),
            _ => None,
        }
    }

    /// A byte string: `Some((text, bpchar_trim))` (a collatable string, bpchar's trimmed), `None` for any other image.
    pub const fn bytes_form(self) -> Option<(bool, bool)> {
        match self {
            WordImage::Bytes { text, bpchar_trim } => Some((text, bpchar_trim)),
            _ => None,
        }
    }

    /// A byte string's bytes (a collatable string, bytea, inet / cidr).
    pub const fn is_bytes(self) -> bool {
        matches!(self, WordImage::Bytes { .. })
    }

    /// bpchar's image: a collatable string whose trailing blanks are trimmed.
    pub const fn is_bpchar(self) -> bool {
        matches!(self, WordImage::Bytes { bpchar_trim: true, .. })
    }

    /// A collatable string compared by all its bytes (text, varchar, name; not bpchar).
    pub const fn is_plain_text(self) -> bool {
        matches!(self, WordImage::Bytes { text: true, bpchar_trim: false })
    }

    /// A bit string's canonical image.
    pub const fn is_bits(self) -> bool {
        matches!(self, WordImage::Bits)
    }

    /// A fixed-width by-reference type's normal form (uuid's bytes, an interval's span, a timetz's (time, zone)), `None`
    /// for any other image.
    pub const fn fixed_norm(self) -> Option<FixedNorm> {
        match self {
            WordImage::Fixed { norm } => Some(norm),
            _ => None,
        }
    }

    /// The type's canonical image, the one lossy image (arrays, records, ranges, jsonb, geometric and user types).
    pub const fn is_canonical(self) -> bool {
        matches!(self, WordImage::Canonical)
    }
}

/// A type the table does not list, from its storage alone (typefacts.rs `opaque`): enums and by-value types key as their
/// word, every other type by its canonical image.
pub const fn opaque(is_enum: bool, byval: bool, typlen: i16) -> (WordImage, KeyKind, u8) {
    match (is_enum, byval, typlen) {
        (true, _, _) => (WordImage::Enum, KeyKind::I32, 1),
        (false, true, 1..=4) => (WordImage::Raw { signed: false }, KeyKind::I32, 1),
        (false, true, _) => (WordImage::Raw { signed: false }, KeyKind::I64, 1),
        _ => (WordImage::Canonical, KeyKind::Fp128, 2),
    }
}

/// The record kind, image and words a key over this type takes: a numeric keys by mantissa only when its facts are narrow (D197).
pub fn key_of(tf: &TypeFacts, num: NumTypmod) -> (KeyKind, WordImage, u8) {
    match tf.image {
        WordImage::NumMantissa if num.narrow() => (KeyKind::I64, WordImage::NumMantissa, 1),
        WordImage::NumMantissa => (KeyKind::Fp128, WordImage::NumNormalized, 2),
        _ => (tf.kind, tf.image, tf.words),
    }
}

/// A key's record form over type facts `tf`: (kind, numeric scale, image, words). A numeric keys by its mantissa at the
/// producer's scale `num.scale` when `num` is narrow (D197), else by its normalised digits; a text-like key under a
/// collation `coll` whose equality is not byte equality takes `Collated`; every other type keys by its row.
pub fn key_form(tf: &TypeFacts, num: NumTypmod, coll: u32) -> (KeyKind, i16, KeyImage, u8) {
    let (kind, image, words) = key_of(tf, num);
    let scale = if image == WordImage::NumMantissa { num.scale } else { NO_SCALE };
    (kind, scale, KeyImage::of(image, coll), words)
}

/// An int4 key's record form (`key_form` over int4's row): a bounded-domain extract keys so (agg keys.rs `key_kind`).
pub const INT4_KEY: (KeyKind, i16, KeyImage, u8) = (KeyKind::I32, NO_SCALE, KeyImage::Raw, 1);

/// A sort key's kind and image from its type's catalog facts alone (no child to read a numeric scale through; V4R-84):
/// its row's record form under collation `coll` (an unknown type by reference), ordered as `order_image` says.
pub fn sort_key_form(facts: &dyn Facts, typid: u32, coll: u32) -> (KeyKind, KeyImage) {
    let (kind, image) = typefacts::facts_of(facts, typid).map_or((KeyKind::Fp128, KeyImage::ByRef), |r| {
        let (kind, image, _) = key_of(&r, NumTypmod::NONE);
        (kind, KeyImage::of(image, coll))
    });
    order_image(facts, typid, kind, image)
}

/// A type's own key image (`KeyImage::of(class, 0)`): its record image at the C collation, where a key's collation does
/// not change which arm family keys it (lowerv4's built-kind census, a DISTINCT ON's hash kind).
pub const fn class_image(class: WordImage) -> KeyImage {
    KeyImage::of(class, 0)
}

/// The family of emitters a key's image is built by, before any cell exists (lowerv4 rep `key_kind` maps it to its
/// datum / hash / order kind): a code key the code arm; a numeric image the packed arm over packed lanes (a wide packed
/// lane, and any unpacked numeric, orders through the hosted arm: D2426 / D771), else the varlena arm; a text / bytes
/// fingerprint the varlena arm; uuid / interval / timetz the fixed arm; any other fingerprint the hosted arm; a word the
/// word arm. `order`: the family that orders the key (else the one that hashes it).
pub const fn arm_family(kind: KeyKind, image: Option<KeyImage>, packed: bool, order: bool) -> ArmFamily {
    match (kind, image) {
        (KeyKind::Code, _) => ArmFamily::Code,
        (_, Some(KeyImage::NumNormalized)) if packed && order => ArmFamily::Hosted,
        (_, Some(KeyImage::NumMantissa | KeyImage::NumNormalized)) if packed => ArmFamily::Packed,
        (_, Some(KeyImage::NumMantissa | KeyImage::NumNormalized)) if order => ArmFamily::Hosted,
        (_, Some(KeyImage::NumMantissa | KeyImage::NumNormalized)) | (KeyKind::Fp128, Some(KeyImage::Text | KeyImage::Bpchar | KeyImage::Bytes | KeyImage::Collated)) => ArmFamily::Varlena,
        (_, Some(KeyImage::Uuid | KeyImage::IntervalNorm | KeyImage::Timetz)) => ArmFamily::Fixed,
        (KeyKind::Fp128, _) => ArmFamily::Hosted,
        _ => ArmFamily::Word,
    }
}

/// `arm_family`'s answer: the emitter families, in lowerv4 rep's `ARMS` row order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArmFamily {
    Word,
    Code,
    Packed,
    Varlena,
    Fixed,
    Hosted,
}

/// `Facts::key_eq`'s answer where no catalog tells (a fixture): user code, what it compares not known here.
pub const UNKNOWN_EQ: KeyEq = KeyEq::User;

/// `Facts::key_eq` from the key home's equality form (gravity_fn typesem `eq_form`): the type's own `=`, a byte
/// equality, user code; any other answer (another built-in, a failed catalog read) is `Other`, the refusal.
pub const fn key_eq_from(own: bool, bytes: bool, user: bool) -> KeyEq {
    if own {
        KeyEq::Own
    } else if bytes {
        KeyEq::Bytes
    } else if user {
        KeyEq::User
    } else {
        KeyEq::Other
    }
}

/// How a key's word(s) are built from the value (2d's per-row op; the record compares words bitwise).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyImage {
    /// The datum's own bits (by-value types, dates, timestamps, fixed-width by-reference types under 8 bytes).
    Raw,
    /// A float's bits with `-0` folded to `+0` and every NaN to one pattern (the type's equality).
    F32,
    F64,
    /// A numeric's i64 mantissa at `KeyDecl.scale`.
    NumMantissa,
    /// A numeric without a decided scale: the fingerprint of its normalised digits (trailing zeros stripped, the type's equality).
    NumNormalized,
    /// The 128-bit fingerprint of the text; `Bpchar` fingerprints the value with trailing spaces trimmed.
    Text,
    Bpchar,
    /// The fingerprint of the byte string (bytea; inet / cidr's family, bits and address) / the bit string's canonical image.
    Bytes,
    Bits,
    /// Two words: an interval's normalised i128 (`time + days × 86 400 s + months × 30 days`), a uuid's 16 bytes, a timetz's (time, zone).
    IntervalNorm,
    Uuid,
    Timetz,
    /// A text-like key under a user-defined collation (`KeyDecl.coll`): the bytes' fingerprint when `pg_collation` says
    /// deterministic, the collation sort key's otherwise (the lowerer reads the catalog; bpchar trims first).
    Collated,
    /// Any other type (arrays, composites, jsonb, ranges, geometric, extension types; decision 78): the 128-bit
    /// fingerprint of the datum's image under the type's equality (its hash opclass's canonical form), a collision
    /// resolved by the type's `=`; a by-value datum of an unlisted type is its own image.
    ByRef,
}

impl KeyImage {
    /// The datum's own bits.
    pub const fn is_raw(self) -> bool {
        matches!(self, KeyImage::Raw)
    }

    /// A text's fingerprint (under a collation whose equality is byte equality).
    pub const fn is_text(self) -> bool {
        matches!(self, KeyImage::Text)
    }

    /// A text-like key under a user collation (`KeyDecl.coll`).
    pub const fn is_collated(self) -> bool {
        matches!(self, KeyImage::Collated)
    }

    /// The fingerprint of the datum's canonical image under the type's equality (decision 78).
    pub const fn is_by_ref(self) -> bool {
        matches!(self, KeyImage::ByRef)
    }

    /// A word image the value does not read back from (M6 L0-m): a float's folded bits, a numeric's mantissa (its
    /// display scale), an interval's or timetz's normal form, a uuid's two words (M6-TYPES R1c). A group keeps one
    /// member's datum beside such a key.
    pub const fn word_not_datum(self) -> bool {
        matches!(self, KeyImage::F32 | KeyImage::F64 | KeyImage::NumMantissa | KeyImage::IntervalNorm | KeyImage::Uuid | KeyImage::Timetz)
    }

    /// The images a plan dump prints no note for: the datum's bits and a text's fingerprint.
    pub const fn dump_plain(self) -> bool {
        matches!(self, KeyImage::Raw | KeyImage::Text)
    }

    /// A numeric's i64 mantissa at `KeyDecl.scale`.
    pub const fn is_mantissa(self) -> bool {
        matches!(self, KeyImage::NumMantissa)
    }

    /// A numeric's normalised digits' fingerprint (no decided scale).
    pub const fn is_normalized(self) -> bool {
        matches!(self, KeyImage::NumNormalized)
    }

    /// A numeric's image in either form.
    pub const fn is_num(self) -> bool {
        matches!(self, KeyImage::NumMantissa | KeyImage::NumNormalized)
    }

    /// A float's folded bits.
    pub const fn is_float(self) -> bool {
        matches!(self, KeyImage::F32 | KeyImage::F64)
    }

    /// A byte string's fingerprint (bytea; inet / cidr).
    pub const fn is_bytes(self) -> bool {
        matches!(self, KeyImage::Bytes)
    }

    /// bpchar's fingerprint (trailing blanks trimmed).
    pub const fn is_bpchar(self) -> bool {
        matches!(self, KeyImage::Bpchar)
    }

    /// A text-like key compared by its string: a text's fingerprint or a collated one.
    pub const fn text_like(self) -> bool {
        matches!(self, KeyImage::Text | KeyImage::Collated)
    }

    /// A string key's fingerprint of any collatable string: text, bpchar or collated.
    pub const fn is_string(self) -> bool {
        matches!(self, KeyImage::Text | KeyImage::Bpchar | KeyImage::Collated)
    }

    /// A two-word normal form of a fixed-width type: uuid, interval, timetz.
    pub const fn is_fixed(self) -> bool {
        matches!(self, KeyImage::Uuid | KeyImage::IntervalNorm | KeyImage::Timetz)
    }

    /// The stored width of a fixed-width image's value: 16 bytes for uuid and interval, 12 for timetz; `None` otherwise.
    pub const fn fixed_len(self) -> Option<u16> {
        match self {
            KeyImage::Uuid | KeyImage::IntervalNorm => Some(16),
            KeyImage::Timetz => Some(12),
            _ => None,
        }
    }

    /// The value does not read back from the key's words (a group keeps a member's datum beside it): `word_not_datum`'s
    /// images and a numeric's normalised digits.
    pub const fn needs_datum(self) -> bool {
        self.word_not_datum() || matches!(self, KeyImage::NumNormalized)
    }

    /// Equal values of this key are one image and print alike (one member stands for its group): the datum's bits, a
    /// float's folded bits, a byte string's, a text's under a collation whose equality is byte equality (`bytes_eq`).
    /// D1520: a group row's member is then PG's first row's (lowerv4 ops/group/stream.rs, an ordered-set aggregate's
    /// direct argument over a group key).
    pub const fn members_alike(self, bytes_eq: bool) -> bool {
        matches!(self, KeyImage::Raw | KeyImage::F32 | KeyImage::F64 | KeyImage::Bytes) || (matches!(self, KeyImage::Text) && bytes_eq)
    }

    /// The record image of a type-facts image (`coll`: a text key under a user collation is `Collated`).
    pub const fn of(image: WordImage, coll: u32) -> KeyImage {
        match image {
            WordImage::Raw { .. } | WordImage::Enum => KeyImage::Raw,
            WordImage::Float { f32: true } => KeyImage::F32,
            WordImage::Float { f32: false } => KeyImage::F64,
            WordImage::Bytes { text: true, .. } if !KeyColl::of(coll).bytes_eq => KeyImage::Collated,
            WordImage::Bytes { text: true, bpchar_trim: true } => KeyImage::Bpchar,
            WordImage::Bytes { text: true, bpchar_trim: false } => KeyImage::Text,
            WordImage::Bytes { text: false, .. } => KeyImage::Bytes,
            WordImage::Bits => KeyImage::Bits,
            WordImage::Fixed { norm: FixedNorm::Raw } => KeyImage::Uuid,
            WordImage::Fixed { norm: FixedNorm::Interval } => KeyImage::IntervalNorm,
            WordImage::Fixed { norm: FixedNorm::Timetz } => KeyImage::Timetz,
            WordImage::NumMantissa => KeyImage::NumMantissa,
            WordImage::NumNormalized => KeyImage::NumNormalized,
            WordImage::Canonical => KeyImage::ByRef,
        }
    }
}

/// An ORDER key's kind and image: an enum orders by `pg_enum.enumsortorder` (C `enum_cmp`), never by its oid word, so it
/// takes the by-reference image, which orders through the type's own comparison (V4R-84); every other key as given.
pub fn order_image(facts: &dyn Facts, typid: u32, kind: KeyKind, image: KeyImage) -> (KeyKind, KeyImage) {
    match typefacts::facts_of(facts, typid) {
        Some(tf) if tf.image == WordImage::Enum => (KeyKind::Fp128, KeyImage::ByRef),
        _ => (kind, image),
    }
}

/// D3848: whether numeric key `k` is the scale-free form, the two-word Fp128 of its normalized value (`key_of`: no one
/// narrow form over its sources). A packed lane under it images through rep.packed.hash, the varlena Fp128 a heap numeric
/// of the same value has at any dscale, and its record keeps one member's datum.
pub fn scale_free(k: &KeyDecl<'_>) -> bool {
    k.image == KeyImage::NumNormalized && k.kind == KeyKind::Fp128 && k.words == 2
}

/// keyimage.typmod: key `k` as a keyed table holds it, given the record kind and words the table keeps for it (lowerv4
/// table/keyimg.rs `record_words`). A narrow numeric's one-word mantissa (at the scale its typmod names) held as two
/// Fp128 words (its cell a varlena: a heap column's lane, a computed numeric, D197; or D3857's packed set side) is
/// imaged as the Fp128 of its normalized digits, the image of a numeric without a scale (`NumNormalized`): the scale no
/// longer tells two sides apart, as PG's numeric_eq and hash_numeric ignore a scale's trailing zeros. Any other key is
/// held as decided.
pub fn held_form<'p>(k: &KeyDecl<'p>, (kind, words): (KeyKind, u8)) -> KeyDecl<'p> {
    match (kind, words) {
        (KeyKind::Fp128, 2) if k.image == KeyImage::NumMantissa => KeyDecl { image: KeyImage::NumNormalized, kind, words, scale: NO_SCALE, ..*k },
        _ => *k,
    }
}

/// D3857: whether two set sides hold one numeric column apart: the other side's held form (`held_form`) is a numeric
/// image, and the two differ in image, kind, scale or words.
pub fn held_apart(a: &KeyDecl<'_>, b: &KeyDecl<'_>) -> bool {
    matches!(b.image, KeyImage::NumMantissa | KeyImage::NumNormalized) && (a.image, a.kind, a.scale, a.words) != (b.image, b.kind, b.scale, b.words)
}

/// Why a key pair is not a set key, `None` when it is: its record image is its equality in whole words (D1168), the same on
/// both sides (image, kind, scale, words, column; the protocol compares the image words and the NULL flags). One word: a
/// by-value datum, a float's canonical bits, a numeric mantissa at one scale. Two words: the Fp128 of a text / bpchar /
/// bytea (a fingerprint is its equality) or of a text under a deterministic user collation (its bytes' Fp128),
/// and the exact 16-byte image of a uuid / interval / timetz. SETOP R1: the exact two-word Fp128 of a key with no native
/// image too (an array, a composite, jsonb, a range: rep.hosted.hash's canonical form), of a text under a nondeterministic
/// collation (its sort key's), of a numeric without a scale and of a bit string.
pub fn set_key(bytes_eq: &dyn Fn(u32) -> bool, l: &KeyDecl<'_>, r: &KeyDecl<'_>) -> Option<&'static str> {
    use KeyImage::*;
    if l.kind == KeyKind::Code || r.kind == KeyKind::Code {
        // a dictionary code is its own dictionary's: two sides' codes are not one domain
        return Some("a set operation over a dictionary code key (two sides' codes are not one domain)");
    }
    if (l.image, l.kind, l.scale, l.words, l.resno, l.coll) != (r.image, r.kind, r.scale, r.words, r.resno, r.coll) {
        return Some("a set operation whose sides' key images differ");
    }
    match (l.image, l.words) {
        (Raw | F32 | F64 | NumMantissa, 1) | (Text | Bpchar | Bytes | Uuid | IntervalNorm | Timetz, 2) => None,
        (Collated, 2) if bytes_eq(l.coll) => None,
        // V4R (SETOP R1): the exact Fp128 the side's group keys by (lowerv4 table/keyimg.rs `record_words`): rep.hosted.hash's
        // canonical form (ByRef; equal under the type's `=` <=> one image), key.coll's sort-key image (a nondeterministic
        // collation's text), rep.varlena.hash's normalized digits (NumNormalized) and the bit string's image (Bits)
        (Collated | ByRef | NumNormalized | Bits, 2) if l.kind == KeyKind::Fp128 => None,
        (Collated, _) => Some("a set operation over a text key under a nondeterministic collation (equal values differ in bytes)"),
        (ByRef, _) => Some("a set operation over a key whose image is a fingerprint the type's = resolves (arrays, composites, jsonb, ranges, ...)"),
        _ => Some("a set operation over a key whose image is not its equality in whole words (a numeric without a scale, a bit string)"),
    }
}

/// The types' rows unify; a numeric pair keys by mantissa at the larger scale when both sides are narrow and the
/// rescaled mantissa still fits 18 digits, else by normalised digits.
pub fn pair(facts: &dyn crate::catalog::Facts, outer: Side, inner: Side, coll: u32) -> JoinKey {
    let dict = if outer.code || inner.code { JoinDict::Fingerprint } else { JoinDict::None };
    let mut d = JoinKey { kind: None, image: None, scale: NO_SCALE, coll: facts.coll(coll), dict, types: (outer.typid.unwrap_or(0), inner.typid.unwrap_or(0)) };
    let (Some(to), Some(ti)) = (outer.typid.and_then(|t| typefacts::facts_of(facts, t)), inner.typid.and_then(|t| typefacts::facts_of(facts, t))) else { return d };
    if to.image == WordImage::NumMantissa && ti.image == WordImage::NumMantissa {
        let (no, ni) = (outer.num, inner.num);
        let s = no.scale.max(ni.scale);
        let digits = (no.precision as i32 - no.scale as i32).max(ni.precision as i32 - ni.scale as i32);
        let fits = no.narrow() && ni.narrow() && s as i32 + digits <= gravity_program::NUM_WORD_DIGITS as i32;
        (d.kind, d.image, d.scale) = if fits { (Some(KeyKind::I64), Some(WordImage::NumMantissa), s) } else { (Some(KeyKind::Fp128), Some(WordImage::NumNormalized), NO_SCALE) };
        return d;
    }
    d.kind = typefacts::unify(to.kind, ti.kind);
    d.image = d.kind.map(|k| if to.kind == k { to.image } else { ti.image });
    d
}

/// Key types `(a, b)` whose key images compare their values (lowerv4 hashjoin keys.rs `check_keys` keys them by image):
/// one type, two integer types (`one_integer`: their Word images are the value), two types of one representation and
/// equality (a binary-coercible relabel, regproc = oid, a domain = its base: `same_rep`), or a name beside a text
/// (`name_text`: the name keys as its text). Any other pair under a builtin hashable equality keys by each side's hash
/// value (D2725, `effects::user_hash_clause`).
pub fn images_pair(facts: &dyn crate::catalog::Facts, (a, b): (u32, u32), coll: KeyColl) -> bool {
    let rep = |t| typefacts::facts_of(facts, t);
    a == b || rep(a).zip(rep(b)).is_some_and(|(x, y)| one_integer(facts, &x, &y) || same_rep(&x, &y)) || name_text(facts, (a, b), coll)
}

/// OIDW-33: two types whose words are one integer value: each a sign-extended integer image (`Raw { signed }` at kind I32
/// or I64, a domain by its base's row), of one default btree family, whose cross-type `=` compares the values (int2 /
/// int4 / int8: integer_ops). A same-image type of another family (money, time) keys a different unit. A source that
/// reads no families (a plan capture, `Facts::default_family`) answers by the images alone; a cross-type clause the
/// server runs reaches a word key only under a builtin hashable equality (D2036 keys any other by its hash value).
pub fn one_integer(facts: &dyn crate::catalog::Facts, x: &typefacts::TypeFacts, y: &typefacts::TypeFacts) -> bool {
    let int = |t: &typefacts::TypeFacts| t.image == WordImage::Raw { signed: true } && matches!(t.kind, KeyKind::I32 | KeyKind::I64);
    int(x) && int(y) && facts.default_family(x.base) == facts.default_family(y.base)
}

/// Two types' catalog facts (`facts_of`: a domain carries its base's) name one representation and one equality: one base
/// type (a domain and its base, two domains over one base), or two known rows (an equality operator) that share length,
/// passing, image, key kind and eq / hash (a binary-coercible relabel: regproc = oid, varchar = text). Two types the table
/// does not know (eq 0: an enum, a user base type) pair only through one base: their equality is their own.
pub fn same_rep(x: &typefacts::TypeFacts, y: &typefacts::TypeFacts) -> bool {
    x.base == y.base || (x.eq != 0 && y.eq != 0 && (x.typlen, x.byval, x.image, x.kind, x.words, x.eq, x.hash) == (y.typlen, y.byval, y.image, y.kind, y.words, y.eq, y.hash))
}

/// Key pair `types` (outer, inner) is a name beside a text, either way round (lowerv4 hashjoin keys_name.rs): the name
/// side by its facts row's base, the text side by text's own row (its image and equality: text, varchar, a domain over
/// either), under an operator collation that is set (PostgreSQL raises on an unset one) and whose equal bytes are equal
/// values (a nondeterministic one compares by its sort key, which hashname does not read).
pub fn name_text(facts: &dyn crate::catalog::Facts, (a, b): (u32, u32), coll: KeyColl) -> bool {
    use types_core::catalog::{NAMEOID, TEXTOID};
    let name = |t| typefacts::facts_of(facts, t).is_some_and(|f| f.base == NAMEOID);
    let text = |t| typefacts::facts_of(facts, t).zip(typefacts::row_of(TEXTOID)).is_some_and(|(f, x)| f.image == x.image && f.eq == x.eq);
    coll.oid != 0 && coll.bytes_eq && ((name(a) && text(b)) || (text(a) && name(b)))
}
