//! The seal-time metadata-builder factory — M3-E's real builders, driven by
//! M3-D's seal driver (spec §19.7: "M3-E builds, M3-D drives").
//!
//! ## What this module replaced
//!
//! Until this lane both seal sites ([`crate::writer::TableWriter::cut_part`]
//! and [`crate::par::ParEngine::seal_one`]) hardcoded
//! [`crate::meta_standin::StandinMetaBuilder`], whose own doc predicted the
//! fix shape verbatim: *"when M3-E's builders land they replace this through
//! the identical trait; the seal driver does not change."* That is exactly
//! what happened — not one line of `seal.rs` moved. The stand-in emitted
//! exact `nonnull` and nothing else: every zone key `Absent`, sortedness
//! `Unknown`, every other aggregate zero, no aux sections. Parts sealed that
//! way carry no min/max, no PSMA, no blooms and no NDV, so granule pruning
//! is structurally impossible over them however good the scan side gets.
//!
//! ## The profile, and why it can only come from the column descriptor
//!
//! [`ColumnMeta`] is parameterised by a [`MetaProfile`], which is derived
//! from `(StorageClass, CollationClass, TypeSemantics)`. The first two the
//! writer has always carried on [`ColSchema`]; the third it did not, and
//! could not invent — a storage class fixes bytes, not the order or equality
//! those bytes stand for. `uuid` and `interval` are both `Fixed{16}`;
//! `text`, `bytea`, `numeric` and `jsonb` are all `VarlenaVerbatim`. Guessing
//! `MemcmpOrdered` for an interval column would mint zone keys in memcmp
//! order for a type whose semantic order is not memcmp (1 month vs 30 days)
//! — wrong min/max, and therefore, once a prober trusts them, WRONG ANSWERS.
//! So `TypeSemantics` rides `ColSchema` as a declared per-column fact
//! (`pgrc_am::schema` maps it from `atttypid` beside the byval hint) and
//! this module only ever reads it.
//!
//! ## Degrade, never guess (the two failure arms)
//!
//! 1. A column whose semantics were never declared arrives as
//!    [`TypeSemantics::Opaque`] — validity + counts (+ byte lengths for
//!    varlena). That is *precisely* the stand-in's behaviour plus length
//!    stats, so an unmapped type loses pruning and changes nothing else.
//! 2. A column whose declared semantics are INCONSISTENT with its storage
//!    class (`MetaProfile::derive` returns `BadClassHint` — e.g. `SignedInt`
//!    on a varlena) falls back to the `Opaque` profile for that same class
//!    rather than refusing the seal. A mislabelled column must not be able
//!    to fail an ingest, and it must never be able to emit a key derived
//!    from a derivation its bytes do not satisfy. The `Opaque` arm is total
//!    over all six storage classes, so the fallback cannot itself fail;
//!    [`opaque_profile`] asserts that in a released expect.
//!
//! Both arms preserve the ANSWER-IDENTITY law: statistics are advisory
//! metadata, so a part sealed here must answer every query byte-identically
//! to the same part sealed with the stand-in — only the work skipped may
//! differ. The stand-in itself is retained (it is one leg of the born-RED
//! two-witness test, and the A/B arm for pruning measurements).

use pgrc_format::abi::ColumnMetaBuilder;
use pgrc_format::class::ColSchema;
use pgrc_meta::builder::ColumnMeta;
use pgrc_meta::profile::MetaProfile;

use crate::ingest::ColBuffer;
use crate::shred::ShredLane;

/// The profile for one column: its declared semantics when they are
/// consistent with its storage class, the `Opaque` profile otherwise (arm 2
/// of the degrade rule — see the module doc).
///
/// Delegates to [`MetaProfile::derive_or_opaque`] and does NOT reimplement
/// the fallback: the scan side derives its probe-time profile through the
/// same function, and the measured-only law is sound only while the two
/// agree exactly. A second copy of this rule here would be a latent way for
/// a prober to consult an aggregate this builder never computed.
pub fn profile_for(schema: &ColSchema) -> MetaProfile {
    MetaProfile::derive_or_opaque(schema.class, schema.collation_class, schema.semantics)
}

