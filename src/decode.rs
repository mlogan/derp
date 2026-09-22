//! Decoder for the handful of `AArch64` instruction classes the rewriter
//! cares about. Everything else is `Other` and left untouched.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// `b target`
    B {
        target: u64,
    },
    /// `bl target`
    Bl {
        target: u64,
    },
    /// `b.cond target`; `cond` is the 4-bit condition code
    BCond {
        cond: u8,
        target: u64,
    },
    /// `cbz`/`cbnz` `rt, target`
    Cbz {
        sf: bool,
        rt: u8,
        nonzero: bool,
        target: u64,
    },
    /// `tbz`/`tbnz` `rt, #bit, target`
    Tbz {
        rt: u8,
        bit: u8,
        nonzero: bool,
        target: u64,
    },
    Br {
        rn: u8,
    },
    Blr {
        rn: u8,
    },
    Ret,
    /// `mrs xN, cntvct_el0` or `cntpct_el0`: the CPU's counter, a clock
    /// no interposer sees
    Counter {
        rt: u8,
    },
    /// A load or store that can be displaced into a stub and re-executed
    Mem {
        base: u8,
    },
    /// `ldxr`-family load; opens an exclusive span
    ExclusiveLoad,
    /// `stxr`-family store; closes an exclusive span
    ExclusiveStore,
    /// `add/sub xN, sp|x29, …` or `mov xN, sp`: the function hands out a
    /// stack address
    StackAddr,
    Svc,
    Other,
}

pub const SP: u8 = 31;
pub const FP: u8 = 29;

fn sext(v: u64, bits: u32) -> i64 {
    let shift = 64 - bits;
    ((v << shift) as i64) >> shift
}

fn rel(pc: u64, imm: u64, bits: u32) -> u64 {
    pc.wrapping_add((sext(imm, bits) * 4) as u64)
}

pub fn decode(w: u32, pc: u64) -> Class {
    if w & 0xFFFF_FFE0 == 0xD53B_E040 || w & 0xFFFF_FFE0 == 0xD53B_E020 {
        return Class::Counter {
            rt: (w & 0x1F) as u8,
        };
    }
    // Unconditional immediate branches
    if w & 0xFC00_0000 == 0x1400_0000 {
        return Class::B {
            target: rel(pc, u64::from(w & 0x03FF_FFFF), 26),
        };
    }
    if w & 0xFC00_0000 == 0x9400_0000 {
        return Class::Bl {
            target: rel(pc, u64::from(w & 0x03FF_FFFF), 26),
        };
    }
    // b.cond and bc.cond
    if w & 0xFF00_0000 == 0x5400_0000 {
        let cond = (w & 0xF) as u8;
        let target = rel(pc, u64::from((w >> 5) & 0x7FFFF), 19);
        return if cond >= 0b1110 {
            Class::B { target }
        } else {
            Class::BCond { cond, target }
        };
    }
    if w & 0x7E00_0000 == 0x3400_0000 {
        return Class::Cbz {
            sf: w >> 31 != 0,
            rt: (w & 0x1F) as u8,
            nonzero: w & 0x0100_0000 != 0,
            target: rel(pc, u64::from((w >> 5) & 0x7FFFF), 19),
        };
    }
    if w & 0x7E00_0000 == 0x3600_0000 {
        return Class::Tbz {
            rt: (w & 0x1F) as u8,
            bit: (((w >> 31) << 5) | ((w >> 19) & 0x1F)) as u8,
            nonzero: w & 0x0100_0000 != 0,
            target: rel(pc, u64::from((w >> 5) & 0x3FFF), 14),
        };
    }
    if w & 0xFFFF_FC1F == 0xD61F_0000 {
        return Class::Br {
            rn: ((w >> 5) & 0x1F) as u8,
        };
    }
    if w & 0xFFFF_FC1F == 0xD63F_0000 {
        return Class::Blr {
            rn: ((w >> 5) & 0x1F) as u8,
        };
    }
    if w & 0xFFFF_FC1F == 0xD65F_0000 {
        return Class::Ret;
    }
    if w & 0xFFE0_001F == 0xD400_0001 {
        return Class::Svc;
    }

    // Loads and stores: op0 = x1x0 at bits 28..25
    if w & 0x0A00_0000 == 0x0800_0000 {
        return decode_mem(w);
    }

    // Stack address materialization
    let rd = (w & 0x1F) as u8;
    let rn = ((w >> 5) & 0x1F) as u8;
    let add_sub_imm64 = w & 0x1F80_0000 == 0x1100_0000 && w >> 31 != 0;
    if add_sub_imm64 && rd != SP && (rn == SP || rn == FP) && !(rn == SP && rd == FP) {
        return Class::StackAddr;
    }
    // add/sub (extended register) with sp as the first operand
    if w & 0x1FE0_03E0 == 0x0B20_03E0 && w >> 31 != 0 && rd != SP {
        return Class::StackAddr;
    }
    Class::Other
}

