pub const HEAP_DEFAULT_FILLFACTOR: i32 = 100;
pub const HEAP_MIN_FILLFACTOR: i32 = 10;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoVacOpts {
    pub enabled: bool,
    pub vacuum_threshold: i32,
    pub vacuum_max_threshold: i32,
    pub vacuum_ins_threshold: i32,
    pub analyze_threshold: i32,
    pub vacuum_cost_limit: i32,
    pub freeze_min_age: i32,
    pub freeze_max_age: i32,
    pub freeze_table_age: i32,
    pub multixact_freeze_min_age: i32,
    pub multixact_freeze_max_age: i32,
    pub multixact_freeze_table_age: i32,
    pub log_min_duration: i32,
    pub vacuum_cost_delay: f64,
    pub vacuum_scale_factor: f64,
    pub vacuum_ins_scale_factor: f64,
    pub analyze_scale_factor: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum StdRdOptIndexCleanup {
    STDRD_OPTION_VACUUM_INDEX_CLEANUP_AUTO = 0,
    STDRD_OPTION_VACUUM_INDEX_CLEANUP_OFF = 1,
    STDRD_OPTION_VACUUM_INDEX_CLEANUP_ON = 2,
}

pub use StdRdOptIndexCleanup::*;

// StdRdOptions (utils/rel.h) minus the vl_len_ varlena header: the parse
// result is an owned struct, not a bytea image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StdRdOptions {
    pub fillfactor: i32,
    pub toast_tuple_target: i32,
    pub autovacuum: AutoVacOpts,
    pub user_catalog_table: bool,
    pub parallel_workers: i32,
    pub vacuum_index_cleanup: StdRdOptIndexCleanup,
    pub vacuum_truncate: bool,
    pub vacuum_truncate_set: bool,
    pub vacuum_max_eager_freeze_failure_rate: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum ViewOptCheckOption {
    VIEW_OPTION_CHECK_OPTION_NOT_SET = 0,
    VIEW_OPTION_CHECK_OPTION_LOCAL = 1,
    VIEW_OPTION_CHECK_OPTION_CASCADED = 2,
}

pub use ViewOptCheckOption::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ViewOptions {
    pub security_barrier: bool,
    pub security_invoker: bool,
    pub check_option: ViewOptCheckOption,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum GistOptBufferingMode {
    GIST_OPTION_BUFFERING_AUTO = 0,
    GIST_OPTION_BUFFERING_ON = 1,
    GIST_OPTION_BUFFERING_OFF = 2,
}

pub use GistOptBufferingMode::*;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BTOptions {
    pub fillfactor: i32,
    pub vacuum_cleanup_index_scale_factor: f64,
    pub deduplicate_items: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HashOptions {
    pub fillfactor: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GinOptions {
    pub use_fast_update: bool,
    pub pending_list_cleanup_size: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GistOptions {
    pub fillfactor: i32,
    pub buffering_mode: GistOptBufferingMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpGistOptions {
    pub fillfactor: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HnswOptions {
    pub m: i32,
    pub ef_construction: i32,
}

/// contrib/bloom's parsed reloptions: types_bloom::BloomOptions (length already
/// converted bits -> words, C bloptions contract).
pub use types_bloom::BloomOptions;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrinOptions {
    pub pages_per_range: i32,
    pub autosummarize: bool,
}

pub const PGRCOLUMNAR_SORTED_BY_MAX: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PgrcolumnarOptions {
    pub std: StdRdOptions,
    sorted_by_len: u16,
    sorted_by_buf: [u8; PGRCOLUMNAR_SORTED_BY_MAX],
    shred_paths_len: u16,
    shred_paths_buf: [u8; PGRCOLUMNAR_SORTED_BY_MAX],
    sorted_by_partial: bool,
    sort_runs: bool,
}

impl PgrcolumnarOptions {
    pub fn new(std: StdRdOptions) -> PgrcolumnarOptions {
        PgrcolumnarOptions {
            std,
            sorted_by_len: 0,
            sorted_by_buf: [0; PGRCOLUMNAR_SORTED_BY_MAX],
            shred_paths_len: 0,
            shred_paths_buf: [0; PGRCOLUMNAR_SORTED_BY_MAX],
            sorted_by_partial: false,
            sort_runs: false,
        }
    }

    pub fn set_sort_runs(&mut self, on: bool) {
        self.sort_runs = on;
    }

    pub fn sort_runs(&self) -> bool {
        self.sort_runs
    }

    /// false = value too long (caller reports the option error).
    pub fn set_sorted_by(&mut self, v: &str) -> bool {
        if v.len() > PGRCOLUMNAR_SORTED_BY_MAX {
            return false;
        }
        self.sorted_by_buf[..v.len()].copy_from_slice(v.as_bytes());
        self.sorted_by_len = v.len() as u16;
        true
    }

    pub fn sorted_by(&self) -> &str {
        // Buffer only ever holds set_sorted_by's UTF-8.
        core::str::from_utf8(&self.sorted_by_buf[..self.sorted_by_len as usize]).unwrap()
    }

    /// `sorted_by_partial`: the key was set on a table that held parts, so it
    /// holds only for the parts written after it (ALTER ... SET sorted_by on
    /// a non-empty table): no whole-relation order claim.
    pub fn set_sorted_by_partial(&mut self, v: bool) {
        self.sorted_by_partial = v;
    }

    pub fn sorted_by_partial(&self) -> bool {
        self.sorted_by_partial
    }

    /// false = value too long (caller reports the option error).
    pub fn set_shred_paths(&mut self, v: &str) -> bool {
        if v.len() > PGRCOLUMNAR_SORTED_BY_MAX {
            return false;
        }
        self.shred_paths_buf[..v.len()].copy_from_slice(v.as_bytes());
        self.shred_paths_len = v.len() as u16;
        true
    }

    /// The `pgrc_shred_paths` canonical spelling (`a.b:text, c:numeric`).
    pub fn shred_paths(&self) -> &str {
        // Buffer only ever holds set_shred_paths's UTF-8.
        core::str::from_utf8(&self.shred_paths_buf[..self.shred_paths_len as usize]).unwrap()
    }

    /// The declared typed paths, in order; an element that does not
    /// parse is skipped (DDL validated the spelling).
    pub fn shred_path_decls(&self) -> Vec<ShredPathDecl> {
        self.shred_paths().split(',').filter_map(ShredPathDecl::parse).collect()
    }

    /// The declared key columns in order (syntax-checked at parse time;
    /// whitespace around a name is trimmed). A path key yields its jsonb
    /// column.
    pub fn sorted_by_cols(&self) -> impl Iterator<Item = String> {
        self.sorted_by_keys().into_iter().map(|k| k.col)
    }

    /// The declared keys in order, read back from the stored canonical
    /// spelling ([`SortedByKey::render`]: names verbatim). An element that
    /// does not read as a key comes back as a column of that whole spelling,
    /// which no live column matches, so the whole key is void.
    pub fn sorted_by_keys(&self) -> Vec<SortedByKey> {
        split_sorted_by(self.sorted_by())
            .into_iter()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(|c| {
                read_canonical_key(c).unwrap_or_else(|| SortedByKey { col: c.to_string(), path: Vec::new() })
            })
            .collect()
    }
}

/// The lane kind of a typed path (`pgrc_shred_paths`, DESIGN R3): the
/// writer's `jsonb_shred::Lane` it is elected at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShredPathKind {
    Text,
    Numeric,
    Bool,
    Uuid,
}

impl ShredPathKind {
    pub fn name(self) -> &'static str {
        match self {
            ShredPathKind::Text => "text",
            ShredPathKind::Numeric => "numeric",
            ShredPathKind::Bool => "bool",
            ShredPathKind::Uuid => "uuid",
        }
    }
}

/// One `pgrc_shred_paths` element: `a.b[:kind]`, a dotted path of object
/// keys (no empty key), kind text (default), int / bigint / numeric, bool
/// or uuid. The declared path is elected in every part at that kind, ahead
/// of the cap and with no presence floor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShredPathDecl {
    pub keys: Vec<String>,
    pub kind: ShredPathKind,
}

impl ShredPathDecl {
    pub fn parse(item: &str) -> Option<ShredPathDecl> {
        let item = item.trim();
        let (path, kind) = match item.rsplit_once(':') {
            Some((p, k)) => (p.trim(), k.trim().to_ascii_lowercase()),
            None => (item, "text".to_string()),
        };
        let kind = match kind.as_str() {
            "text" => ShredPathKind::Text,
            "int" | "integer" | "bigint" | "int8" | "int4" | "numeric" => ShredPathKind::Numeric,
            "bool" | "boolean" => ShredPathKind::Bool,
            "uuid" => ShredPathKind::Uuid,
            _ => return None,
        };
        let keys: Vec<String> = path.split('.').map(str::to_string).collect();
        if path.is_empty() || keys.iter().any(|k| k.is_empty()) {
            return None;
        }
        Some(ShredPathDecl { keys, kind })
    }

    pub fn render(&self) -> String {
        format!("{}:{}", self.keys.join("."), self.kind.name())
    }
}

/// One `sorted_by` key: a column (`path` empty), or a text path on a jsonb
/// column, `col->'k1'->...->>'kn'` (object keys; `->` steps then one final
/// `->>`): the writer sorts by PostgreSQL's value of that expression (NULL
/// when a step meets no object or no key, or the value is JSON null).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortedByKey {
    pub col: String,
    pub path: Vec<String>,
}

impl SortedByKey {
    /// The canonical spelling: the name verbatim, then `->'k'` steps and the
    /// final `->>'k'` (a quote doubled inside a key).
    pub fn render(&self) -> String {
        let mut out = self.col.clone();
        for (i, k) in self.path.iter().enumerate() {
            out.push_str(if i + 1 == self.path.len() { "->>'" } else { "->'" });
            out.push_str(&k.replace('\'', "''"));
            out.push('\'');
        }
        out
    }
}

/// Split a `sorted_by` value at the commas outside single quotes (path keys)
/// and double quotes (names).
pub fn split_sorted_by(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut sq, mut dq) = (0usize, false, false);
    for (i, ch) in s.char_indices() {
        match ch {
            '\'' if !dq => sq = !sq,
            '"' if !sq => dq = !dq,
            ',' if !sq && !dq => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// Read one canonical element back (the inverse of [`SortedByKey::render`]).
fn read_canonical_key(item: &str) -> Option<SortedByKey> {
    let cut = item.find("->").unwrap_or(item.len());
    let col = item[..cut].to_string();
    let mut rest = &item[cut..];
    let mut path = Vec::new();
    let mut texts = Vec::new();
    while !rest.is_empty() {
        let text = rest.starts_with("->>");
        rest = rest.strip_prefix(if text { "->>'" } else { "->'" })?;
        let mut key = std::string::String::new();
        let mut end = None;
        let b = rest.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\'' {
                if b.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                end = Some(i);
                break;
            }
            i += 1;
        }
        let end = end?;
        key.push_str(&rest[..end].replace("''", "'"));
        rest = &rest[end + 1..];
        path.push(key);
        texts.push(text);
    }
    if let Some((last, init)) = texts.split_last() {
        if !*last || init.iter().any(|t| *t) {
            return None;
        }
    }
    Some(SortedByKey { col, path })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RdOptions {
    Std(StdRdOptions),
    Pgrcolumnar(PgrcolumnarOptions),
    View(ViewOptions),
    BTree(BTOptions),
    Hash(HashOptions),
    Gin(GinOptions),
    Gist(GistOptions),
    SpGist(SpGistOptions),
    Brin(BrinOptions),
    Hnsw(HnswOptions),
    Bloom(BloomOptions),
}

impl RdOptions {
    #[inline]
    pub fn std(&self) -> Option<&StdRdOptions> {
        match self {
            RdOptions::Std(o) => Some(o),
            // pgrcolumnar carries the heap vocabulary verbatim.
            RdOptions::Pgrcolumnar(o) => Some(&o.std),
            _ => None,
        }
    }

    #[inline]
    pub fn pgrcolumnar(&self) -> Option<&PgrcolumnarOptions> {
        match self {
            RdOptions::Pgrcolumnar(o) => Some(o),
            _ => None,
        }
    }

    #[inline]
    pub fn view(&self) -> Option<&ViewOptions> {
        match self {
            RdOptions::View(o) => Some(o),
            _ => None,
        }
    }

    // RelationGetFillFactor is used by C against whichever option struct the
    // relkind/AM parsed; variants without a fillfactor member fall to default.
    #[inline]
    pub fn fillfactor(&self) -> Option<i32> {
        match self {
            RdOptions::Std(o) => Some(o.fillfactor),
            RdOptions::Pgrcolumnar(o) => Some(o.std.fillfactor),
            RdOptions::BTree(o) => Some(o.fillfactor),
            RdOptions::Hash(o) => Some(o.fillfactor),
            RdOptions::Gist(o) => Some(o.fillfactor),
            RdOptions::SpGist(o) => Some(o.fillfactor),
            RdOptions::View(_) | RdOptions::Gin(_) | RdOptions::Brin(_)
            | RdOptions::Hnsw(_) | RdOptions::Bloom(_) => None,
        }
    }

    #[inline]
    pub fn btree(&self) -> Option<&BTOptions> {
        match self {
            RdOptions::BTree(o) => Some(o),
            _ => None,
        }
    }

    #[inline]
    pub fn hash(&self) -> Option<&HashOptions> {
        match self {
            RdOptions::Hash(o) => Some(o),
            _ => None,
        }
    }

    #[inline]
    pub fn gin(&self) -> Option<&GinOptions> {
        match self {
            RdOptions::Gin(o) => Some(o),
            _ => None,
        }
    }

    #[inline]
    pub fn gist(&self) -> Option<&GistOptions> {
        match self {
            RdOptions::Gist(o) => Some(o),
            _ => None,
        }
    }

    #[inline]
    pub fn spgist(&self) -> Option<&SpGistOptions> {
        match self {
            RdOptions::SpGist(o) => Some(o),
            _ => None,
        }
    }

    #[inline]
    pub fn brin(&self) -> Option<&BrinOptions> {
        match self {
            RdOptions::Brin(o) => Some(o),
            _ => None,
        }
    }

    #[inline]
    pub fn bloom(&self) -> Option<BloomOptions> {
        match self {
            RdOptions::Bloom(o) => Some(*o),
            _ => None,
        }
    }

    #[inline]
    pub fn hnsw(&self) -> Option<&HnswOptions> {
        match self {
            RdOptions::Hnsw(o) => Some(o),
            _ => None,
        }
    }
}
