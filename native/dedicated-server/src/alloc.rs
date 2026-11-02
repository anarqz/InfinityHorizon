// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The counting allocator behind the public profile's memory budget (contract §8): the system
//! allocator plus one relaxed atomic counter of the live bytes. The binary installs it as the
//! global allocator; a library user that does not reads 0.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicUsize, Ordering},
};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

pub struct CountingAlloc;

#[allow(unsafe_code)]
// SAFETY: every call forwards to the system allocator with the caller's own arguments; the
// counters never affect the memory returned.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged (GlobalAlloc's contract is the caller's).
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            add(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            add(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged; `ptr` came from this allocator with `layout`.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            add(new_size);
        }
        p
    }
}

fn add(n: usize) {
    let live = LIVE.fetch_add(n, Ordering::Relaxed) + n;
    if live > PEAK.load(Ordering::Relaxed) {
        PEAK.store(live, Ordering::Relaxed);
    }
}

/// Live heap bytes (0 when the counting allocator is not installed).
pub fn live_bytes() -> usize {
    LIVE.load(Ordering::Relaxed)
}

pub fn peak_bytes() -> usize {
    PEAK.load(Ordering::Relaxed)
}
