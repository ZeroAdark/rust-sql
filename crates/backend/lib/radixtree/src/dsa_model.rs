//! dsa.c's page, superblock and segment rules replayed for an RT_SHMEM tree
//! kept in local memory, so memory_usage is dsa_get_total_size.

use types_error::{PgError, PgResult};

const FPM_PAGE_SIZE: usize = 4096;
const DSA_PAGES_PER_SUPERBLOCK: usize = 16;
const DSA_NUM_SEGMENTS_AT_EACH_SIZE: usize = 2;
const DSA_NUM_SEGMENT_BINS: usize = 16;
const DSA_MAX_SEGMENTS: usize = 1024;
pub const DSA_DEFAULT_INIT_SEGMENT_SIZE: usize = 1024 * 1024;
pub const DSA_MIN_SEGMENT_SIZE: usize = 256 * 1024;
// 40 offset bits with 8-byte dsa_pointers, 27 with the 4-byte ones of a 32-bit build.
#[cfg(not(target_family = "wasm"))]
pub const DSA_MAX_SEGMENT_SIZE: usize = 1 << 40;
#[cfg(target_family = "wasm")]
pub const DSA_MAX_SEGMENT_SIZE: usize = 1 << 27;

// MAXALIGN'd LP64 sizeofs.
const SIZEOF_AREA_CONTROL: usize = 6192;
const SIZEOF_FREE_PAGE_MANAGER: usize = 1096;
const SIZEOF_SEGMENT_HEADER: usize = 56;
const SIZEOF_DSA_POINTER: usize = 8;

const DSA_SCLASS_BLOCK_OF_SPANS: usize = 0;

const DSA_SIZE_CLASSES: [usize; 38] = [
    56, 0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448,
    512, 640, 768, 896, 1024, 1280, 1560, 1816, 2048, 2616, 3120, 3640, 4096, 5456, 6552, 7280,
    8192,
];

const DSA_SIZE_CLASS_MAP: [u8; 128] = [
    2, 3, 4, 5, 6, 7, 8, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 14, 14, 15, 15, 15, 15, 16,
    16, 16, 16, 17, 17, 17, 17, 18, 18, 18, 18, 18, 18, 18, 18, 19, 19, 19, 19, 19, 19, 19, 19,
    20, 20, 20, 20, 20, 20, 20, 20, 21, 21, 21, 21, 21, 21, 21, 21, 22, 22, 22, 22, 22, 22, 22,
    22, 22, 22, 22, 22, 22, 22, 22, 22, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23, 23,
    23, 23, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25,
    25, 25, 25, 25, 25, 25, 25, 25, 25, 25, 25,
];
const DSA_SIZE_CLASS_MAP_QUANTUM: usize = 8;

struct Segment {
    // One contiguous run: this model never returns pages.
    free_pages: usize,
    bin: usize,
    bin_seq: u64,
}

pub struct DsaSizeModel {
    init_segment_size: usize,
    max_segment_size: usize,
    total_segment_size: usize,
    segments: Vec<Segment>,
    bin_seq: u64,
    free_objects: [usize; DSA_SIZE_CLASSES.len()],
}

fn contiguous_pages_to_segment_bin(n: usize) -> usize {
    let fls = (usize::BITS - n.leading_zeros()) as usize;
    fls.min(DSA_NUM_SEGMENT_BINS - 1)
}

fn pad_to_page(bytes: usize) -> usize {
    bytes.div_ceil(FPM_PAGE_SIZE) * FPM_PAGE_SIZE
}

fn size_class_for(size: usize) -> usize {
    if size < DSA_SIZE_CLASS_MAP.len() * DSA_SIZE_CLASS_MAP_QUANTUM {
        let mapidx = size.div_ceil(DSA_SIZE_CLASS_MAP_QUANTUM) - 1;
        return DSA_SIZE_CLASS_MAP[mapidx] as usize;
    }
    let mut min = DSA_SIZE_CLASS_MAP[DSA_SIZE_CLASS_MAP.len() - 1] as usize;
    let mut max = DSA_SIZE_CLASSES.len() - 1;
    while min < max {
        let mid = (min + max) / 2;
        if DSA_SIZE_CLASSES[mid] < size {
            min = mid + 1;
        } else {
            max = mid;
        }
    }
    min
}

fn out_of_memory(size: usize) -> Box<PgError> {
    Box::new(
        PgError::error("out of memory")
            .with_sqlstate(types_error::ERRCODE_OUT_OF_MEMORY)
            .with_detail(format!("Failed on DSA request of size {size}.")),
    )
}

impl DsaSizeModel {
    /// tidstore.c TidStoreCreateShared's segment sizing, then dsa_create_ext.
    pub fn for_tidstore(max_bytes: usize) -> PgResult<DsaSizeModel> {
        let mut max_segment_size = DSA_MAX_SEGMENT_SIZE;
        while 8 * max_segment_size > max_bytes {
            max_segment_size >>= 1;
        }
        max_segment_size = max_segment_size.max(DSA_MIN_SEGMENT_SIZE);
        let init_segment_size = DSA_DEFAULT_INIT_SEGMENT_SIZE.min(max_segment_size);
        Self::create(init_segment_size, max_segment_size)
    }

