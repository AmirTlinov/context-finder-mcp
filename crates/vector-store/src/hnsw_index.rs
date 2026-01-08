use crate::error::{Result, VectorStoreError};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

const DEFAULT_M: usize = 16;
const DEFAULT_EF_CONSTRUCTION: usize = 64;
const DEFAULT_EF_SEARCH: usize = 64;
const BRUTE_FORCE_THRESHOLD: usize = 2048;

pub struct HnswIndex {
    dimension: usize,
    nodes: Vec<Node>,
    id_to_node: HashMap<usize, usize>,
    entry_point: Option<usize>,
    max_level: usize,
    m: usize,
    ef_construction: usize,
    ef_search: usize,
    rng: XorShift64,
    tombstones: usize,
}

impl HnswIndex {
    pub fn new(dimension: usize) -> Self {
        Self {
            dimension,
            nodes: Vec::new(),
            id_to_node: HashMap::new(),
            entry_point: None,
            max_level: 0,
            m: DEFAULT_M,
            ef_construction: DEFAULT_EF_CONSTRUCTION,
            ef_search: DEFAULT_EF_SEARCH,
            rng: XorShift64::new(0x243F_6A88_85A3_08D3),
            tombstones: 0,
        }
    }

    /// Add a vector to the index (normalizes it for cosine similarity).
    pub fn add(&mut self, id: usize, vector: &[f32]) -> Result<()> {
        self.add_owned(id, vector.to_vec())
    }

    /// Add an owned vector to the index (normalizes it for cosine similarity).
    pub fn add_owned(&mut self, id: usize, mut vector: Vec<f32>) -> Result<()> {
        if vector.len() != self.dimension {
            return Err(VectorStoreError::InvalidDimension {
                expected: self.dimension,
                actual: vector.len(),
            });
        }
        normalize_in_place(&mut vector);
        self.add_shared(id, Arc::new(vector))
    }

    /// Add a shared (already-normalized) vector to the index.
    ///
    /// This is the preferred API when the caller already owns/stores the normalized vector and
    /// wants to avoid duplicating memory.
    pub fn add_shared(&mut self, id: usize, vector: Arc<Vec<f32>>) -> Result<()> {
        if vector.len() != self.dimension {
            return Err(VectorStoreError::InvalidDimension {
                expected: self.dimension,
                actual: vector.len(),
            });
        }

        self.remove(id);
        self.insert_node(id, vector);
        Ok(())
    }

    /// Search for `k` nearest neighbors using cosine similarity.
    /// Returns `(id, score)` sorted by score descending.
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(usize, f32)>> {
        if query.len() != self.dimension {
            return Err(VectorStoreError::InvalidDimension {
                expected: self.dimension,
                actual: query.len(),
            });
        }

        let limit = k.min(self.id_to_node.len());
        if limit == 0 {
            return Ok(vec![]);
        }

        let inv_query_norm = inverse_norm(query);
        if inv_query_norm == 0.0 {
            // All cosine scores are identical (0.0). Return smallest ids for determinism.
            let mut ids: Vec<usize> = self.id_to_node.keys().copied().collect();
            ids.sort();
            ids.truncate(limit);
            return Ok(ids.into_iter().map(|id| (id, 0.0)).collect());
        }

        if self.id_to_node.len() <= BRUTE_FORCE_THRESHOLD {
            return Ok(self.brute_force_search(query, inv_query_norm, limit));
        }

        let Some(entry) = self.any_entry_point() else {
            return Ok(vec![]);
        };

        // HNSW search: greedy descent through upper layers, then ef-search on layer 0.
        let mut ep = entry;
        for level in (1..=self.max_level).rev() {
            ep = self.greedy_search_level(query, inv_query_norm, ep, level);
        }

        let ef = self
            .ef_search
            .max(limit.saturating_mul(4))
            .min(self.id_to_node.len());
        let candidates = self.search_layer(query, inv_query_norm, ep, ef, 0);

        let mut results: Vec<(usize, f32)> = Vec::with_capacity(candidates.len());
        for cand in candidates {
            let Some(node) = self.nodes.get(cand.idx) else {
                continue;
            };
            let Some(vector) = node.vector.as_ref() else {
                continue;
            };
            let score = dot(query, vector) * inv_query_norm;
            results.push((node.id, score));
        }

        results.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        results.truncate(limit);
        Ok(results)
    }

