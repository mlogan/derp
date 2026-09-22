//! Rewrites the program text: every hooked site becomes a branch to a
//! trampoline in `__STUB` that calls the segment's one shared body, which
//! decrements the run's quantum counter and enters the scheduler when it
//! expires, and then performs the original branch or memory access. The
//! trampoline is four to six words; what names the site to the scheduler
//! is the trampoline's return address, which the body leaves on the stack.
//! Big programs hook millions of sites, and every site must reach its
//! trampoline, and the trampoline its target, with one `b` (±128 MB).
//!
//! The counter and the scheduler slot are not in the image: they live in
//! the fixed region the supervisor sets up at `shared::STUB_BASE`, which
//! the body reaches with one `movz`. A default-linked binary has header
//! room for one new segment but not for a second, writable one, so a
//! rewritten binary only runs with the supervisor injected.

use crate::decode::{self, Class, FP, SP};
use crate::macho::{self, MachO};
use crate::rng::Rng;
use crate::shared::{COUNTER_ENTRY_OFFSET, COUNTER_OFFSET, SLOT_OFFSET, STUB_BASE};
use crate::stub;

/// Read-only header at the start of `__STUB`, in front of the stub code.
/// Offsets are stable; the supervisor reads them.
pub const HEADER_MAGIC: u32 = 0;
pub const HEADER_SITES: u32 = 8;
pub const HEADER_MEM_SITES: u32 = 16;
pub const HEADER_SEED: u32 = 24;
pub const HEADER_SIZE: u64 = 32;
pub const MAGIC: u64 = 0x0033_3030_5453_5752; // "RWST003" little-endian

#[derive(Debug, Clone)]
pub struct Options {
    pub seed: u64,
    /// Probability of hooking a candidate memory instruction, as a fraction
    pub mem_rate: (u32, u32),
}

impl Default for Options {
    fn default() -> Self {
        Options {
            seed: 0,
            mem_rate: (0, 1),
        }
    }
}

/// Parse `0`, `1`, `1/16`, `3/8` into a fraction
pub fn parse_rate(s: &str) -> Option<(u32, u32)> {
    if let Some((n, d)) = s.split_once('/') {
        let (n, d) = (n.trim().parse().ok()?, d.trim().parse().ok()?);
        (d > 0 && n <= d).then_some((n, d))
    } else {
        match s.trim() {
            "0" => Some((0, 1)),
            "1" => Some((1, 1)),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub functions: usize,
    pub stack_tainted_functions: usize,
    pub words: usize,
    pub branch_sites: usize,
    pub call_sites: usize,
    /// Reads of the CPU's counter register, answered with the virtual clock
    pub counter_sites: usize,
    pub mem_candidates: usize,
    pub mem_sites: usize,
    /// Sites a `b` could not reach from, or a trampoline reach a target
    /// from: left alone (programs of over about 120 MB)
    pub unreachable_sites: usize,
    /// Function-table entries without a symbol, left alone (constant
    /// tables in hand-written assembly)
    pub unnamed_entries: usize,
    pub unnamed_addrs: Vec<u64>,
    /// Rooms the program was linked with, and the trampoline bytes in them
    pub rooms: usize,
    pub room_bytes: u64,
    /// Sites tried, per MB of text from the start of `__text`
    pub sites_per_mb: Vec<u32>,
    pub exclusive_words: usize,
    pub data_in_code_words: usize,
    pub skipped_blr_x30: usize,
    pub stub_bytes: usize,
    /// Addresses of the hooked memory instructions
    pub mem_site_addrs: Vec<u64>,
}

impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "functions={} (stack-tainted {})",
            self.functions, self.stack_tainted_functions
        )?;
        writeln!(f, "words={}", self.words)?;
        writeln!(
            f,
            "branch_sites={} call_sites={}",
            self.branch_sites, self.call_sites
        )?;
        writeln!(
            f,
            "mem_candidates={} mem_sites={}",
            self.mem_candidates, self.mem_sites
        )?;
        writeln!(
            f,
            "skipped: exclusive_words={} data_in_code_words={} blr_x30={} unreachable={} unnamed_entries={}",
            self.exclusive_words,
            self.data_in_code_words,
            self.skipped_blr_x30,
            self.unreachable_sites,
            self.unnamed_entries
        )?;
        writeln!(f, "stub_bytes={}", self.stub_bytes)?;
        writeln!(f, "rooms={} room_bytes={}", self.rooms, self.room_bytes)?;
        write!(f, "mem_site_addrs=")?;
        for (i, a) in self.mem_site_addrs.iter().enumerate() {
            write!(f, "{}{a:#x}", if i == 0 { "" } else { "," })?;
        }
        Ok(())
    }
}

