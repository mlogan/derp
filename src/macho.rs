//! Minimal Mach-O reader and writer for aarch64 executables.
//!
//! Reads the header, load commands, segments and sections; provides the
//! function table from `LC_FUNCTION_STARTS`; and emits a copy of the image
//! with patched text words and two new segments (`__STUBD`, `__STUB`)
//! inserted in front of `__LINKEDIT`. The old code signature is dropped so
//! `codesign -s -` can replace it.

use std::fmt;

pub const MH_MAGIC_64: u32 = 0xFEED_FACF;
pub const CPU_TYPE_ARM64: u32 = 0x0100_000C;
pub const MH_EXECUTE: u32 = 2;
pub const PAGE: u64 = 0x4000;

pub const LC_SEGMENT_64: u32 = 0x19;
pub const LC_SYMTAB: u32 = 0x2;
pub const LC_DYSYMTAB: u32 = 0xB;
pub const LC_DYLD_INFO: u32 = 0x22;
pub const LC_DYLD_INFO_ONLY: u32 = 0x8000_0022;
pub const LC_CODE_SIGNATURE: u32 = 0x1D;
pub const LC_SEGMENT_SPLIT_INFO: u32 = 0x1E;
pub const LC_FUNCTION_STARTS: u32 = 0x26;
pub const LC_DATA_IN_CODE: u32 = 0x29;
pub const LC_DYLIB_CODE_SIGN_DRS: u32 = 0x2B;
pub const LC_LINKER_OPTIMIZATION_HINT: u32 = 0x2E;
pub const LC_DYLD_EXPORTS_TRIE: u32 = 0x8000_0033;
pub const LC_DYLD_CHAINED_FIXUPS: u32 = 0x8000_0034;
pub const LC_ATOM_INFO: u32 = 0x36;

pub const VM_PROT_READ: u32 = 1;
pub const VM_PROT_WRITE: u32 = 2;
pub const VM_PROT_EXECUTE: u32 = 4;

pub const STUB_DATA_SEGMENT: &str = "__STUBD";
pub const STUB_TEXT_SEGMENT: &str = "__STUB";

const HEADER_SIZE: usize = 32;
const SEGMENT_CMD_SIZE: usize = 72;
const SECTION_SIZE: usize = 80;