    /// Remove a vector from the index (best-effort; missing ids are ignored).
    pub fn remove(&mut self, id: usize) {
        let Some(node_idx) = self.id_to_node.remove(&id) else {
            return;
        };
        if let Some(node) = self.nodes.get_mut(node_idx) {
            if node.vector.take().is_some() {
                self.tombstones = self.tombstones.saturating_add(1);
            }
        }

        if matches!(self.entry_point, Some(ep) if ep == node_idx) {
            self.entry_point = self.id_to_node.values().next().copied();
        }

        if self.should_rebuild() {
            self.rebuild();
        }
    }

    /// Get number of vectors in index
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.id_to_node.len()
    }

    /// Check if index is empty
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.id_to_node.is_empty()
    }

    /// Clear all vectors
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.id_to_node.clear();
        self.entry_point = None;
        self.max_level = 0;
        self.tombstones = 0;
    }

    fn brute_force_search(
        &self,
        query: &[f32],
        inv_query_norm: f32,
        limit: usize,
    ) -> Vec<(usize, f32)> {
        let mut heap: BinaryHeap<WorstCandidate> = BinaryHeap::with_capacity(limit);

        for (&id, &node_idx) in &self.id_to_node {
            let Some(vector) = self.nodes.get(node_idx).and_then(|n| n.vector.as_ref()) else {
                continue;
            };
            let score = dot(query, vector) * inv_query_norm;
            let candidate = WorstCandidate { id, score };

            if heap.len() < limit {
                heap.push(candidate);
                continue;
            }

            let worst = *heap
                .peek()
                .unwrap_or_else(|| unreachable!("heap is non-empty when len==limit"));
            if candidate < worst {
                heap.pop();
                heap.push(candidate);
            }
        }

        let mut results: Vec<(usize, f32)> = heap.into_iter().map(|c| (c.id, c.score)).collect();
        results.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        results
    }

    fn any_entry_point(&self) -> Option<usize> {
        match self.entry_point {
            Some(idx) => self
                .nodes
                .get(idx)
                .and_then(|n| n.vector.as_ref())
                .map(|_| idx)
                .or_else(|| self.id_to_node.values().next().copied()),
            None => self.id_to_node.values().next().copied(),
        }
    }

    fn insert_node(&mut self, id: usize, vector: Arc<Vec<f32>>) {
        let level = self.sample_level();
        let node_idx = self.nodes.len();
        self.nodes.push(Node::new(id, vector.clone(), level));
        self.id_to_node.insert(id, node_idx);

        let Some(mut entry) = self.any_entry_point().filter(|idx| *idx != node_idx) else {
            self.entry_point = Some(node_idx);
            self.max_level = level;
            return;
        };

        let query = vector.as_slice();
        let inv_query_norm = 1.0; // vectors are normalized on insert

        if self.max_level > level {
            for layer in ((level + 1)..=self.max_level).rev() {
                entry = self.greedy_search_level(query, inv_query_norm, entry, layer);
            }
        }

        let upper = self.max_level.min(level);
        for layer in (0..=upper).rev() {
            let ef = self
                .ef_construction
                .max(self.m)
                .min(self.id_to_node.len().max(1));
            let candidates = self.search_layer(query, inv_query_norm, entry, ef, layer);
            let max_degree = if layer == 0 { self.m * 2 } else { self.m };
            let selected = self.select_neighbors(&candidates, max_degree);

            self.nodes[node_idx].neighbors[layer] = selected.clone();
            for &neighbor_idx in &selected {
                self.link_bidirectional(node_idx, neighbor_idx, layer);
            }

            if let Some(best) = candidates.iter().min_by(|a, b| {
                a.distance
                    .total_cmp(&b.distance)
                    .then_with(|| a.idx.cmp(&b.idx))
            }) {
                entry = best.idx;
            }
        }

        if level > self.max_level {
            self.entry_point = Some(node_idx);
            self.max_level = level;
        }
    }

    fn greedy_search_level(
        &self,
        query: &[f32],
        inv_query_norm: f32,
        entry: usize,
        level: usize,
    ) -> usize {
        let mut current = entry;
        let mut current_dist = self.distance_to_idx(query, inv_query_norm, current);

        loop {
            let Some(neighbors) = self.nodes.get(current).and_then(|n| n.neighbors.get(level))
            else {
                break;
            };
            let neighbor_snapshot: Vec<usize> = neighbors.to_vec();

            let mut improved = false;
            for candidate in neighbor_snapshot {
                if self
                    .nodes
                    .get(candidate)
                    .and_then(|n| n.vector.as_ref())
                    .is_none()
                {
                    continue;
                }
                let d = self.distance_to_idx(query, inv_query_norm, candidate);
                if d < current_dist
                    || (d.total_cmp(&current_dist) == Ordering::Equal && candidate < current)
                {
                    current = candidate;
                    current_dist = d;
                    improved = true;
                }
            }

            if !improved {
                break;
            }
        }

        current
    }

    fn search_layer(
        &self,
        query: &[f32],
        inv_query_norm: f32,
        entry: usize,
        ef: usize,
        level: usize,
    ) -> Vec<NeighborCandidate> {
        let mut visited: HashSet<usize> = HashSet::with_capacity(ef.saturating_mul(2));
        let mut candidates: BinaryHeap<std::cmp::Reverse<NeighborCandidate>> = BinaryHeap::new();
        let mut top: BinaryHeap<NeighborCandidate> = BinaryHeap::new(); // worst-first

        let entry_dist = self.distance_to_idx(query, inv_query_norm, entry);
        let entry_cand = NeighborCandidate {
            idx: entry,
            distance: entry_dist,
        };
        candidates.push(std::cmp::Reverse(entry_cand));
        top.push(entry_cand);
        visited.insert(entry);

        while let Some(std::cmp::Reverse(current)) = candidates.pop() {
            let worst_top = *top
                .peek()
                .unwrap_or_else(|| unreachable!("top is non-empty when candidates is non-empty"));
            if current.distance > worst_top.distance {
                break;
            }

            let Some(neighbors) = self
                .nodes
                .get(current.idx)
                .and_then(|n| n.neighbors.get(level))
            else {
                continue;
            };
            let neighbor_snapshot: Vec<usize> = neighbors.to_vec();
            for candidate_idx in neighbor_snapshot {
                if !visited.insert(candidate_idx) {
                    continue;
                }
                if self
                    .nodes
                    .get(candidate_idx)
                    .and_then(|n| n.vector.as_ref())
                    .is_none()
                {
                    continue;
                }

                let d = self.distance_to_idx(query, inv_query_norm, candidate_idx);
                let cand = NeighborCandidate {
                    idx: candidate_idx,
                    distance: d,
                };

                if top.len() < ef {
                    candidates.push(std::cmp::Reverse(cand));
                    top.push(cand);
                    continue;
                }

                let worst = *top
                    .peek()
                    .unwrap_or_else(|| unreachable!("top is non-empty when len==ef"));
                if cand < worst {
                    top.pop();
                    top.push(cand);
                    candidates.push(std::cmp::Reverse(cand));
                }
            }
        }

        top.into_iter().collect()
    }

    fn select_neighbors(&self, candidates: &[NeighborCandidate], max_degree: usize) -> Vec<usize> {
        let mut items = candidates.to_vec();
        items.sort_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then_with(|| a.idx.cmp(&b.idx))
        });
        items.into_iter().take(max_degree).map(|c| c.idx).collect()
    }

    fn link_bidirectional(&mut self, a: usize, b: usize, level: usize) {
        if a == b {
            return;
        }
        if self.nodes.get(a).and_then(|n| n.vector.as_ref()).is_none()
            || self.nodes.get(b).and_then(|n| n.vector.as_ref()).is_none()
        {
            return;
        }
        if self
            .nodes
            .get(a)
            .and_then(|n| n.neighbors.get(level))
            .is_none()
            || self
                .nodes
                .get(b)
                .and_then(|n| n.neighbors.get(level))
                .is_none()
        {
            return;
        }

        {
            let neighbors = &mut self.nodes[a].neighbors[level];
            if !neighbors.contains(&b) {
                neighbors.push(b);
            }
        }
        {
            let neighbors = &mut self.nodes[b].neighbors[level];
            if !neighbors.contains(&a) {
                neighbors.push(a);
            }
        }

        let max_degree = if level == 0 { self.m * 2 } else { self.m };
        self.prune_neighbors(a, level, max_degree);
        self.prune_neighbors(b, level, max_degree);
    }

    fn prune_neighbors(&mut self, base: usize, level: usize, max_degree: usize) {
        let Some(base_vec) = self
            .nodes
            .get(base)
            .and_then(|n| n.vector.as_ref())
            .cloned()
        else {
            return;
        };
        let Some(existing) = self.nodes.get(base).and_then(|n| n.neighbors.get(level)) else {
            return;
        };
        if existing.len() <= max_degree {
            return;
        }

        let neighbor_snapshot: Vec<usize> = existing.to_vec();
        let mut scored: Vec<(usize, f32)> = Vec::with_capacity(neighbor_snapshot.len());
        for idx in neighbor_snapshot {
            let Some(vec) = self.nodes.get(idx).and_then(|n| n.vector.as_ref()) else {
                continue;
            };
            let d = 1.0 - dot(&base_vec, vec);
            scored.push((idx, d));
        }

        scored.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(max_degree);

        let mut kept: Vec<usize> = scored.into_iter().map(|(idx, _)| idx).collect();
        kept.sort_unstable();
        kept.dedup();

        if let Some(neighbors_mut) = self
            .nodes
            .get_mut(base)
            .and_then(|n| n.neighbors.get_mut(level))
        {
            *neighbors_mut = kept;
        }
    }

    fn distance_to_idx(&self, query: &[f32], inv_query_norm: f32, idx: usize) -> f32 {
        let Some(vector) = self.nodes.get(idx).and_then(|n| n.vector.as_ref()) else {
            return f32::INFINITY;
        };
        let score = dot(query, vector) * inv_query_norm;
        1.0 - score
    }

    fn sample_level(&mut self) -> usize {
        // Standard HNSW exponential distribution for levels:
        // level = floor(-ln(u) * (1/ln(m)))
        let u = self.rng.next_f64().clamp(f64::MIN_POSITIVE, 1.0);
        let m = self.m.max(2) as f64;
        let level_mult = 1.0 / m.ln();
        let level = (-u.ln() * level_mult).floor();
        usize::try_from(level as i64).unwrap_or(0).min(32)
    }

    fn should_rebuild(&self) -> bool {
        if self.tombstones < 1024 {
            return false;
        }
        let live = self.id_to_node.len();
        let total = self.nodes.len().max(1);
        let dead = total.saturating_sub(live);
        dead.saturating_mul(4) > total // >25% dead
    }

    fn rebuild(&mut self) {
        let mut pairs: Vec<(usize, Arc<Vec<f32>>)> = self
            .nodes
            .iter()
            .filter_map(|n| n.vector.as_ref().map(|v| (n.id, v.clone())))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));

        let mut rebuilt = HnswIndex::new(self.dimension);
        rebuilt.m = self.m;
        rebuilt.ef_construction = self.ef_construction;
        rebuilt.ef_search = self.ef_search;
        for (id, vec) in pairs {
            rebuilt.insert_node(id, vec);
        }
        *self = rebuilt;
    }
}

