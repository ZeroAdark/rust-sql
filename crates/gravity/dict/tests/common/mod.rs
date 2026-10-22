#![allow(dead_code)]
use gravity_dict::{Dict, Region, View};

/// A synthetic dictionary: entries in code order, an optional code standing for NULL.
pub struct VecDict {
    pub entries: Vec<Vec<u8>>,
    pub null: Option<u32>,
}

impl VecDict {
    pub fn of(strs: &[&str]) -> VecDict {
        VecDict { entries: strs.iter().map(|s| s.as_bytes().to_vec()).collect(), null: None }
    }

    pub fn code_of(&self, s: &str) -> u32 {
        self.entries.iter().position(|e| e == s.as_bytes()).expect("entry") as u32
    }
}

impl Dict for VecDict {
    fn ncodes(&self) -> u32 {
        self.entries.len() as u32
    }

    fn null_code(&self) -> Option<u32> {
        self.null
    }

    fn entries(&self, f: &mut dyn FnMut(u32, View)) {
        for (i, e) in self.entries.iter().enumerate() {
            f(i as u32, View::of(e));
        }
    }
}

pub struct Buf(pub Vec<u64>);

impl Buf {
    pub fn new(bytes: usize) -> Buf {
        Buf(vec![0u64; bytes.div_ceil(8)])
    }

    pub fn region(&mut self) -> Region {
        unsafe { Region::new(self.0.as_mut_ptr() as *mut u8, self.0.len() * 8) }
    }
}

/// A synthetic URL-like corpus: `n` distinct strings from a fixed generator, byte-rank sorted
/// like a part dictionary.
pub fn url_corpus(n: usize, seed: u64) -> Vec<Vec<u8>> {
    let hosts = ["example.com", "shop.example.org", "news.site.ru", "m.video.net", "images.host.io", "a.b.c.d.e.f.example", "пример.рф", "日本.jp"];
    let paths = ["", "/", "/index.html", "/search?q=", "/item/", "/user/profile", "/cat/sub/leaf", "/img/", "/api/v2/items", "/blog/2024/03/post-"];
    let mut x = seed | 1;
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(n);
    let mut i = 0usize;
    while out.len() < n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let h = hosts[(x % hosts.len() as u64) as usize];
        let p = paths[((x >> 8) % paths.len() as u64) as usize];
        let s = format!("http://{h}{p}{}{}", i, if x & 0x100 != 0 { "&utm_source=Garden" } else if x & 0x200 != 0 { "?ref=search" } else { "" });
        out.push(s.into_bytes());
        i += 1;
    }
    out.sort();
    out.dedup();
    out
}
