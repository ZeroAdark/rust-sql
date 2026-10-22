//! CFG analysis for the stackifier: reverse postorder, dominators
//! (Cooper–Harvey–Kennedy), the reducibility check and Ramsey's node
//! classes ("Beyond Relooper", ICFP 2022 §4: a block is a *merge node*
//! when two or more forward edges enter it, a *loop header* when a back
//! edge does).

use gravity_ir::{Block, Function};

pub struct Cfg {
    /// Reverse postorder, entry first; unreachable blocks are absent.
    pub rpo: Vec<Block>,
    /// Position in `rpo` per block, `u32::MAX` when unreachable.
    pub rpo_num: Vec<u32>,
    /// Immediate dominator per block (the entry's is itself).
    pub idom: Vec<Block>,
    /// Dominator-tree children per block.
    pub children: Vec<Vec<Block>>,
    pub merge: Vec<bool>,
    pub loop_header: Vec<bool>,
}

pub enum CfgError {
    Irreducible { from: Block, to: Block },
}

impl Cfg {
    pub fn reachable(&self, b: Block) -> bool {
        self.rpo_num[b.index()] != u32::MAX
    }

    pub fn is_back_edge(&self, from: Block, to: Block) -> bool {
        self.rpo_num[to.index()] <= self.rpo_num[from.index()]
    }

    pub fn dominates(&self, a: Block, b: Block) -> bool {
        let mut x = b;
        loop {
            if x == a {
                return true;
            }
            let d = self.idom[x.index()];
            if d == x {
                return false;
            }
            x = d;
        }
    }

    pub fn build(f: &Function) -> Result<Cfg, CfgError> {
        let n = f.blocks.len();
        let mut rpo_num = vec![u32::MAX; n];
        let mut post: Vec<Block> = Vec::with_capacity(n);
        let mut visited = vec![false; n];
        let mut stack: Vec<(Block, usize)> = vec![(f.entry, 0)];
        visited[f.entry.index()] = true;
        while let Some(&mut (b, ref mut next)) = stack.last_mut() {
            let succs: Vec<Block> = f.block(b).term.edges().map(|e| e.target).collect();
            if *next < succs.len() {
                let s = succs[*next];
                *next += 1;
                if !visited[s.index()] {
                    visited[s.index()] = true;
                    stack.push((s, 0));
                }
            } else {
                post.push(b);
                stack.pop();
            }
        }
        let rpo: Vec<Block> = post.iter().rev().copied().collect();
        for (i, &b) in rpo.iter().enumerate() {
            rpo_num[b.index()] = i as u32;
        }
        let mut preds: Vec<Vec<Block>> = vec![Vec::new(); n];
        for &b in &rpo {
            for e in f.block(b).term.edges() {
                preds[e.target.index()].push(b);
            }
        }
        let mut idom: Vec<Block> = vec![f.entry; n];
        let mut known = vec![false; n];
        known[f.entry.index()] = true;
        let mut changed = true;
        while changed {
            changed = false;
            for &b in rpo.iter().skip(1) {
                let mut new: Option<Block> = None;
                for &p in &preds[b.index()] {
                    if !known[p.index()] {
                        continue;
                    }
                    new = Some(match new {
                        None => p,
                        Some(q) => intersect(&idom, &rpo_num, p, q),
                    });
                }
                let new = new.expect("a reachable block has a processed predecessor in rpo");
                if !known[b.index()] || idom[b.index()] != new {
                    idom[b.index()] = new;
                    known[b.index()] = true;
                    changed = true;
                }
            }
        }
        let mut children: Vec<Vec<Block>> = vec![Vec::new(); n];
        for &b in rpo.iter().skip(1) {
            children[idom[b.index()].index()].push(b);
        }
        let mut merge = vec![false; n];
        let mut loop_header = vec![false; n];
        let mut fwd_in = vec![0u32; n];
        for &b in &rpo {
            for e in f.block(b).term.edges() {
                let t = e.target;
                if rpo_num[t.index()] <= rpo_num[b.index()] {
                    let mut x = b;
                    let dominated = loop {
                        if x == t {
                            break true;
                        }
                        let d = idom[x.index()];
                        if d == x {
                            break false;
                        }
                        x = d;
                    };
                    if !dominated {
                        return Err(CfgError::Irreducible { from: b, to: t });
                    }
                    loop_header[t.index()] = true;
                } else {
                    fwd_in[t.index()] += 1;
                }
            }
        }
        for b in 0..n {
            merge[b] = fwd_in[b] >= 2;
        }
        Ok(Cfg { rpo, rpo_num, idom, children, merge, loop_header })
    }
}

fn intersect(idom: &[Block], rpo_num: &[u32], mut a: Block, mut b: Block) -> Block {
    while a != b {
        while rpo_num[a.index()] > rpo_num[b.index()] {
            a = idom[a.index()];
        }
        while rpo_num[b.index()] > rpo_num[a.index()] {
            b = idom[b.index()];
        }
    }
    a
}
