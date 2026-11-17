//! In-memory call/reference graph in CSR form, loaded from the `edge` (index store) and `ts_ref`
//! (tree-sitter) tables. Long-lived processes (the MCP server, `foxd`) keep one per project and
//! rebuild it when that project's generation moves; cold CLI calls use recursive SQL instead.

use anyhow::Result;
use rusqlite::Connection;

/// Node ids are kept in one sorted array and found by binary search (no hash map: on big
/// projects the map cost more memory than the adjacency itself).
#[derive(Default)]
pub struct Graph {
    ids: Vec<i64>,
    out_off: Vec<u32>,
    out: Vec<u32>,
    in_off: Vec<u32>,
    inn: Vec<u32>,
}

fn csr(n: usize, pairs: &[(u32, u32)]) -> (Vec<u32>, Vec<u32>) {
    let mut off = vec![0u32; n + 1];
    for &(a, _) in pairs {
        off[a as usize + 1] += 1;
    }
    for i in 0..n {
        off[i + 1] += off[i];
    }
    let mut fill = off.clone();
    let mut adj = vec![0u32; pairs.len()];
    for &(a, b) in pairs {
        adj[fill[a as usize] as usize] = b;
        fill[a as usize] += 1;
    }
    (off, adj)
}

impl Graph {
    /// The call/reference graph of one project. Every resolved tree-sitter reference and every
    /// index-store call is in `edge`, keyed by its source, so this is a scan of the project's
    /// id ranges over the edge primary key (a join through `node` on older stores).
    pub fn load_project(c: &Connection, pid: i64) -> Result<Graph> {
        let mut raw: Vec<(i64, i64)> = Vec::new();
        let push = |raw: &mut Vec<(i64, i64)>, q: &mut rusqlite::Statement, p: &[i64]| -> Result<()> {
            let mut rows = q.query(rusqlite::params_from_iter(p))?;
            while let Some(r) = rows.next()? {
                raw.push((r.get(0)?, r.get(1)?));
            }
            Ok(())
        };
        let exact = crate::db::ranges(c, &[pid]);
        match exact.as_ref().map(|rs| crate::db::cover(rs, 16)) {
            Some(rs) => {
                // Big projects: scan slices of the ranges in parallel, each on its own connection.
                let span: i64 = rs.iter().map(|(lo, hi)| hi - lo + 1).sum();
                let step = (span / 8).max(50_000);
                let slices: Vec<(i64, i64)> =
                    rs.iter().flat_map(|&(lo, hi)| (lo..=hi).step_by(step as usize).map(move |a| (a, (a + step - 1).min(hi)))).collect();
                const SQL: &str = "SELECT src, dst FROM edge WHERE src BETWEEN ?1 AND ?2 AND kind IN ('CALLS','REFERENCES')";
                let parts: Vec<Option<Vec<(i64, i64)>>> = std::thread::scope(|sc| {
                    let hs: Vec<_> = slices
                        .iter()
                        .skip(1)
                        .map(|&(lo, hi)| {
                            sc.spawn(move || {
                                let c = crate::db::open_ro()?;
                                let mut q = c.prepare(SQL).ok()?;
                                let v = q.query_map([lo, hi], |r| Ok((r.get(0)?, r.get(1)?))).ok()?.flatten().collect();
                                Some(v)
                            })
                        })
                        .collect();
                    hs.into_iter().map(|h| h.join().ok().flatten()).collect()
                });
                let mut q = c.prepare_cached(SQL)?;
                if let Some(&(lo, hi)) = slices.first() {
                    push(&mut raw, &mut q, &[lo, hi])?;
                }
                for (part, &(lo, hi)) in parts.into_iter().zip(slices.iter().skip(1)) {
                    match part {
                        Some(v) => raw.extend(v),
                        // No second connection (an in-memory or vanished store): scan it here.
                        None => push(&mut raw, &mut q, &[lo, hi])?,
                    }
                }
            }
            None => {
                let mut q = c.prepare_cached(
                    "SELECT e.src, e.dst FROM node n JOIN edge e ON e.src = n.id WHERE n.project_id = ?1 AND e.kind IN ('CALLS','REFERENCES')",
                )?;
                push(&mut raw, &mut q, &[pid])?;
            }
        }
        if let Some(ex) = exact.as_ref().filter(|ex| ex.len() > 16) {
            // The scanned slices bridged other projects' interleaved saves: drop their edges.
            raw.retain(|&(src, _)| {
                let i = ex.partition_point(|r| r.1 < src);
                ex.get(i).is_some_and(|r| r.0 <= src)
            });
        }
        let t = std::time::Instant::now();
        let g = Graph::from_pairs(raw);
        if std::env::var_os("FOXDB_TRACE").is_some() {
            eprintln!("graph csr {:?}", t.elapsed());
        }
        Ok(g)
    }

    pub fn from_pairs(mut raw: Vec<(i64, i64)>) -> Graph {
        use rayon::prelude::*;
        raw.par_sort_unstable();
        raw.dedup();
        let mut ids: Vec<i64> = raw.iter().flat_map(|&(a, b)| [a, b]).collect();
        ids.par_sort_unstable();
        ids.dedup();
        let at = |id: i64| ids.binary_search(&id).map(|i| i as u32).unwrap_or(0);
        let pairs: Vec<(u32, u32)> = raw.par_iter().map(|&(a, b)| (at(a), at(b))).collect();
        let n = ids.len();
        let (out_off, out) = csr(n, &pairs);
        let mut rev: Vec<(u32, u32)> = pairs.par_iter().map(|&(a, b)| (b, a)).collect();
        rev.par_sort_unstable();
        let (in_off, inn) = csr(n, &rev);
        Graph { ids, out_off, out, in_off, inn }
    }

    pub fn edges(&self) -> usize {
        self.out.len()
    }

    fn adj<'a>(&'a self, off: &'a [u32], adj: &'a [u32], id: i64) -> impl Iterator<Item = i64> + 'a {
        let r = self.ids.binary_search(&id).map(|i| off[i] as usize..off[i + 1] as usize).unwrap_or(0..0);
        adj[r].iter().map(|&j| self.ids[j as usize])
    }

    pub fn callees(&self, id: i64) -> impl Iterator<Item = i64> + '_ {
        self.adj(&self.out_off, &self.out, id)
    }

    pub fn callers(&self, id: i64) -> impl Iterator<Item = i64> + '_ {
        self.adj(&self.in_off, &self.inn, id)
    }

    /// Callers by level (BFS, up to `depth` hops), excluding the roots.
    pub fn caller_levels(&self, roots: &[i64], depth: usize) -> Vec<Vec<i64>> {
        let mut seen: std::collections::HashSet<i64> = roots.iter().copied().collect();
        let mut frontier = roots.to_vec();
        let mut levels = vec![];
        for _ in 0..depth {
            let mut next = vec![];
            for &id in &frontier {
                for c in self.callers(id) {
                    if seen.insert(c) {
                        next.push(c);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            next.sort_unstable();
            levels.push(next.clone());
            frontier = next;
        }
        levels
    }
}