#[derive(Clone)]
struct Node {
    id: usize,
    vector: Option<Arc<Vec<f32>>>,
    neighbors: Vec<Vec<usize>>,
}

impl Node {
    fn new(id: usize, vector: Arc<Vec<f32>>, level: usize) -> Self {
        Self {
            id,
            vector: Some(vector),
            neighbors: vec![Vec::new(); level.saturating_add(1)],
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct NeighborCandidate {
    idx: usize,
    distance: f32,
}

impl PartialEq for NeighborCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.idx == other.idx && self.distance.total_cmp(&other.distance) == Ordering::Equal
    }
}

impl Eq for NeighborCandidate {}

impl PartialOrd for NeighborCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NeighborCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // Worst-first ordering for a max-heap:
        // - larger distance is worse,
        // - for ties, larger idx is worse (deterministic).
        self.distance
            .total_cmp(&other.distance)
            .then_with(|| self.idx.cmp(&other.idx))
    }
}

#[derive(Debug, Clone, Copy)]
struct WorstCandidate {
    id: usize,
    score: f32,
}

impl PartialEq for WorstCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.score.total_cmp(&other.score) == Ordering::Equal
    }
}

impl Eq for WorstCandidate {}

impl PartialOrd for WorstCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for WorstCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // "Worst first" ordering for a max-heap:
        // - lower score is worse (comes first),
        // - for ties, larger id is worse (deterministic).
        other
            .score
            .total_cmp(&self.score)
            .then_with(|| self.id.cmp(&other.id))
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

