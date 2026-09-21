//! `AArch64` encoders for the instructions the stubs are built from.

pub const STP_X0_X1_PRE: u32 = 0xA9BF_07E0; // stp x0, x1, [sp, #-16]!
pub const LDP_X0_X1_POST: u32 = 0xA8C1_07E0; // ldp x0, x1, [sp], #16
pub const STR_X30_PRE: u32 = 0xF81F_0FFE; // str x30, [sp, #-16]!
pub const LDR_X30_POST: u32 = 0xF841_07FE; // ldr x30, [sp], #16
pub const SUB_X1_X1_1: u32 = 0xD100_0421;
pub const BLR_X0: u32 = 0xD63F_0000;
pub const RET: u32 = 0xD65F_03C0;

pub const B_RANGE: i64 = 128 << 20;

fn fits(off: i64, bits: u32) -> bool {
    let lim = 1i64 << (bits + 1); // offsets are in words, encoded as imm*4
    off.trailing_zeros() >= 2 && off >= -lim && off < lim
}

/// `b` from `pc` to `target`
pub fn b(pc: u64, target: u64) -> Option<u32> {
    let off = target.wrapping_sub(pc) as i64;
    fits(off, 26).then_some(0x1400_0000 | (((off / 4) as u32) & 0x03FF_FFFF))
}

pub fn bl(pc: u64, target: u64) -> Option<u32> {
    b(pc, target).map(|w| w | 0x8000_0000)
}

pub fn b_cond(cond: u8, pc: u64, target: u64) -> Option<u32> {
    let off = target.wrapping_sub(pc) as i64;
    fits(off, 19).then(|| 0x5400_0000 | ((((off / 4) as u32) & 0x7FFFF) << 5) | u32::from(cond))
}

pub fn cbz(sf: bool, rt: u8, nonzero: bool, pc: u64, target: u64) -> Option<u32> {
    let off = target.wrapping_sub(pc) as i64;
    fits(off, 19).then(|| {
        0x3400_0000
            | (u32::from(sf) << 31)
            | (u32::from(nonzero) << 24)
            | ((((off / 4) as u32) & 0x7FFFF) << 5)
            | u32::from(rt)
    })
}

pub fn tbz(rt: u8, bit: u8, nonzero: bool, pc: u64, target: u64) -> Option<u32> {
    let off = target.wrapping_sub(pc) as i64;
    fits(off, 14).then(|| {
        0x3600_0000
            | (u32::from(bit >> 5) << 31)
            | (u32::from(nonzero) << 24)
            | (u32::from(bit & 31) << 19)
            | ((((off / 4) as u32) & 0x3FFF) << 5)
            | u32::from(rt)
    })
}

pub fn br(rn: u8) -> u32 {
    0xD61F_0000 | (u32::from(rn) << 5)
}

/// `adrp xd, target` from `pc`: the page of `target`, within 4 GB
pub fn adrp(rd: u8, pc: u64, target: u64) -> Option<u32> {
    let pages = ((target >> 12) as i64).wrapping_sub((pc >> 12) as i64);
    if !(-(1 << 20)..(1 << 20)).contains(&pages) {
        return None;
    }
    let imm = pages as u32;
    Some(0x9000_0000 | ((imm & 3) << 29) | (((imm >> 2) & 0x7_FFFF) << 5) | u32::from(rd))
}

/// `add xd, xn, #imm12`
pub fn add_imm(rd: u8, rn: u8, imm: u32) -> u32 {
    debug_assert!(imm < 0x1000);
    0x9100_0000 | (imm << 10) | (u32::from(rn) << 5) | u32::from(rd)
}

/// `movz xd, #imm16, lsl #shift` for a value with one non-zero 16-bit chunk
pub fn movz_x(rd: u8, value: u64) -> Option<u32> {
    let shift = if value == 0 {
        0
    } else {
        value.trailing_zeros() / 16
    };
    let imm = value >> (shift * 16);
    (imm <= 0xFFFF).then(|| 0xD280_0000 | (shift << 21) | ((imm as u32) << 5) | u32::from(rd))
}

/// `ldr xt, [xn, #imm]`, `imm` a multiple of 8 below 32 KB
pub fn ldr_x_imm(rt: u8, rn: u8, imm: u32) -> u32 {
    debug_assert!(imm.is_multiple_of(8) && imm < 0x8000);
    0xF940_0000 | ((imm / 8) << 10) | (u32::from(rn) << 5) | u32::from(rt)
}

pub fn str_x_imm(rt: u8, rn: u8, imm: u32) -> u32 {
    debug_assert!(imm.is_multiple_of(8) && imm < 0x8000);
    0xF900_0000 | ((imm / 8) << 10) | (u32::from(rn) << 5) | u32::from(rt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_assembler() {
        assert_eq!(b(0x1000, 0x1000), Some(0x1400_0000));
        assert_eq!(bl(0x1004, 0x1000), Some(0x97FF_FFFF));
        assert_eq!(b_cond(0, 0x1008, 0x1000), Some(0x54FF_FFC0));
        assert_eq!(b_cond(1, 0x100C, 0x1000), Some(0x54FF_FFA1));
        assert_eq!(cbz(true, 3, false, 0x1010, 0x1000), Some(0xB4FF_FF83));
        assert_eq!(cbz(false, 5, true, 0x1014, 0x1000), Some(0x35FF_FF65));
        assert_eq!(tbz(7, 40, false, 0x1018, 0x1000), Some(0xB647_FF47));
        assert_eq!(tbz(2, 3, true, 0x101C, 0x1000), Some(0x371F_FF22));
        assert_eq!(br(9), 0xD61F_0120);
        assert_eq!(RET, 0xD65F_03C0);
        assert_eq!(adrp(16, 0x1_0000, 0x1_1000), Some(0xB000_0010));
        assert_eq!(adrp(16, 0x1_0000, 0x1_2000), Some(0xD000_0010));
        assert_eq!(adrp(16, 0x1_2000, 0x1_0fff), Some(0xD0FF_FFF0));
        assert_eq!(adrp(0, 0x1000, 0x1000), Some(0x9000_0000));
        assert!(adrp(0, 0, 1 << 32).is_none());
        assert_eq!(add_imm(16, 16, 0x123), 0x9104_8E10);
        assert_eq!(movz_x(0, 0x78_0000_0000), Some(0xD2C0_0F00));
        assert_eq!(movz_x(3, 0x1234), Some(0xD282_4683));
        assert_eq!(movz_x(0, 0), Some(0xD280_0000));
        assert_eq!(movz_x(0, 0x1_0001), None);
        assert_eq!(ldr_x_imm(1, 0, 0), 0xF940_0001);
        assert_eq!(ldr_x_imm(1, 0, 8), 0xF940_0401);
        assert_eq!(str_x_imm(1, 0, 0), 0xF900_0001);
    }

    #[test]
    fn range_limits() {
        assert!(b(0, B_RANGE as u64).is_none());
        assert!(b(0, (B_RANGE - 4) as u64).is_some());
        assert!(b_cond(0, 0, 1 << 20).is_none());
        assert!(tbz(0, 0, false, 0, 1 << 15).is_none());
        assert!(tbz(0, 0, false, 1 << 15, 0).is_some());
    }
}