fn decode_mem(w: u32) -> Class {
    let base = ((w >> 5) & 0x1F) as u8;
    // Load register (literal): pc-relative, not shared memory
    if w & 0x3B00_0000 == 0x1800_0000 {
        return Class::Other;
    }
    // Load/store exclusive, ordered, compare-and-swap: bits 29..24 = 001000
    if w & 0x3F00_0000 == 0x0800_0000 {
        let o2 = w & 0x0080_0000 != 0;
        let l = w & 0x0040_0000 != 0;
        let o1 = w & 0x0020_0000 != 0;
        if !o2 {
            // casp has bit 31 clear and o1 set; everything else is exclusive
            if w >> 31 == 0 && o1 {
                return Class::Mem { base };
            }
            return if l {
                Class::ExclusiveLoad
            } else {
                Class::ExclusiveStore
            };
        }
        // ldar/stlr (o1 clear) and cas (o1 set)
        return Class::Mem { base };
    }
    // Advanced SIMD load/store structures: bits 29..24 = 00110x
    if w & 0xBF00_0000 == 0x0C00_0000 {
        return Class::Mem { base };
    }
    // Load/store register, unsigned immediate
    if w & 0x3B00_0000 == 0x3900_0000 {
        if w & 0xFFC0_0000 == 0xF980_0000 {
            return Class::Other; // prfm
        }
        return Class::Mem { base };
    }
    // Load/store register: unscaled, post, pre, register offset, atomics
    if w & 0x3B00_0000 == 0x3800_0000 {
        let op21 = w & 0x0020_0000 != 0;
        let op10 = (w >> 10) & 3;
        if !op21 {
            if op10 == 0b10 {
                return Class::Other; // unprivileged ldtr/sttr
            }
            if w & 0xFFE0_0C00 == 0xF880_0000 {
                return Class::Other; // prfum
            }
            return Class::Mem { base };
        }
        if op10 == 0b10 {
            if w & 0xFFE0_0C00 == 0xF8A0_0800 {
                return Class::Other; // prfm (register)
            }
            return Class::Mem { base };
        }
        if op10 == 0b00 {
            return Class::Mem { base }; // LSE atomics, ldapr
        }
        return Class::Other; // pointer-auth ldraa/ldrab and reserved
    }
    // Load/store pair (no-allocate, post, offset, pre)
    if w & 0x3A00_0000 == 0x2800_0000 {
        return Class::Mem { base };
    }
    Class::Other
}

/// Condition code with the sense flipped
pub fn invert_cond(cond: u8) -> u8 {
    cond ^ 1
}

#[cfg(test)]
mod tests {
    use super::*;

    const PC: u64 = 0x1000;

    #[test]
    fn branches() {
        assert_eq!(decode(0x1400_0000, PC), Class::B { target: PC });
        assert_eq!(decode(0x97FF_FFFF, PC), Class::Bl { target: PC - 4 });
        assert_eq!(
            decode(0x54FF_FFC0, PC),
            Class::BCond {
                cond: 0,
                target: PC - 8
            }
        );
        assert_eq!(
            decode(0x54FF_FFA1, PC),
            Class::BCond {
                cond: 1,
                target: PC - 12
            }
        );
        assert_eq!(
            decode(0xB4FF_FF83, PC),
            Class::Cbz {
                sf: true,
                rt: 3,
                nonzero: false,
                target: PC - 16
            }
        );
        assert_eq!(
            decode(0x35FF_FF65, PC),
            Class::Cbz {
                sf: false,
                rt: 5,
                nonzero: true,
                target: PC - 20
            }
        );
        assert_eq!(
            decode(0xB647_FF47, PC),
            Class::Tbz {
                rt: 7,
                bit: 40,
                nonzero: false,
                target: PC - 24
            }
        );
        assert_eq!(
            decode(0x371F_FF22, PC),
            Class::Tbz {
                rt: 2,
                bit: 3,
                nonzero: true,
                target: PC - 28
            }
        );
        assert_eq!(decode(0xD61F_0120, PC), Class::Br { rn: 9 });
        assert_eq!(decode(0xD63F_0200, PC), Class::Blr { rn: 16 });
        assert_eq!(decode(0xD65F_03C0, PC), Class::Ret);
        assert_eq!(decode(0xD400_1001, PC), Class::Svc);
        assert_eq!(decode(0xD503_201F, PC), Class::Other); // nop
    }