pub struct Rewritten {
    /// Unsigned image; pass through `macho::adhoc_sign` after writing
    pub image: Vec<u8>,
    pub stats: Stats,
    pub sites: Vec<Site>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteKind {
    Branch,
    Call,
    Load,
    Store,
    /// A read of the CPU's counter, answered with the virtual clock
    Counter,
}

/// One hooked instruction
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Site {
    /// Where the shared body returns to in this site's trampoline: what
    /// the supervisor sees when a quantum runs out here, and what a
    /// schedule trace calls `site`
    pub yield_pc: u64,
    /// The original instruction
    pub addr: u64,
    pub kind: SiteKind,
}

impl SiteKind {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            SiteKind::Branch => "branch",
            SiteKind::Call => "call",
            SiteKind::Load => "load",
            SiteKind::Store => "store",
            SiteKind::Counter => "counter",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<SiteKind> {
        [
            SiteKind::Branch,
            SiteKind::Call,
            SiteKind::Load,
            SiteKind::Store,
            SiteKind::Counter,
        ]
        .into_iter()
        .find(|k| k.name() == name)
    }
}

/// The site table as text, one `<yield pc> <address> <kind>` per line.
#[must_use]
pub fn sites_to_text(sites: &[Site]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for s in sites {
        let _ = writeln!(out, "{:x} {:x} {}", s.yield_pc, s.addr, s.kind.name());
    }
    out
}

#[must_use]
pub fn sites_from_text(text: &str) -> Vec<Site> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split(' ');
            Some(Site {
                yield_pc: u64::from_str_radix(w.next()?, 16).ok()?,
                addr: u64::from_str_radix(w.next()?, 16).ok()?,
                kind: SiteKind::from_name(w.next()?)?,
            })
        })
        .collect()
}