    /// dsa.c create_internal.
    pub fn create(init_segment_size: usize, max_segment_size: usize) -> PgResult<DsaSizeModel> {
        let total_pages = init_segment_size / FPM_PAGE_SIZE;
        let metadata = pad_to_page(
            SIZEOF_AREA_CONTROL + SIZEOF_FREE_PAGE_MANAGER + total_pages * SIZEOF_DSA_POINTER,
        );
        let usable_pages = (init_segment_size - metadata) / FPM_PAGE_SIZE;
        let mut model = DsaSizeModel {
            init_segment_size,
            max_segment_size,
            total_segment_size: init_segment_size,
            segments: Vec::new(),
            bin_seq: 0,
            free_objects: [0; DSA_SIZE_CLASSES.len()],
        };
        model.push_segment(usable_pages)?;
        Ok(model)
    }

    pub fn total_size(&self) -> usize {
        self.total_segment_size
    }

    fn push_segment(&mut self, usable_pages: usize) -> PgResult<()> {
        self.segments
            .try_reserve(1)
            .map_err(|_| out_of_memory(core::mem::size_of::<Segment>()))?;
        self.bin_seq += 1;
        self.segments.push(Segment {
            free_pages: usable_pages,
            bin: contiguous_pages_to_segment_bin(usable_pages),
            bin_seq: self.bin_seq,
        });
        Ok(())
    }

    pub fn allocate(&mut self, size: usize) -> PgResult<()> {
        let largest = DSA_SIZE_CLASSES[DSA_SIZE_CLASSES.len() - 1];
        if size > largest {
            self.alloc_object(DSA_SCLASS_BLOCK_OF_SPANS, size)?;
            return self.get_pages(size.div_ceil(FPM_PAGE_SIZE), size);
        }
        self.alloc_object(size_class_for(size), size)
    }

    /// dsa_free; pages of emptied superblocks are not given back.
    pub fn free(&mut self, size: usize) {
        if size <= DSA_SIZE_CLASSES[DSA_SIZE_CLASSES.len() - 1] {
            self.free_objects[size_class_for(size)] += 1;
        }
    }

    /// alloc_object + ensure_active_superblock.
    fn alloc_object(&mut self, size_class: usize, request: usize) -> PgResult<()> {
        if self.free_objects[size_class] == 0 {
            let obsize = DSA_SIZE_CLASSES[size_class];
            let (npages, nmax) = if size_class == DSA_SCLASS_BLOCK_OF_SPANS {
                (1, FPM_PAGE_SIZE / obsize - 1)
            } else {
                self.alloc_object(DSA_SCLASS_BLOCK_OF_SPANS, request)?;
                (DSA_PAGES_PER_SUPERBLOCK, DSA_PAGES_PER_SUPERBLOCK * FPM_PAGE_SIZE / obsize)
            };
            self.get_pages(npages, request)?;
            self.free_objects[size_class] = nmax;
        }
        self.free_objects[size_class] -= 1;
        Ok(())
    }

    fn get_pages(&mut self, npages: usize, request: usize) -> PgResult<()> {
        let idx = match self.best_segment(npages) {
            Some(idx) => idx,
            None => self.make_new_segment(npages).ok_or_else(|| out_of_memory(request))??,
        };
        self.segments[idx].free_pages -= npages;
        Ok(())
    }

    fn bin_next(&self, bin: usize, below: u64) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, seg) in self.segments.iter().enumerate() {
            if seg.bin == bin
                && seg.bin_seq < below
                && best.is_none_or(|b| seg.bin_seq > self.segments[b].bin_seq)
            {
                best = Some(i);
            }
        }
        best
    }

    fn best_segment(&mut self, npages: usize) -> Option<usize> {
        for bin in contiguous_pages_to_segment_bin(npages)..DSA_NUM_SEGMENT_BINS {
            let threshold = 1usize << (bin.max(1) - 1);
            let mut below = u64::MAX;
            while let Some(i) = self.bin_next(bin, below) {
                below = self.segments[i].bin_seq;
                let contiguous = self.segments[i].free_pages;
                if contiguous >= threshold && contiguous < npages {
                    continue;
                }
                if contiguous < threshold {
                    self.bin_seq += 1;
                    let seg = &mut self.segments[i];
                    seg.bin = contiguous_pages_to_segment_bin(contiguous);
                    seg.bin_seq = self.bin_seq;
                }
                if contiguous >= npages {
                    return Some(i);
                }
            }
        }
        None
    }

    /// dsa.c make_new_segment; None where C returns NULL.
    fn make_new_segment(&mut self, requested_pages: usize) -> Option<PgResult<usize>> {
        let new_index = self.segments.len();
        if new_index >= DSA_MAX_SEGMENTS {
            return None;
        }
        let mut total_size = self
            .init_segment_size
            .checked_shl((new_index / DSA_NUM_SEGMENTS_AT_EACH_SIZE) as u32)
            .unwrap_or(usize::MAX)
            .min(self.max_segment_size);
        let fixed = SIZEOF_SEGMENT_HEADER + SIZEOF_FREE_PAGE_MANAGER;
        let mut metadata = pad_to_page(fixed + SIZEOF_DSA_POINTER * (total_size / FPM_PAGE_SIZE));
        if total_size <= metadata {
            return None;
        }
        let mut usable_pages = (total_size - metadata) / FPM_PAGE_SIZE;
        if requested_pages > usable_pages {
            usable_pages = requested_pages;
            metadata = fixed + usable_pages * SIZEOF_DSA_POINTER;
            metadata += metadata.div_ceil(FPM_PAGE_SIZE - SIZEOF_DSA_POINTER) * SIZEOF_DSA_POINTER;
            metadata = pad_to_page(metadata);
            total_size = metadata + usable_pages * FPM_PAGE_SIZE;
            if total_size > DSA_MAX_SEGMENT_SIZE {
                return None;
            }
        }
        self.total_segment_size += total_size;
        Some(self.push_segment(usable_pages).map(|()| new_index))
    }
}
