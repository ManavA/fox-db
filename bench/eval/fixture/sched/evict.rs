/// Kicks pods off a node when it runs short on memory or disk, lowest priority first.
pub fn evict_under_pressure(node: &mut Node) -> Vec<PodId> {
    let mut out = vec![];
    while node.pressure() {
        match node.lowest_priority_pod() {
            Some(p) => {
                node.remove(p);
                out.push(p);
            }
            None => break,
        }
    }
    out
}

/// Node memory and disk thresholds from the config file.
pub struct Thresholds {
    pub memory_mb: u64,
    pub disk_pct: u8,
}
