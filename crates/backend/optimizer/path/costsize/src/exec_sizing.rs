//! The executor's memory-sizing arithmetic the planner prices plans with: C's
//! `hash_agg_entry_size` / `hash_agg_set_limits` / `hash_choose_num_partitions`
//! (nodeAgg.c), `get_hash_memory_limit` / `ExecChooseHashTableSize` (nodeHash.c)
//! and `ExecEstimateCacheEntryOverheadBytes` (nodeMemoize.c).
//!
//! These lived in the row executor's node crates (nodeagg, nodehash,
//! nodememoize); they moved here so the planner keeps them when those crates
//! go (rowexec deletion E2). The arithmetic is unchanged.

use ::types_tuple::SizeofMinimalTupleHeader;

// C: sizeof(MinimalTupleData) header = 15 bytes, MAXALIGNed to 16 by the callers.
const SIZEOF_MINIMAL_TUPLE_HEADER: usize = 15;
// C: sizeof(MemoryChunk) = 8 in production builds (memutils_memorychunk.h).
const CHUNKHDRSZ: usize = 8;

/// C `sizeof(AggStatePerGroupData)` (nodeAgg.h): a Datum and two bools, 16 bytes on
/// LP64. execexpr's `AggPerGroup` asserts the same size at compile time.
pub const SIZEOF_AGG_PER_GROUP: usize = 16;

#[inline]
const fn maxalign(n: usize) -> usize {
    (n + 7) & !7
}

/// C `hash_agg_entry_size` (nodeAgg.c).
pub fn hash_agg_entry_size(num_trans: usize, tuple_width: usize, transition_space: usize) -> f64 {
    let tuple_size = maxalign(SIZEOF_MINIMAL_TUPLE_HEADER) + tuple_width;
    let tuple_chunk_size = maxalign(tuple_size);
    let pergroup_chunk_size = num_trans * SIZEOF_AGG_PER_GROUP;
    let transition_chunk_size = if transition_space > 0 {
        CHUNKHDRSZ + transition_space.next_power_of_two()
    } else {
        0
    };
    (16 + tuple_chunk_size + pergroup_chunk_size + transition_chunk_size) as f64
}

const HASHAGG_PARTITION_FACTOR: f64 = 1.50;
const HASHAGG_MIN_PARTITIONS: f64 = 4.0;
const HASHAGG_MAX_PARTITIONS: f64 = 1024.0;
const HASHAGG_READ_BUFFER_SIZE: f64 = 8192.0;
const HASHAGG_WRITE_BUFFER_SIZE: f64 = 8192.0;

// C my_log2 (dynahash.c): ceil(log2(num)).
fn my_log2(num: i64) -> u32 {
    if num <= 1 {
        return 0;
    }
    64 - ((num - 1) as u64).leading_zeros()
}

/// C `hash_choose_num_partitions` (nodeAgg.c) -> (npartitions,
/// partition_bits).
pub fn hash_choose_num_partitions(
    input_groups: f64,
    hashentrysize: f64,
    used_bits: u32,
) -> (usize, u32) {
    let hash_mem_limit = get_hash_memory_limit() as f64;
    let partition_limit =
        (hash_mem_limit * 0.25 - HASHAGG_READ_BUFFER_SIZE) / HASHAGG_WRITE_BUFFER_SIZE;
    let mem_wanted = HASHAGG_PARTITION_FACTOR * input_groups * hashentrysize;
    let mut dpartitions = 1.0 + (mem_wanted / hash_mem_limit);
    if dpartitions > partition_limit {
        dpartitions = partition_limit;
    }
    dpartitions = dpartitions.clamp(HASHAGG_MIN_PARTITIONS, HASHAGG_MAX_PARTITIONS);
    let mut partition_bits = my_log2(dpartitions as i64);
    if partition_bits + used_bits >= 32 {
        partition_bits = 32 - used_bits;
    }
    (1usize << partition_bits, partition_bits)
}