    #[test]
    fn memory() {
        let mem = |base| Class::Mem { base };
        assert_eq!(decode(0xF940_0462, PC), mem(3)); // ldr x2, [x3, #8]
        assert_eq!(decode(0xB940_13E2, PC), mem(31)); // ldr w2, [sp, #16]
        assert_eq!(decode(0xF85F_83A2, PC), mem(29)); // ldur x2, [x29, #-8]
        assert_eq!(decode(0xF840_8462, PC), mem(3)); // ldr x2, [x3], #8
        assert_eq!(decode(0xF800_8C62, PC), mem(3)); // str x2, [x3, #8]!
        assert_eq!(decode(0xF864_6862, PC), mem(3)); // ldr x2, [x3, x4]
        assert_eq!(decode(0xA941_0C82, PC), mem(4)); // ldp x2, x3, [x4, #16]
        assert_eq!(decode(0xADBF_07E0, PC), mem(31)); // stp q0, q1, [sp, #-32]!
        assert_eq!(decode(0x3DC0_0020, PC), mem(1)); // ldr q0, [x1]
        assert_eq!(decode(0xC8DF_FC20, PC), mem(1)); // ldar
        assert_eq!(decode(0xC89F_FC20, PC), mem(1)); // stlr
        assert_eq!(decode(0xF820_0041, PC), mem(2)); // ldadd
        assert_eq!(decode(0xB8E0_0041, PC), mem(2)); // ldaddal
        assert_eq!(decode(0xC8A0_7C41, PC), mem(2)); // cas
        assert_eq!(decode(0x88E0_FC41, PC), mem(2)); // casal
        assert_eq!(decode(0xF820_8041, PC), mem(2)); // swp
        assert_eq!(decode(0x4820_7C82, PC), mem(4)); // casp
        assert_eq!(decode(0x4C40_7820, PC), mem(1)); // ld1.4s
        assert_eq!(decode(0x4C9F_7820, PC), mem(1)); // st1.4s post
        assert_eq!(decode(0x3940_0C20, PC), mem(1)); // ldrb
        assert_eq!(decode(0xB980_0420, PC), mem(1)); // ldrsw
        assert_eq!(decode(0x7900_0420, PC), mem(1)); // strh
        assert_eq!(decode(0xF8BF_C020, PC), mem(1)); // ldapr
        assert_eq!(decode(0x3980_1020, PC), mem(1)); // ldrsb

        assert_eq!(decode(0xC85F_7C20, PC), Class::ExclusiveLoad); // ldxr
        assert_eq!(decode(0xC802_7C20, PC), Class::ExclusiveStore); // stxr
        assert_eq!(decode(0x885F_FC20, PC), Class::ExclusiveLoad); // ldaxr
        assert_eq!(decode(0x8802_FC20, PC), Class::ExclusiveStore); // stlxr
        assert_eq!(decode(0xC87F_0440, PC), Class::ExclusiveLoad); // ldxp
        assert_eq!(decode(0xC824_0440, PC), Class::ExclusiveStore); // stxp

        assert_eq!(decode(0x5800_02C0, PC), Class::Other); // ldr x0, literal
        assert_eq!(decode(0xF980_0020, PC), Class::Other); // prfm
        assert_eq!(decode(0xF840_0820, PC), Class::Other); // ldtr
    }

    #[test]
    fn stack_addresses() {
        assert_eq!(decode(0x9100_43E0, PC), Class::StackAddr); // add x0, sp, #16
        assert_eq!(decode(0xD100_43E0, PC), Class::StackAddr); // sub x0, sp, #16
        assert_eq!(decode(0x9100_03E0, PC), Class::StackAddr); // mov x0, sp
        assert_eq!(decode(0x8B21_63E0, PC), Class::StackAddr); // add x0, sp, x1
        assert_eq!(decode(0x9100_43A0, PC), Class::StackAddr); // add x0, x29, #16
        assert_eq!(decode(0xD100_43A0, PC), Class::StackAddr); // sub x0, x29, #16
        assert_eq!(decode(0x9100_43FD, PC), Class::Other); // add x29, sp, #16
        assert_eq!(decode(0x9100_43FF, PC), Class::Other); // add sp, sp, #16
        assert_eq!(decode(0x9100_4020, PC), Class::Other); // add x0, x1, #16
        assert_eq!(decode(0xD53B_D060, PC), Class::Other); // mrs
    }
}