#[derive(Debug)]
pub enum Error {
    MachO(macho::Error),
    OutOfRange { site: u64, target: u64 },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::MachO(e) => write!(f, "{e}"),
            Error::OutOfRange { site, target } => {
                write!(f, "branch from {site:#x} to {target:#x} is out of range")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<macho::Error> for Error {
    fn from(e: macho::Error) -> Self {
        Error::MachO(e)
    }
}

/// What the stub does after the counter check
#[derive(Clone, Copy)]
enum Tail {
    /// `island`: the site is a call, so x16 may be used to reach a far
    /// target, as a linker's branch island would
    Jump {
        target: u64,
        island: bool,
    },
    Indirect(u8),
    /// Re-execute the displaced word, then continue after the site
    Replay(u32),
}

/// A local guard placed at the top of a stub for conditional sites; it
/// branches past the stub when the original condition does not hold
#[derive(Clone, Copy)]
enum Guard {
    None,
    Cond(u8),
    Cbz { sf: bool, rt: u8, nonzero: bool },
    Tbz { rt: u8, bit: u8, nonzero: bool },
}

/// A stretch of address space trampolines are written to: the `__STUB`
/// segment, or a room the program was linked with (see `rooms`)
struct Area {
    base: u64,
    /// Bytes it may hold (None: as many as it takes)
    capacity: Option<u64>,
    /// Address of this area's copy of the shared body
    common: u64,
    code: Vec<u32>,
}

struct Builder {
    /// The segment first, then the rooms in address order
    areas: Vec<Area>,
    /// The area being written
    at: usize,
    text_base: u64,
    /// Sites left alone because no area was in a `b`'s reach
    unreachable: usize,
    /// Sites tried, per MB of text: what a room plan needs
    site_buckets: Vec<u32>,
    patches: Vec<(u64, u32)>,
    sites: Vec<Site>,
}

impl Builder {
    fn new(segment: u64, rooms: &[(u64, u64)], text_base: u64) -> Self {
        let mut areas = vec![Area {
            base: segment,
            capacity: None,
            common: 0,
            code: Vec::new(),
        }];
        areas.extend(rooms.iter().map(|&(start, end)| Area {
            base: start,
            capacity: Some(end - start),
            common: 0,
            code: Vec::new(),
        }));
        let mut b = Builder {
            areas,
            at: 0,
            text_base,
            unreachable: 0,
            site_buckets: Vec::new(),
            patches: Vec::new(),
            sites: Vec::new(),
        };
        for i in 0..b.areas.len() {
            b.at = i;
            b.emit_common();
        }
        b.at = 0;
        b
    }

    fn pc(&self) -> u64 {
        self.areas[self.at].base + (self.areas[self.at].code.len() * 4) as u64
    }

    /// Address of word `i` of the area being written
    fn word_addr(&self, i: usize) -> u64 {
        self.areas[self.at].base + (i * 4) as u64
    }

    fn emit(&mut self, w: u32) {
        self.areas[self.at].code.push(w);
    }

    fn len(&self) -> usize {
        self.areas[self.at].code.len()
    }

    fn set(&mut self, i: usize, w: u32) {
        self.areas[self.at].code[i] = w;
    }

    /// The body every trampoline calls with x0, x1 and x30 live: count the
    /// event and, when the quantum is out, enter the scheduler. Its own
    /// x30 is the trampoline's return address, which names the site; the
    /// scheduler's entry reads it from the stack, where it is saved before
    /// the call. One per area: a `bl` has to reach it.
    fn emit_common(&mut self) {
        self.areas[self.at].common = self.pc();
        self.emit(stub::STP_X0_X1_PRE);
        self.emit(stub::movz_x(0, STUB_BASE as u64).expect("STUB_BASE fits one movz"));
        self.emit(stub::ldr_x_imm(1, 0, COUNTER_OFFSET));
        self.emit(stub::SUB_X1_X1_1);
        self.emit(stub::str_x_imm(1, 0, COUNTER_OFFSET));
        let cbz_at = self.len();
        self.emit(0);
        let resume = self.pc();
        self.emit(stub::LDP_X0_X1_POST);
        self.emit(stub::RET);
        // Expired path
        let expired = self.pc();
        let w = stub::cbz(true, 1, false, self.word_addr(cbz_at), expired).unwrap();
        self.set(cbz_at, w);
        // x0 still holds STUB_BASE here
        self.emit(stub::STR_X30_PRE);
        self.emit(stub::ldr_x_imm(0, 0, SLOT_OFFSET));
        let skip_at = self.len();
        self.emit(0);
        self.emit(stub::BLR_X0);
        let skip = self.pc();
        let w = stub::cbz(true, 0, false, self.word_addr(skip_at), skip).unwrap();
        self.set(skip_at, w);
        self.emit(stub::LDR_X30_POST);
        let pc = self.pc();
        self.emit(stub::b(pc, resume).unwrap());
    }

    /// Emit one trampoline for `site` and record the site patch, in the
    /// first area a `b` reaches from the site and from which the tail
    /// reaches its target: the segment, else a room. `call` means the
    /// site keeps a `bl` so x30 is set by hardware. Returns false, having
    /// emitted nothing, when no area will do: a program of over about
    /// 120 MB has such sites unless it was linked with rooms, and they
    /// stay as they are.
    fn stub(&mut self, site: u64, guard: Guard, call: bool, tail: Tail) -> Result<bool, Error> {
        let bucket = (site.saturating_sub(self.text_base) >> 20) as usize;
        if self.site_buckets.len() <= bucket {
            self.site_buckets.resize(bucket + 1, 0);
        }
        self.site_buckets[bucket] += 1;
        for area in 0..self.areas.len() {
            self.at = area;
            let mark = (self.len(), self.patches.len(), self.sites.len());
            let fits = match self.stub_or_far(site, guard, call, tail) {
                Ok(()) => self.areas[area]
                    .capacity
                    .is_none_or(|cap| (self.len() * 4) as u64 <= cap),
                Err(Error::OutOfRange { .. }) => false,
                Err(e) => return Err(e),
            };
            if fits {
                self.at = 0;
                return Ok(true);
            }
            self.areas[area].code.truncate(mark.0);
            self.patches.truncate(mark.1);
            self.sites.truncate(mark.2);
        }
        self.at = 0;
        self.unreachable += 1;
        Ok(false)
    }

    /// A counter read: the site becomes a `b` to a trampoline that calls the
    /// supervisor's entry (`COUNTER_ENTRY_OFFSET`) with the register number
    /// in the word after the call, for the entry to write the count into.
    /// The entry pops x0 and x1 and returns past that word.
    fn counter_stub(&mut self, site: u64, rt: u8) -> Result<bool, Error> {
        for area in 0..self.areas.len() {
            self.at = area;
            let mark = (self.len(), self.patches.len(), self.sites.len());
            let start = self.pc();
            let fits = match stub::b(site, start).ok_or(Error::OutOfRange {
                site,
                target: start,
            }) {
                Ok(site_word) => {
                    self.patches.push((site, site_word));
                    self.emit(stub::STR_X30_PRE);
                    self.emit(stub::STP_X0_X1_PRE);
                    self.emit(stub::movz_x(0, STUB_BASE as u64).expect("STUB_BASE fits one movz"));
                    self.emit(stub::ldr_x_imm(1, 0, COUNTER_ENTRY_OFFSET));
                    self.emit(stub::BLR_X1);
                    let yield_pc = self.pc();
                    self.emit(u32::from(rt));
                    self.emit(stub::LDR_X30_POST);
                    let pc = self.pc();
                    match stub::b(pc, site + 4) {
                        Some(w) => {
                            self.emit(w);
                            self.sites.push(Site {
                                yield_pc,
                                addr: site,
                                kind: SiteKind::Counter,
                            });
                            self.areas[area]
                                .capacity
                                .is_none_or(|cap| (self.len() * 4) as u64 <= cap)
                        }
                        None => false,
                    }
                }
                Err(_) => false,
            };
            if fits {
                self.at = 0;
                return Ok(true);
            }
            self.areas[area].code.truncate(mark.0);
            self.patches.truncate(mark.1);
            self.sites.truncate(mark.2);
        }
        self.unreachable += 1;
        self.at = 0;
        Ok(false)
    }

    fn stub_or_far(
        &mut self,
        site: u64,
        guard: Guard,
        call: bool,
        tail: Tail,
    ) -> Result<(), Error> {
        let start = self.pc();
        let site_word = if call {
            stub::bl(site, start)
        } else {
            stub::b(site, start)
        }
        .ok_or(Error::OutOfRange {
            site,
            target: start,
        })?;
        self.patches.push((site, site_word));

        // Guard: index of the word to patch with the fall-through branch
        let guard_at = if matches!(guard, Guard::None) {
            None
        } else {
            self.emit(0);
            Some(self.len() - 1)
        };
        self.emit(stub::STR_X30_PRE);
        let pc = self.pc();
        let to_common = stub::bl(pc, self.areas[self.at].common).ok_or(Error::OutOfRange {
            site,
            target: self.areas[self.at].common,
        })?;
        self.emit(to_common);
        let yield_pc = self.pc();
        self.emit(stub::LDR_X30_POST);
        match tail {
            Tail::Jump { target, island } => match b_to(self.pc(), target) {
                Ok(w) => self.emit(w),
                // A callee may be anywhere in the text; the other hooked
                // branches stay inside their function
                Err(e) if !island => return Err(e),
                Err(_) => {
                    let page = stub::adrp(16, self.pc(), target)
                        .ok_or(Error::OutOfRange { site, target })?;
                    self.emit(page);
                    self.emit(stub::add_imm(16, 16, (target & 0xFFF) as u32));
                    self.emit(stub::br(16));
                }
            },
            Tail::Indirect(rn) => self.emit(stub::br(rn)),
            Tail::Replay(word) => {
                self.emit(word);
                let w = b_to(self.pc(), site + 4)?;
                self.emit(w);
            }
        }
        let kind = match tail {
            Tail::Replay(word) if is_load(word) => SiteKind::Load,
            Tail::Replay(_) => SiteKind::Store,
            _ if call => SiteKind::Call,
            _ => SiteKind::Branch,
        };
        self.sites.push(Site {
            yield_pc,
            addr: site,
            kind,
        });

        if let Some(at) = guard_at {
            let fallthrough = self.pc();
            let pc = self.word_addr(at);
            let w = match guard {
                Guard::None => unreachable!(),
                Guard::Cond(c) => stub::b_cond(decode::invert_cond(c), pc, fallthrough).unwrap(),
                Guard::Cbz { sf, rt, nonzero } => {
                    stub::cbz(sf, rt, !nonzero, pc, fallthrough).unwrap()
                }
                Guard::Tbz { rt, bit, nonzero } => {
                    stub::tbz(rt, bit, !nonzero, pc, fallthrough).unwrap()
                }
            };
            self.set(at, w);
            self.emit(b_to(fallthrough, site + 4)?);
        }
        Ok(())
    }
}

/// Whether a hooked memory instruction reads. In most A64 load/store
/// encodings bit 22 says so; the integer register forms (bits 29:27 = 111,
/// V = 0) have a two-bit opc in 23:22 where 10 and 11 are the sign-extending
/// loads.
fn is_load(word: u32) -> bool {
    let integer_register_form = (word >> 27) & 7 == 7 && word & (1 << 26) == 0;
    if integer_register_form {
        (word >> 22) & 3 != 0
    } else {
        word & (1 << 22) != 0
    }
}

fn b_to(site: u64, target: u64) -> Result<u32, Error> {
    stub::b(site, target).ok_or(Error::OutOfRange { site, target })
}

/// Function ranges from `LC_FUNCTION_STARTS`, clipped to `__text`
/// The functions to hook: `(start, end)` from the function table, and how
/// many entries were passed over. An entry without a symbol is an atom the
/// assembler made without a name: a constant table in hand-written
/// assembly, which the table lists like a function (blst keeps the SHA-256
/// round constants in front of its SHA-256 routine, and one of them
/// decodes as a backward `b`). Compiled functions always have a symbol
/// while the file has any, so such entries are left alone when symbols
/// name at least half of the table; a stripped file hooks everything.
fn function_ranges(m: &MachO) -> Result<Ranges, Error> {
    let text = m.text_section()?;
    let end = text.addr + text.size;
    let starts = m.function_starts()?;
    let symbols = m.symbols();
    let named = |s: u64| symbols.binary_search_by_key(&s, |&(a, _)| a).is_ok();
    let is_room = |s: u64| {
        let first = symbols.partition_point(|&(a, _)| a < s);
        symbols[first..]
            .iter()
            .take_while(|&&(a, _)| a == s)
            .any(|(_, name)| name.starts_with(ROOM_PREFIX))
    };
    let mostly_named = starts.iter().filter(|&&s| named(s)).count() * 2 >= starts.len();
    let mut out = Ranges::default();
    for (i, &s) in starts.iter().enumerate() {
        if s < text.addr || s >= end {
            continue;
        }
        if mostly_named && !named(s) {
            out.unnamed += 1;
            out.unnamed_addrs.push(s);
            continue;
        }
        let e = starts.get(i + 1).copied().unwrap_or(end).min(end);
        if e <= s {
            continue;
        }
        if is_room(s) {
            out.rooms.push((s, e));
        } else {
            out.functions.push((s, e));
        }
    }
    Ok(out)
}

/// The function table sorted out: what to hook, what was passed over, and
/// the rooms the program was linked with (`rooms`), which hold nothing
/// yet and are the rewriter's to fill
#[derive(Default)]
struct Ranges {
    functions: Vec<(u64, u64)>,
    unnamed: usize,
    unnamed_addrs: Vec<u64>,
    rooms: Vec<(u64, u64)>,
}

/// Symbols the rooms are known by: `_rewrite_room_0`, `_rewrite_room_1`, ...
pub const ROOM_PREFIX: &str = "_rewrite_room_";

pub fn rewrite(m: &MachO, opts: &Options) -> Result<Rewritten, Error> {
    let layout = m.plan_layout()?;
    let ranges = function_ranges(m)?;
    let text_base = m.text_section()?.addr;
    let mut b = Builder::new(layout.text_addr + HEADER_SIZE, &ranges.rooms, text_base);
    let mut stats = Stats {
        unnamed_entries: ranges.unnamed,
        unnamed_addrs: ranges.unnamed_addrs,
        rooms: ranges.rooms.len(),
        ..Stats::default()
    };
    let mut rng = Rng::seed_from_u64(opts.seed);
    let (rate_num, rate_den) = opts.mem_rate;
    let dic: Vec<(u64, u64)> = m
        .data_in_code()
        .iter()
        .map(|&(a, len, _)| (a, a + u64::from(len)))
        .collect();

    for (start, end) in ranges.functions {
        stats.functions += 1;
        let n = ((end - start) / 4) as usize;
        let words: Vec<u32> = (0..n)
            .map(|i| m.read_word(start + (i * 4) as u64).unwrap_or(0))
            .collect();
        let classes: Vec<Class> = words
            .iter()
            .enumerate()
            .map(|(i, &w)| decode::decode(w, start + (i * 4) as u64))
            .collect();

        let stack_tainted = classes.iter().any(|c| matches!(c, Class::StackAddr));
        if stack_tainted {
            stats.stack_tainted_functions += 1;
        }
        // Words inside an ldxr…stxr span (inclusive) are never touched.
        let mut exclusive = vec![false; n];
        let mut open = false;
        for (i, c) in classes.iter().enumerate() {
            match c {
                Class::ExclusiveLoad => open = true,
                Class::ExclusiveStore => {
                    exclusive[i] = true;
                    open = false;
                    continue;
                }
                _ => {}
            }
            exclusive[i] = open;
        }

        for i in 0..n {
            let pc = start + (i * 4) as u64;
            stats.words += 1;
            if dic.iter().any(|&(a, e)| pc >= a && pc < e) {
                stats.data_in_code_words += 1;
                continue;
            }
            if exclusive[i] {
                stats.exclusive_words += 1;
                continue;
            }
            let jump = |target: u64, island: bool| Tail::Jump { target, island };
            match classes[i] {
                Class::B { target } if target <= pc => {
                    // Not an island even out of its function: hand-written
                    // assembly (blst, aws-lc) jumps between what the
                    // function table calls functions with x16 live, and a
                    // tail call is not told from that
                    if b.stub(pc, Guard::None, false, jump(target, false))? {
                        stats.branch_sites += 1;
                    }
                }
                Class::Bl { target } => {
                    if b.stub(pc, Guard::None, true, jump(target, true))? {
                        stats.call_sites += 1;
                    }
                }
                Class::Blr { rn } => {
                    if rn == 30 {
                        stats.skipped_blr_x30 += 1;
                    } else if b.stub(pc, Guard::None, true, Tail::Indirect(rn))? {
                        stats.call_sites += 1;
                    }
                }
                Class::Br { rn } => {
                    if b.stub(pc, Guard::None, false, Tail::Indirect(rn))? {
                        stats.branch_sites += 1;
                    }
                }
                Class::BCond { cond, target } if target <= pc => {
                    if b.stub(pc, Guard::Cond(cond), false, jump(target, false))? {
                        stats.branch_sites += 1;
                    }
                }
                Class::Cbz {
                    sf,
                    rt,
                    nonzero,
                    target,
                } if target <= pc => {
                    let guard = Guard::Cbz { sf, rt, nonzero };
                    if b.stub(pc, guard, false, jump(target, false))? {
                        stats.branch_sites += 1;
                    }
                }
                Class::Tbz {
                    rt,
                    bit,
                    nonzero,
                    target,
                } if target <= pc => {
                    let guard = Guard::Tbz { rt, bit, nonzero };
                    if b.stub(pc, guard, false, jump(target, false))? {
                        stats.branch_sites += 1;
                    }
                }
                // x29 and up are never a counter's destination in practice;
                // such a site is left alone rather than given a slot
                Class::Counter { rt } if rt < 29 => {
                    if b.counter_stub(pc, rt)? {
                        stats.counter_sites += 1;
                    }
                }
                Class::Mem { base } => {
                    if (base == SP || base == FP) && !stack_tainted {
                        continue;
                    }
                    stats.mem_candidates += 1;
                    if rate_num == 0 {
                        continue;
                    }
                    let roll = rng.below(u64::from(rate_den));
                    if roll < u64::from(rate_num)
                        && b.stub(pc, Guard::None, false, Tail::Replay(words[i]))?
                    {
                        stats.mem_sites += 1;
                        stats.mem_site_addrs.push(pc);
                    }
                }
                _ => {}
            }
        }
    }

    stats.unreachable_sites = b.unreachable;
    stats.sites_per_mb = b.site_buckets;
    let mut stub_text = vec![0u8; HEADER_SIZE as usize];
    let put = |d: &mut [u8], off: u32, v: u64| {
        d[off as usize..off as usize + 8].copy_from_slice(&v.to_le_bytes());
    };
    put(&mut stub_text, HEADER_MAGIC, MAGIC);
    put(&mut stub_text, HEADER_SITES, b.sites.len() as u64);
    put(&mut stub_text, HEADER_MEM_SITES, stats.mem_sites as u64);
    put(&mut stub_text, HEADER_SEED, opts.seed);
    stub_text.extend(b.areas[0].code.iter().flat_map(|w| w.to_le_bytes()));
    stats.stub_bytes = stub_text.len();
    // A room's trampolines are text patches like the sites' own
    let mut patches = b.patches;
    for room in &b.areas[1..] {
        stats.room_bytes += (room.code.len() * 4) as u64;
        patches.extend(
            room.code
                .iter()
                .enumerate()
                .map(|(i, &w)| (room.base + (i * 4) as u64, w)),
        );
    }
    let image = m.emit(&patches, &stub_text)?;
    Ok(Rewritten {
        image,
        stats,
        sites: b.sites,
    })
}

/// Scan without emitting: the statistics only.
pub fn scan(m: &MachO, opts: &Options) -> Result<Stats, Error> {
    rewrite(m, opts).map(|r| r.stats)
}
