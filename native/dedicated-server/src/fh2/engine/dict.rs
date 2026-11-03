// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! A small map whose iteration order is its slot order: an insert takes the most recently freed
//! slot, else a new one at the end; a removal frees its slot. The engine's observable orders (the
//! objects a joiner gets, the deletes of a leaving player, held opens, eviction of the oldest
//! handshake bucket) follow this order, which is the reference engine's map order, so the replay
//! of a golden vector writes its messages in the same order.

#[derive(Debug, Clone)]
pub struct DotMap<K, V> {
    slots: Vec<Option<(K, V)>>,
    free: Vec<usize>,
    len: usize,
}

impl<K, V> Default for DotMap<K, V> {
    fn default() -> Self {
        DotMap {
            slots: Vec::new(),
            free: Vec::new(),
            len: 0,
        }
    }
}

impl<K: PartialEq + Clone, V> DotMap<K, V> {
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
        self.slots
            .iter()
            .position(|s| s.as_ref().is_some_and(|(key, _)| key == k))
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
        if let Some(i) = self.free.pop() {
            self.slots[i] = Some((k, v));
        } else {
            self.slots.push(Some((k, v)));
        }
    }

    pub fn remove(&mut self, k: &K) -> Option<V> {
        let i = self.slot(k)?;
        let (_, v) = self.slots[i].take()?;
        self.len -= 1;
        // an emptied map keeps its freed slots (only `clear` starts over)
        self.free.push(i);
        Some(v)
    }

    pub fn clear(&mut self) {
        self.slots.clear();
        self.free.clear();
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
