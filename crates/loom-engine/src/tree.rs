//! PID-reuse-safe process tree builder and bottom-up termination ordering.
//!
//! Operating systems aggressively recycle exited PIDs. This module mounts a
//! child under a parent process only when [`ProcessKey::is_valid_parent_of`]
//! holds (`parent.pid != child.pid && parent.create_time_100ns <= child.create_time_100ns`),
//! explicitly breaks any equal-timestamp mutual cycles during construction,
//! and double-guards all traversals with a visited set.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hasher};

use loom_types::ProcessKey;

/// Lightweight, deterministic non-cryptographic hasher for trusted integer / [`ProcessKey`] maps.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct FastBuildHasher;

impl BuildHasher for FastBuildHasher {
    type Hasher = FastHasher;

    #[inline]
    fn build_hasher(&self) -> Self::Hasher {
        FastHasher(0)
    }
}

/// Fast multiplicative hasher (`FxHash`-style) avoiding SipHash overhead on hot sampling paths.
pub(crate) struct FastHasher(u64);

impl FastHasher {
    const K: u64 = 0x517c_c1b7_2722_0a95;

    #[inline]
    pub(crate) const fn new() -> Self {
        Self(0)
    }

    #[inline]
    fn mix(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(Self::K);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rem) = bytes.as_chunks::<8>();
        for &chunk in chunks {
            self.mix(u64::from_ne_bytes(chunk));
        }
        if !rem.is_empty() {
            let mut tail = 0u64;
            for (i, &b) in rem.iter().enumerate() {
                tail |= u64::from(b) << (i * 8);
            }
            self.mix(tail);
        }
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.mix(u64::from(i));
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.mix(i);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.mix(i as u64);
    }
}

/// Verified acyclic parent-child hierarchy of live processes keyed by [`ProcessKey`].
#[derive(Debug, Default, Clone)]
pub struct ProcessTree {
    nodes: Vec<ProcessKey>,
    key_to_idx: HashMap<ProcessKey, usize, FastBuildHasher>,
    pid_to_idx: HashMap<u32, usize, FastBuildHasher>,
    parent_idx: Vec<Option<usize>>,
    children_keys: Vec<Vec<ProcessKey>>,
    children_indices: Vec<Vec<usize>>,
    roots: Vec<ProcessKey>,
    parent_pids_scratch: Vec<u32>,
    state_scratch: Vec<u8>,
    chain_scratch: Vec<usize>,
}

impl ProcessTree {
    /// Builds a PID-reuse-safe, cycle-free [`ProcessTree`] from `(ProcessKey, parent_pid)` pairs.
    ///
    /// - A child attaches to `parent` only when `parent.is_valid_parent_of(child)`.
    ///   Otherwise the child is promoted to a root node.
    /// - Equal-timestamp mutual cycles (e.g., `A -> B -> A` created at the exact
    ///   same `100ns` tick) are detected and broken deterministically by promoting
    ///   the cycle back-edge node to a root.
    pub fn build(entries: &[(ProcessKey, u32)]) -> Self {
        let mut tree = Self::default();
        tree.rebuild(entries);
        tree
    }

    /// Rebuilds this [`ProcessTree`] in place from `entries`, reusing internal buffer capacities.
    pub fn rebuild(&mut self, entries: &[(ProcessKey, u32)]) {
        let cap = entries.len();
        self.nodes.clear();
        self.key_to_idx.clear();
        self.pid_to_idx.clear();
        self.parent_idx.clear();
        self.roots.clear();
        self.parent_pids_scratch.clear();

        self.nodes.reserve(cap);
        self.parent_pids_scratch.reserve(cap);
        if self.key_to_idx.capacity() < cap {
            self.key_to_idx.reserve(cap);
        }
        if self.pid_to_idx.capacity() < cap {
            self.pid_to_idx.reserve(cap);
        }

        // Dense indexing of unique ProcessKeys plus PID -> oldest live index lookup.
        for &(key, parent_pid) in entries {
            if self.key_to_idx.contains_key(&key) {
                continue;
            }
            let idx = self.nodes.len();
            self.nodes.push(key);
            self.parent_pids_scratch.push(parent_pid);
            self.key_to_idx.insert(key, idx);

            self.pid_to_idx
                .entry(key.pid)
                .and_modify(|existing_idx| {
                    if key.create_time_100ns < self.nodes[*existing_idx].create_time_100ns {
                        *existing_idx = idx;
                    }
                })
                .or_insert(idx);
        }

        let n = self.nodes.len();
        self.parent_idx.resize(n, None);

        // Step 1: Resolve tentative parent edges validated by creation timestamp.
        for idx in 0..n {
            let key = self.nodes[idx];
            let ppid = self.parent_pids_scratch[idx];
            if let Some(&p_idx) = self.pid_to_idx.get(&ppid)
                && self.nodes[p_idx].is_valid_parent_of(key)
            {
                self.parent_idx[idx] = Some(p_idx);
            }
        }

        // Step 2: Detect and break any cycles (e.g., equal-timestamp mutual cycles).
        // State: 0 = unvisited, 1 = visiting on current chain, 2 = verified acyclic.
        self.state_scratch.clear();
        self.state_scratch.resize(n, 0);
        for start in 0..n {
            if self.state_scratch[start] == 2 {
                continue;
            }
            self.chain_scratch.clear();
            let mut curr = start;
            loop {
                self.state_scratch[curr] = 1;
                self.chain_scratch.push(curr);

                let Some(p_idx) = self.parent_idx[curr] else {
                    break;
                };
                match self.state_scratch[p_idx] {
                    2 => break,
                    1 => {
                        // Back-edge detected: break the cycle by promoting `curr` to root.
                        self.parent_idx[curr] = None;
                        break;
                    }
                    _ => {
                        curr = p_idx;
                    }
                }
            }
            for &node_idx in &self.chain_scratch {
                self.state_scratch[node_idx] = 2;
            }
        }

        // Step 3: Materialize roots and adjacency lists in deterministic input order.
        if self.children_keys.len() < n {
            self.children_keys.resize_with(n, Vec::new);
            self.children_indices.resize_with(n, Vec::new);
        }
        for i in 0..n {
            self.children_keys[i].clear();
            self.children_indices[i].clear();
        }

        for idx in 0..n {
            let key = self.nodes[idx];
            if let Some(p_idx) = self.parent_idx[idx] {
                self.children_keys[p_idx].push(key);
                self.children_indices[p_idx].push(idx);
            } else {
                self.roots.push(key);
            }
        }
    }