/// C `hash_agg_set_limits` (nodeAgg.c) -> (mem_limit, ngroups_limit,
/// num_partitions).
pub fn hash_agg_set_limits(
    hashentrysize: f64,
    input_groups: f64,
    used_bits: u32,
) -> (usize, u64, usize) {
    let hash_mem_limit = get_hash_memory_limit();
    if input_groups * hashentrysize <= hash_mem_limit as f64 {
        return (hash_mem_limit, (hash_mem_limit as f64 / hashentrysize) as u64, 0);
    }
    let (npartitions, _) = hash_choose_num_partitions(input_groups, hashentrysize, used_bits);
    let partition_mem =
        (HASHAGG_READ_BUFFER_SIZE + HASHAGG_WRITE_BUFFER_SIZE * npartitions as f64) as usize;
    let mem_limit = if hash_mem_limit > 4 * partition_mem {
        hash_mem_limit - partition_mem
    } else {
        (hash_mem_limit as f64 * 0.75) as usize
    };
    let ngroups_limit =
        if mem_limit as f64 > hashentrysize { (mem_limit as f64 / hashentrysize) as u64 } else { 1 };
    (mem_limit, ngroups_limit, npartitions)
}

// C constants (hashjoin.h / htup_details.h), 64-bit build.
pub const HJTUPLE_OVERHEAD: usize = 16; // MAXALIGN(sizeof(HashJoinTupleData))
const SIZEOF_HASHJOINTUPLE: usize = 8; // pointer
const NTUP_PER_BUCKET: f64 = 1.0;
const MAX_ALLOC_SIZE: usize = 0x3fff_ffff;
const SKEW_HASH_MEM_PERCENT: usize = 2;
const SKEW_BUCKET_OVERHEAD: usize = 16; // MAXALIGN(sizeof(HashSkewBucket))
const BLCKSZ: usize = 8192;

/// C `get_hash_memory_limit` (nodeHash.c): `work_mem * hash_mem_multiplier * 1024`.
pub fn get_hash_memory_limit() -> usize {
    let work_mem = guc_tables::vars::work_mem.read() as f64;
    let mult = guc_tables::vars::hash_mem_multiplier.read();
    let bytes = work_mem * mult * 1024.0;
    if bytes < usize::MAX as f64 {
        bytes as usize
    } else {
        usize::MAX
    }
}

/// `ExecChooseHashTableSize` -> (numbuckets, numbatches, num_skew_mcvs).
pub fn exec_choose_hash_table_size(
    ntuples: f64,
    tupwidth: i32,
    useskew: bool,
    try_combined_hash_mem: bool,
    parallel_workers: i32,
) -> (u32, i32, i32) {
    let (b, n, s, _) = exec_choose_hash_table_size_full(
        ntuples,
        tupwidth,
        useskew,
        try_combined_hash_mem,
        parallel_workers,
    );
    (b, n, s)
}

