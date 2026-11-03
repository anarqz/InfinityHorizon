// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! A small map whose iteration order is its slot order: an insert takes the most recently freed
//! slot, else a new one at the end; a removal frees its slot. The engine's observable orders (the
//! objects a joiner gets, the deletes of a leaving player, held opens, eviction of the oldest
//! handshake bucket) follow this order, which is the reference engine's map order, so the replay
//! of a golden vector writes its messages in the same order. A hash index finds a key's slot in
//! constant time (a pre-auth OpenConnection looks several maps up).

use std::{collections::HashMap, hash::Hash};

#[derive(Debug, Clone)]
pub struct DotMap<K, V> {
    slots: Vec<Option<(K, V)>>,
    free: Vec<usize>,
    len: usize,
    index: HashMap<K, usize>,
}

impl<K, V> Default for DotMap<K, V> {
    fn default() -> Self {
        DotMap {
            slots: Vec::new(),
            free: Vec::new(),
            len: 0,
            index: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash + Clone, V> DotMap<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn slot(&self, k: &K) -> Option<usize> {
        self.index.get(k).copied()
    }

    pub fn contains_key(&self, k: &K) -> bool {
        self.slot(k).is_some()
    }

    pub fn get(&self, k: &K) -> Option<&V> {
        self.slot(k)
            .and_then(|i| self.slots[i].as_ref().map(|(_, v)| v))
    }

    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        let i = self.slot(k)?;
        self.slots[i].as_mut().map(|(_, v)| v)
    }

    /// Sets `k` (in place when present).
    pub fn insert(&mut self, k: K, v: V) {
        if let Some(i) = self.slot(&k) {
            self.slots[i] = Some((k, v));
            return;
        }
        self.len += 1;
        let i = match self.free.pop() {
            Some(i) => i,
            None => {
                self.slots.push(None);
                self.slots.len() - 1
            }
        };
        self.index.insert(k.clone(), i);
        self.slots[i] = Some((k, v));
    }

    pub fn remove(&mut self, k: &K) -> Option<V> {
        let i = self.slot(k)?;
        let (_, v) = self.slots[i].take()?;
        self.index.remove(k);
        self.len -= 1;
        // an emptied map keeps its freed slots (only `clear` starts over)
        self.free.push(i);
        Some(v)
    }

    pub fn clear(&mut self) {
        self.slots.clear();
        self.free.clear();
        self.index.clear();
        self.len = 0;
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.slots
            .iter()
            .filter_map(|s| s.as_ref().map(|(k, v)| (k, v)))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&K, &mut V)> {
        self.slots
            .iter_mut()
            .filter_map(|s| s.as_mut().map(|(k, v)| (&*k, v)))
    }

    pub fn keys(&self) -> Vec<K> {
        self.iter().map(|(k, _)| k.clone()).collect()
    }

    pub fn first_key(&self) -> Option<K> {
        self.iter().next().map(|(k, _)| k.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::DotMap;

    #[test]
    fn slots_are_reused_most_recent_first_and_found_by_the_index() {
        let mut m = DotMap::new();
        for k in 0..5u32 {
            m.insert(k, k * 10);
        }
        assert_eq!(m.remove(&1), Some(10));
        assert_eq!(m.remove(&3), Some(30));
        m.insert(7, 70); // takes slot 3 (freed last)
        m.insert(8, 80); // then slot 1
        assert_eq!(m.keys(), vec![0, 8, 2, 7, 4]);
        assert_eq!(m.get(&7), Some(&70));
        assert!(!m.contains_key(&3));
        m.insert(7, 71);
        assert_eq!((m.len(), m.get(&7)), (5, Some(&71)));
        m.clear();
        assert!(m.is_empty() && m.get(&0).is_none());
    }
}