    /// Returns the slice of top-level root [`ProcessKey`]s.
    #[inline]
    pub fn roots(&self) -> &[ProcessKey] {
        &self.roots
    }

    /// Returns the verified parent [`ProcessKey`] of `key`, if any.
    #[inline]
    pub fn parent(&self, key: ProcessKey) -> Option<ProcessKey> {
        let &idx = self.key_to_idx.get(&key)?;
        self.parent_idx[idx].map(|p| self.nodes[p])
    }

    /// Returns the direct children slice of `key`, or an empty slice if it has none.
    #[inline]
    pub fn children(&self, key: ProcessKey) -> &[ProcessKey] {
        self.key_to_idx
            .get(&key)
            .map(|&idx| self.children_keys[idx].as_slice())
            .unwrap_or(&[])
    }

    /// Returns `true` if `key` exists in this tree.
    #[inline]
    pub fn contains(&self, key: ProcessKey) -> bool {
        self.key_to_idx.contains_key(&key)
    }

    /// Returns the total number of distinct process nodes in this tree.
    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Returns `true` if this tree contains no processes.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    #[inline]
    pub(crate) fn parent_index(&self, idx: usize) -> Option<usize> {
        self.parent_idx.get(idx).copied().flatten()
    }

    #[inline]
    pub(crate) fn children_indices(&self, idx: usize) -> &[usize] {
        if idx >= self.nodes.len() {
            return &[];
        }
        self.children_indices
            .get(idx)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    #[inline]
    pub(crate) fn key_at(&self, idx: usize) -> ProcessKey {
        self.nodes[idx]
    }
}

/// Collects a bottom-up (post-order DFS) termination sequence rooted at `root_key`.
///
/// Descendants are returned leaves-first and `root_key` is always placed **last**
/// so watchdog parent processes cannot respawn children mid-termination. Uses an
/// iterative stack and a visited set so deep chains or corrupted inputs can never
/// overflow the stack or loop indefinitely.
pub fn collect_bottom_up_kill_order(tree: &ProcessTree, root_key: ProcessKey) -> Vec<ProcessKey> {
    let mut order = Vec::new();
    let mut visited = HashSet::with_hasher(FastBuildHasher);
    let mut stack: Vec<(ProcessKey, bool)> = Vec::new();

    if visited.insert(root_key) {
        stack.push((root_key, false));
    }

    while let Some((node, expanded)) = stack.pop() {
        if expanded {
            order.push(node);
        } else {
            stack.push((node, true));
            for &child in tree.children(node).iter().rev() {
                if visited.insert(child) {
                    stack.push((child, false));
                }
            }
        }
    }

    order
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_reused_newer_parent_and_promotes_child_to_root() {
        let reused_parent = ProcessKey::new(100, 5_000);
        let older_child = ProcessKey::new(200, 1_000);

        let tree = ProcessTree::build(&[(reused_parent, 0), (older_child, 100)]);

        assert_eq!(tree.parent(older_child), None);
        assert!(tree.roots().contains(&reused_parent));
        assert!(tree.roots().contains(&older_child));
        assert!(tree.children(reused_parent).is_empty());
    }

    #[test]
    fn breaks_equal_timestamp_mutual_cycle_without_looping() {
        let a = ProcessKey::new(10, 1_000);
        let b = ProcessKey::new(20, 1_000);

        let tree = ProcessTree::build(&[(a, 20), (b, 10)]);

        assert_eq!(tree.len(), 2);
        assert_eq!(tree.roots().len(), 1);
        let root = tree.roots()[0];
        let kill_order = collect_bottom_up_kill_order(&tree, root);
        assert_eq!(kill_order.len(), 2);
        assert_eq!(*kill_order.last().unwrap(), root);
    }

    #[test]
    fn bottom_up_kill_order_returns_leaves_before_parents_and_root_last() {
        let root = ProcessKey::new(1, 100);
        let mid = ProcessKey::new(2, 200);
        let leaf1 = ProcessKey::new(3, 300);
        let leaf2 = ProcessKey::new(4, 301);

        let tree = ProcessTree::build(&[(root, 0), (mid, 1), (leaf1, 2), (leaf2, 2)]);
        let order = collect_bottom_up_kill_order(&tree, root);

        assert_eq!(order, vec![leaf1, leaf2, mid, root]);
    }

    #[test]
    fn rebuild_shrink_never_exposes_stale_trailing_children_indices() {
        let root = ProcessKey::new(1, 100);
        let child = ProcessKey::new(2, 200);
        let grandchild = ProcessKey::new(3, 300);

        let mut tree = ProcessTree::build(&[(root, 0), (child, 1), (grandchild, 2)]);
        assert_eq!(tree.children_indices(1), &[2]);

        tree.rebuild(&[(root, 0)]);
        assert_eq!(tree.len(), 1);
        assert!(tree.children_indices(0).is_empty());
        assert!(tree.children_indices(1).is_empty());
    }
}