fn inverse_norm(vector: &[f32]) -> f32 {
    let norm_sq: f32 = vector.iter().map(|x| x * x).sum();
    if norm_sq > 0.0 {
        1.0 / norm_sq.sqrt()
    } else {
        0.0
    }
}

pub(crate) fn normalize_in_place(vector: &mut [f32]) {
    let inv_norm = inverse_norm(vector);
    if inv_norm == 0.0 {
        return;
    }
    for value in vector {
        *value *= inv_norm;
    }
}

#[derive(Debug, Clone)]
struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn next_f64(&mut self) -> f64 {
        // [0,1)
        let value = self.next_u64() >> 11; // 53 bits
        (value as f64) * (1.0 / ((1u64 << 53) as f64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_search() {
        let mut index = HnswIndex::new(3);

        // Add some vectors
        index.add(0, &[1.0, 0.0, 0.0]).unwrap();
        index.add(1, &[0.9, 0.1, 0.0]).unwrap();
        index.add(2, &[0.0, 1.0, 0.0]).unwrap();

        assert_eq!(index.len(), 3);

        // Search for nearest to [1, 0, 0]
        let results = index.search(&[1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(results.len(), 2);

        // First result should be id=0 (exact match)
        assert_eq!(results[0].0, 0);
        assert!((results[0].1 - 1.0).abs() < 1e-6);

        // Second should be id=1 (close)
        assert_eq!(results[1].0, 1);
        assert!(results[1].1 > 0.9);
    }

    #[test]
    fn test_dimension_mismatch() {
        let mut index = HnswIndex::new(3);
        let result = index.add(0, &[1.0, 0.0]); // Wrong dimension
        assert!(result.is_err());

        index.add(0, &[1.0, 0.0, 0.0]).unwrap();
        let result = index.search(&[1.0, 0.0], 1); // Wrong query dimension
        assert!(result.is_err());
    }

    #[test]
    fn search_ties_are_deterministic_and_prefer_smaller_ids() {
        let mut index = HnswIndex::new(2);
        index.add(10, &[1.0, 0.0]).unwrap();
        index.add(5, &[1.0, 0.0]).unwrap();
        index.add(7, &[1.0, 0.0]).unwrap();

        // Zero vector query makes all cosine scores identical (0.0).
        let results = index.search(&[0.0, 0.0], 2).unwrap();
        assert_eq!(results, vec![(5, 0.0), (7, 0.0)]);
    }
}
