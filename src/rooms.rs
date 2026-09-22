//! Room for trampolines inside the text of a big program.
//!
//! A hooked site reaches its trampoline with one `b` (±128 MB), and the
//! stub segment sits after `__DATA`, so in a program whose code runs past
//! about 120 MB the early sites reach nothing. The way out is at link
//! time: an object file per room, a `.space` of zeros under a symbol the
//! rewriter knows (`ROOM_PREFIX`), placed in the middle of the text by an
//! order file that lists the symbols meant to come before it (the linker
//! keeps the rest in their usual order after the listed ones). The
//! rewriter then writes the trampolines of sites out of the segment's
//! reach into the room nearest them. `rewrite cc` does the planning and
//! the second link; `rewrite cargo` puts it in cargo's way as the linker.
//!
//! The plan comes from the first link: sites per MB of text, from
//! `Stats::sites_per_mb`, decide each room's size; a room covers 128 MB on
//! either side of itself, less its own size for what comes after it,
//! since inserting it moves that up. Symbols within a megabyte of a
//! function-table entry with no symbol are not listed: such an entry is a
//! constant table the neighbouring code reaches with an `adr`, which the
//! linker would leave behind (it keeps only listed atoms in place), so the
//! neighbourhood goes to the tail together, where the order is kept.

use std::fmt::Write;

use crate::macho::{round_up, MachO, PAGE};
use crate::rewrite::{Stats, ROOM_PREFIX};
use crate::stub::B_RANGE;

pub struct Plan {
    /// Each room's symbol and size, in address order
    pub rooms: Vec<(String, u64)>,
    pub order_file: String,
    /// Sites the segment would not reach
    pub far_sites: u64,
}

/// The longest trampoline
const BYTES_PER_SITE: u64 = 24;
/// An `adr` reaches this far: what stays with an unnamed entry
const NEAR_ENTRY: u64 = 1 << 20;
/// The reachable sites' trampolines come first in the segment; sites this
/// close to the boundary are taken as out of reach too
const MARGIN: u64 = 8 << 20;
/// Room placement is by symbol, a symbol's atom may be long: a room lands
/// somewhat past where it was asked for
const SLACK: u64 = 4 << 20;

/// How much text there is per MB before `hi` (offsets from the text start)
fn sites_in(buckets: &[u32], lo: u64, hi: u64) -> u64 {
    let lo = (lo >> 20) as usize;
    let hi = (hi.div_ceil(1 << 20) as usize).min(buckets.len());
    buckets[lo.min(hi)..hi].iter().map(|&n| u64::from(n)).sum()
}

/// A plan for `m` as linked, or None when every site reaches the segment.
pub fn plan(m: &MachO, stats: &Stats) -> Option<Plan> {
    let text = m.text_section().ok()?;
    let linkedit = m.segment("__LINKEDIT")?;
    let reach = B_RANGE as u64;
    let base = text.addr;
    let segment = linkedit.vmaddr - base;
    let buckets = &stats.sites_per_mb;
    let mut rooms: Vec<(u64, u64)> = Vec::new();
    let mut total = 0;
    // Rooms move the segment up, which puts more sites out of its reach
    for _ in 0..3 {
        // No site lies past the text, however far the rooms push the segment
        let far_end = (segment + total + MARGIN)
            .saturating_sub(reach)
            .min(text.size);
        if far_end == 0 {
            return None;
        }
        rooms.clear();
        total = 0;
        let mut from = 0;
        while from < far_end {
            // A room of z bytes at p serves [p - (R - z), p + (R - z)]: its
            // last trampoline must be within reach of the stretch's first
            // site, its first of the stretch's last site, which the room
            // itself moves up by z. So a stretch is L = 2 (R - z - slack)
            // long, and z is the stretch's sites' worth: with d bytes of
            // trampoline per byte of text, z = 2 d (R - slack) / (1 + 2 d).
            // The density is the stretch's own, so settle the two together.
            let size = |n: u64| round_up(n * BYTES_PER_SITE * 11 / 10 + 4096, PAGE);
            // With b bytes of trampoline for the stretch's len bytes of
            // text, d = b / len and z = 2 b (R - slack) / (len + 2 b).
            let mut len = 2 * reach;
            for _ in 0..8 {
                let end = (from + len).min(text.size);
                let b = size(sites_in(buckets, from, end));
                let z = 2 * b * (reach - SLACK) / ((end - from).max(1) + 2 * b);
                let again = (2 * (reach - SLACK).saturating_sub(z)).max(1 << 20);
                len = (len + again) / 2;
            }
            let next = (from + len).min(text.size);
            let bytes = size(sites_in(buckets, from, next));
            let at = (from + reach.saturating_sub(bytes + SLACK))
                .max(from + 1)
                .min(text.size);
            rooms.push((at, bytes));
            total += bytes;
            if at >= text.size {
                break;
            }
            from = next.max(at + 1);
        }
    }
    let symbols = m.symbols();
    let excluded = |addr: u64| {
        stats
            .unnamed_addrs
            .iter()
            .any(|&u| addr.abs_diff(u) < NEAR_ENTRY)
    };
    let mut order = String::new();
    let mut next_symbol = 0;
    for (k, &(at, _)) in rooms.iter().enumerate() {
        // A listed atom ends before the room: list a symbol only when the
        // next one begins by `at`
        while next_symbol + 1 < symbols.len() && symbols[next_symbol + 1].0 <= base + at {
            let (addr, name) = &symbols[next_symbol];
            if !excluded(*addr) {
                order.push_str(name);
                order.push('\n');
            }
            next_symbol += 1;
        }
        let _ = writeln!(order, "{ROOM_PREFIX}{k}");
    }
    let far_end = (segment + total + MARGIN)
        .saturating_sub(reach)
        .min(text.size);
    Some(Plan {
        rooms: rooms
            .iter()
            .enumerate()
            .map(|(k, &(_, bytes))| (format!("{ROOM_PREFIX}{k}"), bytes))
            .collect(),
        order_file: order,
        far_sites: sites_in(buckets, 0, far_end),
    })
}

/// The assembly of a room: `bytes` of nothing under `name`, kept through
/// dead stripping.
#[must_use]
pub fn assembly(name: &str, bytes: u64) -> String {
    format!(".text\n.globl {name}\n.no_dead_strip {name}\n.p2align 14\n{name}:\n.space {bytes}\n")
}