/// One real meta builder for one column. CONVERT W3-2a (cvformat A3): the builder's own heap is charged to `owner`
/// (the part's: its columns' owner, the loading statement), kind `Region`; `None`: uncharged.
pub fn builder_for(schema: &ColSchema, owner: Option<memledger::Owner>) -> Box<dyn ColumnMetaBuilder> {
    Box::new(ColumnMeta::with_owner(profile_for(schema), owner.map(|o| (o, memledger::Kind::Region))))
        as Box<dyn ColumnMetaBuilder>
}

/// The seal driver's builder vector, in the EXACT stream order `seal_part`
/// derives: roots first (path_ord 0), then shred lanes (part-global path_ord
/// 1..). `seal_part` re-checks the length against its own stream list and
/// refuses a mismatch typed, so the two orders cannot silently drift; this
/// function exists so both seal sites construct it identically.
///
/// A shred lane carries its OWN [`ColSchema`] (a typed lane's semantics are
/// not its parent's — an int lane shredded out of a jsonb column is an int),
/// so lanes derive their profiles from `lane.col.schema` exactly as roots do.
///
/// D3038: `octet` names the columns whose octet-length lanes `seal_part` derives (`crate::octet`); their builders
/// ([`octet_builder`]) follow the lanes', one per named column, whether or not the part seals its lane.
pub fn builders_for_streams(
    roots: &[ColBuffer],
    lanes: &[ShredLane],
    octet: &[u32],
) -> Vec<Box<dyn ColumnMetaBuilder>> {
    // CONVERT W3-2a (cvformat A3): every builder on the part's owner (its columns', read off the first root)
    let owner = roots.first().and_then(ColBuffer::owner);
    roots
        .iter()
        .map(|c| builder_for(&c.schema, owner))
        .chain(lanes.iter().map(|l| builder_for(&l.col.schema, owner)))
        .chain(octet.iter().map(|&a| octet_builder(a, owner)))
        .collect()
}

/// [`builders_for_streams`] for a pooled seal, whose streams seal on other threads.
pub fn send_builders_for_streams(roots: &[ColBuffer], lanes: &[ShredLane], octet: &[u32]) -> Vec<Box<dyn ColumnMetaBuilder + Send>> {
    let schemas = roots.iter().map(|c| &c.schema).chain(lanes.iter().map(|l| &l.col.schema));
    schemas.map(send_builder_for).chain(octet.iter().map(|&a| send_octet_builder(a))).collect()
}

/// [`builder_for`] for a stream sealed on another thread (a pooled seal).
pub fn send_builder_for(schema: &ColSchema) -> Box<dyn ColumnMetaBuilder + Send> {
    Box::new(ColumnMeta::new(profile_for(schema)))
}

/// D3038: the builder of column `attno`'s octet-length lane ([`octet_profile`]). CONVERT W3-2a (cvformat A3): its heap
/// is charged to `owner` as [`builder_for`]'s (kind `Region`; `None`: uncharged).
pub fn octet_builder(attno: u32, owner: Option<memledger::Owner>) -> Box<dyn ColumnMetaBuilder> {
    Box::new(ColumnMeta::with_owner(
        octet_profile(&pgrc_format::shredlane::octet_length_schema(attno)),
        owner.map(|o| (o, memledger::Kind::Region)),
    ))
}

/// [`octet_builder`] for a stream sealed on another thread (a pooled seal).
pub fn send_octet_builder(attno: u32) -> Box<dyn ColumnMetaBuilder + Send> {
    Box::new(ColumnMeta::new(octet_profile(&pgrc_format::shredlane::octet_length_schema(attno))))
}

/// D3038: an octet-length lane's profile (`crate::octet`): its words' min / max, sum and zero count, the length facts a
/// reader may answer from; no equality bloom or NDV registers, since no reader probes a length for a value and both hash
/// every row at seal.
fn octet_profile(schema: &ColSchema) -> MetaProfile {
    MetaProfile { eq_bloomable: false, ndv: false, ..profile_for(schema) }
}