/// As above plus `*space_allowed`.
pub fn exec_choose_hash_table_size_full(
    ntuples: f64,
    tupwidth: i32,
    useskew: bool,
    try_combined_hash_mem: bool,
    parallel_workers: i32,
) -> (u32, i32, i32, usize) {
    let ntuples = if ntuples <= 0.0 { 1000.0 } else { ntuples };

    let tupsize = HJTUPLE_OVERHEAD + maxalign(SizeofMinimalTupleHeader) + maxalign(tupwidth as usize);
    let inner_rel_bytes = ntuples * tupsize as f64;

    let mut hash_table_bytes = get_hash_memory_limit();
    // Parallel hash pools every worker's hash_mem; if the combined budget
    // still needs batching, the recursive call falls back to per-worker.
    if try_combined_hash_mem {
        let newlimit = hash_table_bytes as f64 * (parallel_workers as f64 + 1.0);
        hash_table_bytes = newlimit.min(usize::MAX as f64) as usize;
    }
    let mut space_allowed = hash_table_bytes;

    let mut num_skew_mcvs: i64 = 0;
    if useskew {
        let bytes_per_mcv =
            tupsize + (8 * core::mem::size_of::<usize>()) + core::mem::size_of::<i32>() + SKEW_BUCKET_OVERHEAD;
        let mut skew_mcvs = hash_table_bytes / bytes_per_mcv;
        skew_mcvs = (skew_mcvs * SKEW_HASH_MEM_PERCENT) / 100;
        skew_mcvs = skew_mcvs.min(i32::MAX as usize);
        num_skew_mcvs = skew_mcvs as i64;
        if skew_mcvs > 0 {
            hash_table_bytes -= skew_mcvs * bytes_per_mcv;
        }
    }

    let mut max_pointers = hash_table_bytes / SIZEOF_HASHJOINTUPLE;
    max_pointers = max_pointers.min(MAX_ALLOC_SIZE / SIZEOF_HASHJOINTUPLE);
    max_pointers = prevpower2(max_pointers);
    max_pointers = max_pointers.min(i32::MAX as usize / 2 + 1);

    let mut dbuckets = (ntuples / NTUP_PER_BUCKET).ceil();
    dbuckets = dbuckets.min(max_pointers as f64);
    let mut nbuckets = dbuckets as usize;
    nbuckets = nbuckets.max(1024);
    nbuckets = nextpower2_32(nbuckets as u32) as usize;

    let mut nbatch: i64 = 1;
    let bucket_bytes = SIZEOF_HASHJOINTUPLE * nbuckets;
    if inner_rel_bytes + bucket_bytes as f64 > hash_table_bytes as f64 {
        if try_combined_hash_mem {
            return exec_choose_hash_table_size_full(
                ntuples, tupwidth, useskew, false, parallel_workers,
            );
        }
        let bucket_size = tupsize * (NTUP_PER_BUCKET as usize) + SIZEOF_HASHJOINTUPLE;
        let mut sbuckets = if hash_table_bytes <= bucket_size {
            1
        } else {
            nextpower2_size(hash_table_bytes / bucket_size)
        };
        sbuckets = sbuckets.min(max_pointers);
        nbuckets = nextpower2_32(sbuckets as u32) as usize;
        let bucket_bytes2 = nbuckets * SIZEOF_HASHJOINTUPLE;
        let dbatch = (inner_rel_bytes / (hash_table_bytes - bucket_bytes2) as f64)
            .ceil()
            .min(max_pointers as f64);
        let minbatch = dbatch as i64;
        nbatch = nextpower2_32(2i64.max(minbatch) as u32) as i64;
    }

    while nbatch > 1 {
        if nbuckets > (MAX_ALLOC_SIZE / SIZEOF_HASHJOINTUPLE / 2) {
            break;
        }
        if space_allowed > usize::MAX / 2 {
            break;
        }
        if (nbatch as usize) < space_allowed / BLCKSZ {
            break;
        }
        nbuckets *= 2;
        num_skew_mcvs *= 2;
        space_allowed *= 2;
        nbatch /= 2;
    }

    (nbuckets as u32, nbatch as i32, num_skew_mcvs as i32, space_allowed)
}

#[inline]
fn nextpower2_32(n: u32) -> u32 {
    if n <= 1 {
        1
    } else {
        1u32 << (32 - (n - 1).leading_zeros())
    }
}

#[inline]
fn nextpower2_size(n: usize) -> usize {
    if n <= 1 {
        1
    } else {
        1usize << (usize::BITS - (n - 1).leading_zeros())
    }
}

#[inline]
fn prevpower2(n: usize) -> usize {
    if n == 0 {
        1
    } else {
        1usize << (usize::BITS - 1 - n.leading_zeros())
    }
}

// C LP64 sizeof(MemoizeEntry) / sizeof(MemoizeKey) / sizeof(MemoizeTuple).
const SIZEOF_MEMOIZE_ENTRY: u64 = 24;
const SIZEOF_MEMOIZE_KEY: u64 = 24;
const SIZEOF_MEMOIZE_TUPLE: u64 = 16;

/// `ExecEstimateCacheEntryOverheadBytes` (nodeMemoize.c), C LP64 sizes.
pub fn exec_estimate_cache_entry_overhead_bytes(ntuples: f64) -> f64 {
    (SIZEOF_MEMOIZE_ENTRY + SIZEOF_MEMOIZE_KEY) as f64 + SIZEOF_MEMOIZE_TUPLE as f64 * ntuples
}