#[derive(Debug)]
pub enum Error {
    Truncated(&'static str),
    BadMagic(u32),
    NotArm64Executable,
    MissingSegment(&'static str),
    MissingCommand(&'static str),
    NoHeaderRoom { need: usize, have: usize },
    BadLayout(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated(what) => write!(f, "file truncated while reading {what}"),
            Error::BadMagic(m) => write!(f, "not a 64-bit little-endian Mach-O (magic {m:#x})"),
            Error::NotArm64Executable => write!(f, "not an arm64 MH_EXECUTE image"),
            Error::MissingSegment(s) => write!(f, "no {s} segment"),
            Error::MissingCommand(c) => write!(f, "no {c} load command"),
            Error::NoHeaderRoom { need, have } => write!(
                f,
                "load commands would need {need} bytes but only {have} fit before the first section; relink the guest with -Wl,-headerpad,0x1000"
            ),
            Error::BadLayout(s) => write!(f, "unsupported layout: {s}"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone)]
pub struct Header {
    pub cputype: u32,
    pub cpusubtype: u32,
    pub filetype: u32,
    pub ncmds: u32,
    pub sizeofcmds: u32,
    pub flags: u32,
}

#[derive(Debug, Clone)]
pub struct Command {
    pub cmd: u32,
    /// Raw bytes of the whole command including the 8-byte prefix
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Section {
    pub name: String,
    pub segname: String,
    pub addr: u64,
    pub size: u64,
    pub offset: u32,
    pub align: u32,
    pub flags: u32,
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub cmd_index: usize,
    pub name: String,
    pub vmaddr: u64,
    pub vmsize: u64,
    pub fileoff: u64,
    pub filesize: u64,
    pub maxprot: u32,
    pub initprot: u32,
    pub flags: u32,
    pub sections: Vec<Section>,
}

impl Segment {
    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    pub fn contains_addr(&self, addr: u64) -> bool {
        addr >= self.vmaddr && addr < self.vmaddr + self.vmsize
    }
}

#[derive(Debug, Clone)]
pub struct MachO {
    pub data: Vec<u8>,
    pub header: Header,
    pub commands: Vec<Command>,
    pub segments: Vec<Segment>,
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    b.get(off..off + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
}

fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn name16(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn set_name16(b: &mut [u8], name: &str) {
    b.fill(0);
    b[..name.len()].copy_from_slice(name.as_bytes());
}

pub fn round_up(v: u64, align: u64) -> u64 {
    (v + align - 1) & !(align - 1)
}

fn parse_segment(cmd_index: usize, bytes: &[u8]) -> Result<Segment, Error> {
    if bytes.len() < SEGMENT_CMD_SIZE {
        return Err(Error::Truncated("segment command"));
    }
    let nsects = u32_at(bytes, 64).unwrap() as usize;
    let mut sections = Vec::with_capacity(nsects);
    for i in 0..nsects {
        let off = SEGMENT_CMD_SIZE + i * SECTION_SIZE;
        let s = bytes
            .get(off..off + SECTION_SIZE)
            .ok_or(Error::Truncated("section header"))?;
        sections.push(Section {
            name: name16(&s[0..16]),
            segname: name16(&s[16..32]),
            addr: u64_at(s, 32).unwrap(),
            size: u64_at(s, 40).unwrap(),
            offset: u32_at(s, 48).unwrap(),
            align: u32_at(s, 52).unwrap(),
            flags: u32_at(s, 64).unwrap(),
        });
    }
    Ok(Segment {
        cmd_index,
        name: name16(&bytes[8..24]),
        vmaddr: u64_at(bytes, 24).unwrap(),
        vmsize: u64_at(bytes, 32).unwrap(),
        fileoff: u64_at(bytes, 40).unwrap(),
        filesize: u64_at(bytes, 48).unwrap(),
        maxprot: u32_at(bytes, 56).unwrap(),
        initprot: u32_at(bytes, 60).unwrap(),
        flags: u32_at(bytes, 68).unwrap(),
        sections,
    })
}

/// Load commands whose payload is a single (dataoff, datasize) pair into
/// `__LINKEDIT`.
fn is_linkedit_data(cmd: u32) -> bool {
    matches!(
        cmd,
        LC_CODE_SIGNATURE
            | LC_SEGMENT_SPLIT_INFO
            | LC_FUNCTION_STARTS
            | LC_DATA_IN_CODE
            | LC_DYLIB_CODE_SIGN_DRS
            | LC_LINKER_OPTIMIZATION_HINT
            | LC_DYLD_EXPORTS_TRIE
            | LC_DYLD_CHAINED_FIXUPS
            | LC_ATOM_INFO
    )
}

impl MachO {
    pub fn parse(data: Vec<u8>) -> Result<Self, Error> {
        let magic = u32_at(&data, 0).ok_or(Error::Truncated("header"))?;
        if magic != MH_MAGIC_64 {
            return Err(Error::BadMagic(magic));
        }
        if data.len() < HEADER_SIZE {
            return Err(Error::Truncated("header"));
        }
        let header = Header {
            cputype: u32_at(&data, 4).unwrap(),
            cpusubtype: u32_at(&data, 8).unwrap(),
            filetype: u32_at(&data, 12).unwrap(),
            ncmds: u32_at(&data, 16).unwrap(),
            sizeofcmds: u32_at(&data, 20).unwrap(),
            flags: u32_at(&data, 24).unwrap(),
        };
        if header.cputype != CPU_TYPE_ARM64 || header.filetype != MH_EXECUTE {
            return Err(Error::NotArm64Executable);
        }
        let mut commands = Vec::with_capacity(header.ncmds as usize);
        let mut segments = Vec::new();
        let mut off = HEADER_SIZE;
        for _ in 0..header.ncmds {
            let cmd = u32_at(&data, off).ok_or(Error::Truncated("load command"))?;
            let cmdsize = u32_at(&data, off + 4).ok_or(Error::Truncated("load command"))? as usize;
            let bytes = data
                .get(off..off + cmdsize)
                .ok_or(Error::Truncated("load command"))?
                .to_vec();
            if cmd == LC_SEGMENT_64 {
                segments.push(parse_segment(commands.len(), &bytes)?);
            }
            commands.push(Command { cmd, bytes });
            off += cmdsize;
        }
        Ok(MachO {
            data,
            header,
            commands,
            segments,
        })
    }

    pub fn segment(&self, name: &str) -> Option<&Segment> {
        self.segments.iter().find(|s| s.name == name)
    }

    pub fn command(&self, cmd: u32) -> Option<&Command> {
        self.commands.iter().find(|c| c.cmd == cmd)
    }

    /// The `__TEXT,__text` section, where all program code lives.
    pub fn text_section(&self) -> Result<&Section, Error> {
        self.segment("__TEXT")
            .ok_or(Error::MissingSegment("__TEXT"))?
            .section("__text")
            .ok_or(Error::MissingSegment("__TEXT,__text"))
    }

    pub fn segment_for_addr(&self, addr: u64) -> Option<&Segment> {
        self.segments
            .iter()
            .find(|s| s.filesize > 0 && s.contains_addr(addr))
    }

    /// File offset backing a virtual address, if the address is file-backed.
    pub fn addr_to_offset(&self, addr: u64) -> Option<usize> {
        let seg = self.segment_for_addr(addr)?;
        let rel = addr - seg.vmaddr;
        (rel < seg.filesize).then(|| (seg.fileoff + rel) as usize)
    }

    pub fn read_word(&self, addr: u64) -> Option<u32> {
        u32_at(&self.data, self.addr_to_offset(addr)?)
    }

    fn linkedit_data(&self, cmd: u32) -> Option<(usize, usize)> {
        let c = self.command(cmd)?;
        Some((
            u32_at(&c.bytes, 8)? as usize,
            u32_at(&c.bytes, 12)? as usize,
        ))
    }

    /// Absolute addresses of every function start, ascending.
    pub fn function_starts(&self) -> Result<Vec<u64>, Error> {
        let (off, size) = self
            .linkedit_data(LC_FUNCTION_STARTS)
            .ok_or(Error::MissingCommand("LC_FUNCTION_STARTS"))?;
        let bytes = self
            .data
            .get(off..off + size)
            .ok_or(Error::Truncated("function starts"))?;
        let base = self
            .segment("__TEXT")
            .ok_or(Error::MissingSegment("__TEXT"))?
            .vmaddr;
        let mut out = Vec::new();
        let mut addr = base;
        let mut i = 0;
        while i < bytes.len() {
            let (delta, n) = read_uleb(&bytes[i..]).ok_or(Error::Truncated("function starts"))?;
            i += n;
            if delta == 0 {
                break;
            }
            addr += delta;
            out.push(addr);
        }
        Ok(out)
    }

    /// `(addr, length, kind)` entries from `LC_DATA_IN_CODE`.
    pub fn data_in_code(&self) -> Vec<(u64, u16, u16)> {
        let Some((off, size)) = self.linkedit_data(LC_DATA_IN_CODE) else {
            return Vec::new();
        };
        let base = self.segment("__TEXT").map_or(0, |s| s.vmaddr);
        let mut out = Vec::new();
        let mut i = off;
        while i + 8 <= off + size && i + 8 <= self.data.len() {
            let rel = u32_at(&self.data, i).unwrap();
            let len = u16::from_le_bytes([self.data[i + 4], self.data[i + 5]]);
            let kind = u16::from_le_bytes([self.data[i + 6], self.data[i + 7]]);
            out.push((base + u64::from(rel), len, kind));
            i += 8;
        }
        out
    }

    /// Bytes free between the end of the load commands and the first
    /// file-backed section of `__TEXT`.
    pub fn header_room(&self) -> usize {
        let end = HEADER_SIZE + self.header.sizeofcmds as usize;
        let first = self
            .segments
            .iter()
            .flat_map(|s| s.sections.iter())
            .filter(|s| s.offset != 0)
            .map(|s| s.offset as usize)
            .min()
            .unwrap_or(end);
        first.saturating_sub(end)
    }

    /// Virtual addresses the new segments will occupy: the `__STUBD` data
    /// page and the `__STUB` text right after it. Both take over the range
    /// currently assigned to `__LINKEDIT`, which is pushed up in memory.
    pub fn plan_layout(&self) -> Result<Layout, Error> {
        let linkedit = self
            .segment("__LINKEDIT")
            .ok_or(Error::MissingSegment("__LINKEDIT"))?;
        let text = self
            .segment("__TEXT")
            .ok_or(Error::MissingSegment("__TEXT"))?;
        if linkedit.fileoff % PAGE != 0 || linkedit.vmaddr % PAGE != 0 {
            return Err(Error::BadLayout("__LINKEDIT is not page aligned".into()));
        }
        if self.segments.iter().any(|s| s.fileoff > linkedit.fileoff) {
            return Err(Error::BadLayout(
                "__LINKEDIT is not last in the file".into(),
            ));
        }
        let data_addr = linkedit.vmaddr;
        let text_addr = data_addr + PAGE;
        if text_addr.abs_diff(text.vmaddr) >= 128 << 20 {
            return Err(Error::BadLayout(
                "stub segment is out of b range of __TEXT".into(),
            ));
        }
        Ok(Layout {
            data_addr,
            text_addr,
        })
    }

    /// Emit the rewritten image: `patches` are `(address, word)` pairs
    /// applied to file-backed text, `stub_data` becomes the `__STUBD` page and
    /// `stub_text` the `__STUB` segment. The result is unsigned.
    pub fn emit(
        &self,
        patches: &[(u64, u32)],
        stub_data: &[u8],
        stub_text: &[u8],
    ) -> Result<Vec<u8>, Error> {
        let layout = self.plan_layout()?;
        let linkedit = self.segment("__LINKEDIT").unwrap().clone();
        if stub_data.len() as u64 > PAGE {
            return Err(Error::BadLayout("stub data exceeds one page".into()));
        }
        let data_vmsize = PAGE;
        let text_vmsize = round_up(stub_text.len().max(1) as u64, PAGE);
        let inserted = data_vmsize + text_vmsize;

        // The code signature is the tail of __LINKEDIT; drop it and let
        // codesign append a fresh one.
        let mut new_linkedit_size = linkedit.filesize;
        if let Some((sig_off, sig_size)) = self.linkedit_data(LC_CODE_SIGNATURE) {
            let sig_off = sig_off as u64;
            if sig_off < linkedit.fileoff
                || sig_off + sig_size as u64 > linkedit.fileoff + linkedit.filesize
            {
                return Err(Error::BadLayout(
                    "code signature is not at the end of __LINKEDIT".into(),
                ));
            }
            new_linkedit_size = sig_off - linkedit.fileoff;
        }

        let mut commands: Vec<Command> = Vec::with_capacity(self.commands.len() + 2);
        for (i, c) in self.commands.iter().enumerate() {
            if c.cmd == LC_CODE_SIGNATURE {
                continue;
            }
            if i == linkedit.cmd_index {
                commands.push(segment_command(
                    STUB_DATA_SEGMENT,
                    "__data",
                    layout.data_addr,
                    data_vmsize,
                    linkedit.fileoff,
                    data_vmsize,
                    VM_PROT_READ | VM_PROT_WRITE,
                    0,
                ));
                commands.push(segment_command(
                    STUB_TEXT_SEGMENT,
                    "__text",
                    layout.text_addr,
                    text_vmsize,
                    linkedit.fileoff + data_vmsize,
                    text_vmsize,
                    VM_PROT_READ | VM_PROT_EXECUTE,
                    0x8000_0400, // S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS
                ));
                let mut b = c.bytes.clone();
                put_u64(&mut b, 24, layout.text_addr + text_vmsize);
                put_u64(&mut b, 32, round_up(new_linkedit_size, PAGE));
                put_u64(&mut b, 40, linkedit.fileoff + inserted);
                put_u64(&mut b, 48, new_linkedit_size);
                commands.push(Command {
                    cmd: c.cmd,
                    bytes: b,
                });
                continue;
            }
            let mut b = c.bytes.clone();
            shift_linkedit_offsets(c.cmd, &mut b, inserted);
            commands.push(Command {
                cmd: c.cmd,
                bytes: b,
            });
        }

        let sizeofcmds: usize = commands.iter().map(|c| c.bytes.len()).sum();
        let old_end = HEADER_SIZE + self.header.sizeofcmds as usize;
        let room = self.header_room() + self.header.sizeofcmds as usize;
        if sizeofcmds > room {
            return Err(Error::NoHeaderRoom {
                need: sizeofcmds,
                have: room,
            });
        }

        let mut out = Vec::with_capacity(self.data.len() + inserted as usize);
        out.extend_from_slice(&self.data[..linkedit.fileoff as usize]);
        put_u32(&mut out, 16, commands.len() as u32);
        put_u32(&mut out, 20, sizeofcmds as u32);
        let mut off = HEADER_SIZE;
        for c in &commands {
            out[off..off + c.bytes.len()].copy_from_slice(&c.bytes);
            off += c.bytes.len();
        }
        // Zero the gap left when the command list shrinks or is repacked.
        let new_end = HEADER_SIZE + sizeofcmds;
        out[new_end..old_end.max(new_end)].fill(0);

        for &(addr, word) in patches {
            let off = self.addr_to_offset(addr).ok_or_else(|| {
                Error::BadLayout(format!("patch at {addr:#x} is not file-backed"))
            })?;
            if off + 4 > linkedit.fileoff as usize {
                return Err(Error::BadLayout(format!(
                    "patch at {addr:#x} is inside __LINKEDIT"
                )));
            }
            put_u32(&mut out, off, word);
        }

        out.extend_from_slice(stub_data);
        out.resize(linkedit.fileoff as usize + data_vmsize as usize, 0);
        out.extend_from_slice(stub_text);
        out.resize(linkedit.fileoff as usize + inserted as usize, 0);
        let le_start = linkedit.fileoff as usize;
        out.extend_from_slice(&self.data[le_start..le_start + new_linkedit_size as usize]);
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub data_addr: u64,
    pub text_addr: u64,
}

#[allow(clippy::too_many_arguments)]
fn segment_command(
    segname: &str,
    sectname: &str,
    vmaddr: u64,
    vmsize: u64,
    fileoff: u64,
    filesize: u64,
    prot: u32,
    sect_flags: u32,
) -> Command {
    let mut b = vec![0u8; SEGMENT_CMD_SIZE + SECTION_SIZE];
    let len = b.len() as u32;
    put_u32(&mut b, 0, LC_SEGMENT_64);
    put_u32(&mut b, 4, len);
    set_name16(&mut b[8..24], segname);
    put_u64(&mut b, 24, vmaddr);
    put_u64(&mut b, 32, vmsize);
    put_u64(&mut b, 40, fileoff);
    put_u64(&mut b, 48, filesize);
    put_u32(&mut b, 56, prot);
    put_u32(&mut b, 60, prot);
    put_u32(&mut b, 64, 1);
    put_u32(&mut b, 68, 0);
    let s = &mut b[SEGMENT_CMD_SIZE..];
    set_name16(&mut s[0..16], sectname);
    set_name16(&mut s[16..32], segname);
    put_u64(s, 32, vmaddr);
    put_u64(s, 40, filesize);
    put_u32(s, 48, fileoff as u32);
    put_u32(s, 52, 2);
    put_u32(s, 64, sect_flags);
    Command {
        cmd: LC_SEGMENT_64,
        bytes: b,
    }
}

/// Add `delta` to every file offset that points into `__LINKEDIT`.
fn shift_linkedit_offsets(cmd: u32, b: &mut [u8], delta: u64) {
    let bump = |b: &mut [u8], off: usize| {
        let v = u32_at(b, off).unwrap_or(0);
        if v != 0 {
            put_u32(b, off, v + delta as u32);
        }
    };
    if is_linkedit_data(cmd) {
        bump(b, 8);
    } else if cmd == LC_SYMTAB {
        bump(b, 8);
        bump(b, 16);
    } else if cmd == LC_DYSYMTAB {
        for off in [32, 40, 48, 56, 64, 72] {
            bump(b, off);
        }
    } else if cmd == LC_DYLD_INFO || cmd == LC_DYLD_INFO_ONLY {
        for off in [8, 16, 24, 32, 40] {
            bump(b, off);
        }
    }
}

fn read_uleb(b: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    let mut shift = 0;
    for (i, &byte) in b.iter().enumerate() {
        v |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some((v, i + 1));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
    None
}

/// Ad-hoc sign `path` in place with the system `codesign`.
pub fn adhoc_sign(path: &std::path::Path) -> std::io::Result<()> {
    let out = std::process::Command::new("codesign")
        .arg("-s")
        .arg("-")
        .arg("-f")
        .arg(path)
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "codesign failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )))
    }
}
