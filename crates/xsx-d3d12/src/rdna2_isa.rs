//! AMD RDNA 2 (GFX10.3 / Xbox Series X|S) native GPU ISA instruction decoder & disassembler.
//!
//! Decodes RDNA 2 machine code (extracted from Xbox `FSSM` / `FSPX` / `DXIL` shader blobs
//! or raw ISA binaries) into length-accurate, operand-decoded [`Rdna2Insn`] streams across
//! all major GFX10 encoding families (`SOP1`, `SOP2`, `SOPC`, `SOPK`, `SOPP`, `SMEM`, `VOP1`,
//! `VOP2`, `VOPC`, `VOP3`/`VOP3P`, `VINTRP`, `DS`, `MUBUF`, `MTBUF`, `MIMG` + NSA, `FLAT`,
//! `EXP`), including SDWA modifier dwords and trailing 32-bit inline literals.

use std::fmt::Write as _;

/// RDNA 2 instruction encoding family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rdna2Enc {
    Sop2,
    Sopk,
    Sop1,
    Sopc,
    Sopp,
    Smem,
    Vop2,
    Vop1,
    Vopc,
    Vop3,
    Vinterp,
    Ds,
    Mubuf,
    Mtbuf,
    Mimg,
    Flat,
    Exp,
    Unknown,
}

/// Decoded RDNA 2 source or destination operand.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rdna2Opnd {
    Sgpr(u8),
    Vgpr(u8),
    VccLo,
    VccHi,
    ExecLo,
    ExecHi,
    M0,
    Scc,
    Null,
    ConstInt(i64),
    ConstF32(f32),
    Literal(u32),
    Raw(u16),
}

impl Rdna2Opnd {
    pub fn format(&self) -> String {
        match *self {
            Self::Sgpr(n) => format!("s{n}"),
            Self::Vgpr(n) => format!("v{n}"),
            Self::VccLo => "vcc_lo".into(),
            Self::VccHi => "vcc_hi".into(),
            Self::ExecLo => "exec_lo".into(),
            Self::ExecHi => "exec_hi".into(),
            Self::M0 => "m0".into(),
            Self::Scc => "scc".into(),
            Self::Null => "null".into(),
            Self::ConstInt(v) => format!("{v}"),
            Self::ConstF32(v) => format!("{v:?}"),
            Self::Literal(v) => format!("0x{v:08x}"),
            Self::Raw(c) => format!("opnd<{c}>"),
        }
    }
}

/// Encoding-specific auxiliary fields (offsets, export masks, SDWA modifiers).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rdna2Aux {
    pub imm: i64,
    pub imm2: i64,
    pub mask: u8,
    pub sdwa: Option<u32>,
}

/// One decoded RDNA 2 instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct Rdna2Insn {
    pub offset: usize,
    pub dwords: usize,
    pub enc: Rdna2Enc,
    pub op: u32,
    pub raw: Vec<u32>,
    pub mnemonic: String,
    pub dst: Vec<Rdna2Opnd>,
    pub src: Vec<Rdna2Opnd>,
    pub aux: Rdna2Aux,
}

impl Rdna2Insn {
    pub fn is_endpgm(&self) -> bool {
        self.enc == Rdna2Enc::Sopp && self.op == 0x01
    }

    pub fn format_line(&self) -> String {
        let mut s = format!("{:04x}: {:<22}", self.offset, self.mnemonic);
        let mut first = true;
        for d in &self.dst {
            if !first {
                s.push_str(", ");
            }
            s.push_str(&d.format());
            first = false;
        }
        for o in &self.src {
            if !first {
                s.push_str(", ");
            }
            s.push_str(&o.format());
            first = false;
        }
        match self.enc {
            Rdna2Enc::Sopp if matches!(self.op, 0x02 | 0x04..=0x09 | 0x17..=0x1A) => {
                let target = (self.offset as isize) + 4 + (self.aux.imm as isize) * 4;
                let _ = write!(s, " -> 0x{:04x}", target.max(0));
            }
            Rdna2Enc::Sopp if self.op == 0x0C => {
                let vm = self.aux.imm & 0xF;
                let exp = (self.aux.imm >> 4) & 0x7;
                let lgkm = (self.aux.imm >> 8) & 0x3F;
                let _ = write!(s, " vmcnt({vm}) expcnt({exp}) lgkmcnt({lgkm})");
            }
            Rdna2Enc::Sopk => {
                let _ = write!(s, ", {}", self.aux.imm);
            }
            Rdna2Enc::Exp => {
                let target = self.aux.imm;
                let target_name = match target {
                    0..=7 => format!("mrt{target}"),
                    8 => "mrtz".into(),
                    9 => "null".into(),
                    12..=16 => format!("pos{}", target - 12),
                    32..=63 => format!("param{}", target - 32),
                    _ => format!("tgt{target}"),
                };
                let done = if (self.aux.imm2 & 2) != 0 { " done" } else { "" };
                let compr = if (self.aux.imm2 & 1) != 0 { " compr" } else { "" };
                let _ = write!(s, " {target_name} mask=0x{:x}{done}{compr}", self.aux.mask);
            }
            Rdna2Enc::Smem | Rdna2Enc::Mubuf | Rdna2Enc::Mtbuf | Rdna2Enc::Flat if self.aux.imm != 0 => {
                let _ = write!(s, " offset:{}", self.aux.imm);
            }
            Rdna2Enc::Vinterp => {
                let ch = ["x", "y", "z", "w"][(self.aux.imm2 as usize) & 3];
                let _ = write!(s, " attr{}.{ch}", self.aux.imm);
            }
            _ => {}
        }
        s
    }
}

const SRC_SDWA: u32 = 249;

fn classify(w: u32) -> Rdna2Enc {
    if w >> 23 == 0b1_0111_1111 {
        return Rdna2Enc::Sopp;
    }
    if w >> 23 == 0b1_0111_1101 {
        return Rdna2Enc::Sop1;
    }
    if w >> 23 == 0b1_0111_1110 {
        return Rdna2Enc::Sopc;
    }
    if w >> 28 == 0b1011 {
        return Rdna2Enc::Sopk;
    }
    if w >> 30 == 0b10 {
        return Rdna2Enc::Sop2;
    }
    if w >> 25 == 0b011_1111 {
        return Rdna2Enc::Vop1;
    }
    if w >> 25 == 0b011_1110 {
        return Rdna2Enc::Vopc;
    }
    if w >> 31 == 0 && ((w >> 25) & 0x3F) < 0b11_1110 {
        return Rdna2Enc::Vop2;
    }
    match w >> 26 {
        0b110101 | 0b110011 => Rdna2Enc::Vop3,
        0b110010 => Rdna2Enc::Vinterp,
        0b110110 => Rdna2Enc::Ds,
        0b111000 => Rdna2Enc::Mubuf,
        0b111010 => Rdna2Enc::Mtbuf,
        0b111100 => Rdna2Enc::Mimg,
        0b111101 => Rdna2Enc::Smem,
        0b110111 => Rdna2Enc::Flat,
        0b111110 => Rdna2Enc::Exp,
        _ => Rdna2Enc::Unknown,
    }
}

fn instr_len(enc: Rdna2Enc, w: u32, next: Option<u32>) -> (usize, bool) {
    let is_lit = |v: u32| v == 255;
    match enc {
        Rdna2Enc::Sop2 | Rdna2Enc::Sopc => (1, is_lit(w & 0xFF) || is_lit((w >> 8) & 0xFF)),
        Rdna2Enc::Sop1 => (1, is_lit(w & 0xFF)),
        Rdna2Enc::Sopk => (1, ((w >> 23) & 0x1F) == 0x15),
        Rdna2Enc::Sopp | Rdna2Enc::Vinterp | Rdna2Enc::Unknown => (1, false),
        Rdna2Enc::Smem | Rdna2Enc::Ds | Rdna2Enc::Mubuf | Rdna2Enc::Mtbuf | Rdna2Enc::Flat | Rdna2Enc::Exp => {
            (2, false)
        }
        Rdna2Enc::Vop2 => {
            let op = (w >> 25) & 0x3F;
            let src0 = w & 0x1FF;
            let extra = src0 == 255
                || src0 == SRC_SDWA
                || matches!(src0, 233 | 234 | 250)
                || matches!(op, 0x20 | 0x21 | 0x2C | 0x2D | 0x37 | 0x38);
            (1, extra)
        }
        Rdna2Enc::Vop1 | Rdna2Enc::Vopc => {
            let src0 = w & 0x1FF;
            (1, src0 == 255 || src0 == SRC_SDWA || matches!(src0, 233 | 234 | 250))
        }
        Rdna2Enc::Vop3 => {
            let d1 = next.unwrap_or(0);
            let any_lit = (d1 & 0x1FF) == 255
                || ((d1 >> 9) & 0x1FF) == 255
                || ((d1 >> 18) & 0x1FF) == 255;
            (2, any_lit)
        }
        Rdna2Enc::Mimg => (2 + ((w >> 1) & 3) as usize, false),
    }
}

fn opcode_field(enc: Rdna2Enc, w: u32, next: Option<u32>) -> u32 {
    match enc {
        Rdna2Enc::Sop2 => (w >> 23) & 0x7F,
        Rdna2Enc::Sopk => (w >> 23) & 0x1F,
        Rdna2Enc::Sop1 => (w >> 8) & 0xFF,
        Rdna2Enc::Sopc | Rdna2Enc::Sopp => (w >> 16) & 0x7F,
        Rdna2Enc::Smem | Rdna2Enc::Mubuf => (w >> 18) & 0xFF,
        Rdna2Enc::Vop2 => (w >> 25) & 0x3F,
        Rdna2Enc::Vop1 => (w >> 9) & 0xFF,
        Rdna2Enc::Vopc => (w >> 17) & 0xFF,
        Rdna2Enc::Vop3 if (w >> 26) == 0b110011 => 0x400 | ((w >> 16) & 0x7F),
        Rdna2Enc::Vop3 => (w >> 16) & 0x3FF,
        Rdna2Enc::Mimg => ((w >> 18) & 0x7F) | ((w & 1) << 7),
        Rdna2Enc::Mtbuf => ((w >> 16) & 7) | (((next.unwrap_or(0) >> 21) & 1) << 3),
        Rdna2Enc::Vinterp => (w >> 16) & 0x3,
        Rdna2Enc::Exp => 0,
        _ => (w >> 18) & 0xFF,
    }
}

fn ssrc8(code: u16, lit: Option<u32>) -> Rdna2Opnd {
    match code {
        0..=105 => Rdna2Opnd::Sgpr(code as u8),
        106 => Rdna2Opnd::VccLo,
        107 => Rdna2Opnd::VccHi,
        124 => Rdna2Opnd::M0,
        125 => Rdna2Opnd::Null,
        126 => Rdna2Opnd::ExecLo,
        127 => Rdna2Opnd::ExecHi,
        128 => Rdna2Opnd::ConstInt(0),
        129..=192 => Rdna2Opnd::ConstInt(code as i64 - 128),
        193..=208 => Rdna2Opnd::ConstInt(-(code as i64 - 192)),
        240 => Rdna2Opnd::ConstF32(0.5),
        241 => Rdna2Opnd::ConstF32(-0.5),
        242 => Rdna2Opnd::ConstF32(1.0),
        243 => Rdna2Opnd::ConstF32(-1.0),
        244 => Rdna2Opnd::ConstF32(2.0),
        245 => Rdna2Opnd::ConstF32(-2.0),
        246 => Rdna2Opnd::ConstF32(4.0),
        247 => Rdna2Opnd::ConstF32(-4.0),
        248 => Rdna2Opnd::ConstF32(std::f32::consts::FRAC_1_PI * 0.5),
        253 => Rdna2Opnd::Scc,
        255 => Rdna2Opnd::Literal(lit.unwrap_or(0)),
        other => Rdna2Opnd::Raw(other),
    }
}

fn src9(code: u16, lit: Option<u32>) -> Rdna2Opnd {
    if code >= 256 {
        Rdna2Opnd::Vgpr((code - 256) as u8)
    } else {
        ssrc8(code, lit)
    }
}

fn decode_operands(enc: Rdna2Enc, raw: &[u32]) -> (Vec<Rdna2Opnd>, Vec<Rdna2Opnd>, Rdna2Aux) {
    let w = raw[0];
    let lit = raw.get(1).copied();
    let mut aux = Rdna2Aux::default();
    match enc {
        Rdna2Enc::Sop2 => (
            vec![ssrc8(((w >> 16) & 0x7F) as u16, None)],
            vec![
                ssrc8((w & 0xFF) as u16, lit),
                ssrc8(((w >> 8) & 0xFF) as u16, lit),
            ],
            aux,
        ),
        Rdna2Enc::Sop1 => (
            vec![ssrc8(((w >> 16) & 0x7F) as u16, None)],
            vec![ssrc8((w & 0xFF) as u16, lit)],
            aux,
        ),
        Rdna2Enc::Sopk => {
            aux.imm = ((w & 0xFFFF) as i16) as i64;
            (vec![ssrc8(((w >> 16) & 0x7F) as u16, None)], Vec::new(), aux)
        }
        Rdna2Enc::Sopc => (
            vec![Rdna2Opnd::Scc],
            vec![
                ssrc8((w & 0xFF) as u16, lit),
                ssrc8(((w >> 8) & 0xFF) as u16, lit),
            ],
            aux,
        ),
        Rdna2Enc::Sopp => {
            aux.imm = ((w & 0xFFFF) as i16) as i64;
            (Vec::new(), Vec::new(), aux)
        }
        Rdna2Enc::Smem => {
            let sdata = (w >> 6) & 0x7F;
            let sbase = (w & 0x3F) * 2;
            if let Some(d1) = raw.get(1) {
                aux.imm = (d1 & 0x1F_FFFF) as i64;
            }
            (
                vec![ssrc8(sdata as u16, None)],
                vec![Rdna2Opnd::Sgpr(sbase as u8)],
                aux,
            )
        }
        Rdna2Enc::Vop2 => {
            let vdst = ((w >> 17) & 0xFF) as u8;
            let vsrc1 = ((w >> 9) & 0xFF) as u8;
            let src0 = w & 0x1FF;
            if src0 == SRC_SDWA {
                let sd = raw.get(1).copied().unwrap_or(0);
                aux.sdwa = Some(sd);
                let s0 = if (sd >> 23) & 1 != 0 {
                    ssrc8((sd & 0xFF) as u16, None)
                } else {
                    Rdna2Opnd::Vgpr((sd & 0xFF) as u8)
                };
                let s1 = if (sd >> 31) & 1 != 0 {
                    ssrc8(vsrc1 as u16, None)
                } else {
                    Rdna2Opnd::Vgpr(vsrc1)
                };
                return (vec![Rdna2Opnd::Vgpr(vdst)], vec![s0, s1], aux);
            }
            let op = (w >> 25) & 0x3F;
            let mut srcs = vec![src9(src0 as u16, lit), Rdna2Opnd::Vgpr(vsrc1)];
            if matches!(op, 0x20 | 0x21 | 0x2C | 0x2D | 0x37 | 0x38) {
                if let Some(k) = raw.get(1).copied() {
                    srcs.push(Rdna2Opnd::Literal(k));
                }
            }
            (vec![Rdna2Opnd::Vgpr(vdst)], srcs, aux)
        }
        Rdna2Enc::Vop1 => {
            let vdst = ((w >> 17) & 0xFF) as u8;
            let src0 = w & 0x1FF;
            let dst = if ((w >> 9) & 0xFF) == 0x02 {
                ssrc8(vdst as u16, None)
            } else {
                Rdna2Opnd::Vgpr(vdst)
            };
            if src0 == SRC_SDWA {
                let sd = raw.get(1).copied().unwrap_or(0);
                aux.sdwa = Some(sd);
                let s0 = if (sd >> 23) & 1 != 0 {
                    ssrc8((sd & 0xFF) as u16, None)
                } else {
                    Rdna2Opnd::Vgpr((sd & 0xFF) as u8)
                };
                return (vec![dst], vec![s0], aux);
            }
            (vec![dst], vec![src9(src0 as u16, lit)], aux)
        }
        Rdna2Enc::Vopc => {
            let vsrc1 = ((w >> 9) & 0xFF) as u8;
            let src0 = w & 0x1FF;
            let dst = if (w >> 21) & 1 != 0 {
                Rdna2Opnd::ExecLo
            } else {
                Rdna2Opnd::VccLo
            };
            (vec![dst], vec![src9(src0 as u16, lit), Rdna2Opnd::Vgpr(vsrc1)], aux)
        }
        Rdna2Enc::Vop3 => {
            let vdst = (w & 0xFF) as u8;
            let op = if (w >> 26) == 0b110011 {
                0x400 | ((w >> 16) & 0x7F)
            } else {
                (w >> 16) & 0x3FF
            };
            let d1 = raw.get(1).copied().unwrap_or(0);
            let l3 = raw.get(2).copied();
            aux.mask = (((w >> 8) & 7) | (((d1 >> 29) & 7) << 3)) as u8;
            aux.imm2 = ((d1 >> 27) & 3) as i64;
            aux.imm = ((w >> 15) & 1) as i64;
            let dst = if op < 0x100 && (op & 0x10) != 0 {
                Rdna2Opnd::ExecLo
            } else if op < 0x100 {
                ssrc8(vdst as u16, None)
            } else {
                Rdna2Opnd::Vgpr(vdst)
            };
            (
                vec![dst],
                vec![
                    src9((d1 & 0x1FF) as u16, l3),
                    src9(((d1 >> 9) & 0x1FF) as u16, l3),
                    src9(((d1 >> 18) & 0x1FF) as u16, l3),
                ],
                aux,
            )
        }
        Rdna2Enc::Vinterp => {
            aux.imm = ((w >> 10) & 0x3F) as i64;
            aux.imm2 = ((w >> 8) & 3) as i64;
            (
                vec![Rdna2Opnd::Vgpr(((w >> 18) & 0xFF) as u8)],
                vec![Rdna2Opnd::Vgpr((w & 0xFF) as u8)],
                aux,
            )
        }
        Rdna2Enc::Mimg => {
            let d1 = raw.get(1).copied().unwrap_or(0);
            let nsa = ((w >> 1) & 3) as usize;
            aux.mask = ((w >> 8) & 0xF) as u8;
            aux.imm = ((w >> 3) & 7) as i64;
            aux.imm2 = nsa as i64;
            let mut s = vec![Rdna2Opnd::Vgpr((d1 & 0xFF) as u8)];
            for k in 0..nsa {
                let x = raw.get(2 + k).copied().unwrap_or(0);
                for b in 0..4 {
                    s.push(Rdna2Opnd::Vgpr(((x >> (8 * b)) & 0xFF) as u8));
                }
            }
            s.push(Rdna2Opnd::Sgpr((((d1 >> 16) & 0x1F) * 4) as u8));
            s.push(Rdna2Opnd::Sgpr((((d1 >> 21) & 0x1F) * 4) as u8));
            (vec![Rdna2Opnd::Vgpr(((d1 >> 8) & 0xFF) as u8)], s, aux)
        }
        Rdna2Enc::Mubuf | Rdna2Enc::Mtbuf => {
            let d1 = raw.get(1).copied().unwrap_or(0);
            aux.imm = (w & 0xFFF) as i64;
            aux.mask = (((w >> 12) & 1) | (((w >> 13) & 1) << 1) | (((w >> 14) & 1) << 2)) as u8;
            (
                vec![Rdna2Opnd::Vgpr(((d1 >> 8) & 0xFF) as u8)],
                vec![
                    Rdna2Opnd::Vgpr((d1 & 0xFF) as u8),
                    Rdna2Opnd::Sgpr((((d1 >> 16) & 0x1F) * 4) as u8),
                    ssrc8(((d1 >> 24) & 0xFF) as u16, None),
                ],
                aux,
            )
        }
        Rdna2Enc::Flat => {
            let d1 = raw.get(1).copied().unwrap_or(0);
            aux.mask = ((w >> 14) & 3) as u8;
            aux.imm = (w & 0xFFF) as i64;
            let saddr = (d1 >> 16) & 0x7F;
            (
                vec![Rdna2Opnd::Vgpr((d1 >> 24) as u8)],
                vec![
                    Rdna2Opnd::Vgpr((d1 & 0xFF) as u8),
                    Rdna2Opnd::Vgpr(((d1 >> 8) & 0xFF) as u8),
                    if saddr == 0x7F || aux.mask == 0 {
                        Rdna2Opnd::Null
                    } else {
                        ssrc8(saddr as u16, None)
                    },
                ],
                aux,
            )
        }
        Rdna2Enc::Ds => {
            let d1 = raw.get(1).copied().unwrap_or(0);
            aux.imm = (w & 0xFFFF) as i64;
            (
                vec![Rdna2Opnd::Vgpr((d1 >> 24) as u8)],
                vec![
                    Rdna2Opnd::Vgpr((d1 & 0xFF) as u8),
                    Rdna2Opnd::Vgpr(((d1 >> 8) & 0xFF) as u8),
                    Rdna2Opnd::Vgpr(((d1 >> 16) & 0xFF) as u8),
                ],
                aux,
            )
        }
        Rdna2Enc::Exp => {
            aux.mask = (w & 0xF) as u8;
            aux.imm = ((w >> 4) & 0x3F) as i64;
            aux.imm2 = ((w >> 10) & 7) as i64;
            let d1 = raw.get(1).copied().unwrap_or(0);
            (
                Vec::new(),
                vec![
                    Rdna2Opnd::Vgpr((d1 & 0xFF) as u8),
                    Rdna2Opnd::Vgpr(((d1 >> 8) & 0xFF) as u8),
                    Rdna2Opnd::Vgpr(((d1 >> 16) & 0xFF) as u8),
                    Rdna2Opnd::Vgpr(((d1 >> 24) & 0xFF) as u8),
                ],
                aux,
            )
        }
        Rdna2Enc::Unknown => (Vec::new(), Vec::new(), aux),
    }
}

fn mnemonic_for(enc: Rdna2Enc, op: u32) -> String {
    if enc == Rdna2Enc::Mimg && (0x20..0x60).contains(&op) {
        let base = if op >= 0x40 { "image_gather4" } else { "image_sample" };
        let modi = ["", "_cl", "_d", "_d_cl", "_l", "_b", "_b_cl", "_lz"][(op & 7) as usize];
        return format!(
            "{base}{}{modi}{}",
            if op & 8 != 0 { "_c" } else { "" },
            if op & 0x10 != 0 { "_o" } else { "" }
        );
    }
    let known = match (enc, op) {
        // SOPP
        (Rdna2Enc::Sopp, 0x00) => Some("s_nop"),
        (Rdna2Enc::Sopp, 0x01) => Some("s_endpgm"),
        (Rdna2Enc::Sopp, 0x02) => Some("s_branch"),
        (Rdna2Enc::Sopp, 0x04) => Some("s_cbranch_scc0"),
        (Rdna2Enc::Sopp, 0x05) => Some("s_cbranch_scc1"),
        (Rdna2Enc::Sopp, 0x06) => Some("s_cbranch_vccz"),
        (Rdna2Enc::Sopp, 0x07) => Some("s_cbranch_vccnz"),
        (Rdna2Enc::Sopp, 0x08) => Some("s_cbranch_execz"),
        (Rdna2Enc::Sopp, 0x09) => Some("s_cbranch_execnz"),
        (Rdna2Enc::Sopp, 0x0A) => Some("s_barrier"),
        (Rdna2Enc::Sopp, 0x0C) => Some("s_waitcnt"),
        (Rdna2Enc::Sopp, 0x0F) => Some("s_waitcnt_vscnt"),
        (Rdna2Enc::Sopp, 0x20) => Some("s_clause"),
        // SOP1
        (Rdna2Enc::Sop1, 0x03) => Some("s_mov_b32"),
        (Rdna2Enc::Sop1, 0x04) => Some("s_mov_b64"),
        (Rdna2Enc::Sop1, 0x07) => Some("s_not_b32"),
        (Rdna2Enc::Sop1, 0x1C) => Some("s_getpc_b64"),
        (Rdna2Enc::Sop1, 0x1D) => Some("s_setpc_b64"),
        (Rdna2Enc::Sop1, 0x24) => Some("s_and_saveexec_b32"),
        (Rdna2Enc::Sop1, 0x3C) => Some("s_and_saveexec_b64"),
        // SOP2
        (Rdna2Enc::Sop2, 0x00) => Some("s_add_u32"),
        (Rdna2Enc::Sop2, 0x01) => Some("s_sub_u32"),
        (Rdna2Enc::Sop2, 0x02) => Some("s_add_i32"),
        (Rdna2Enc::Sop2, 0x03) => Some("s_sub_i32"),
        (Rdna2Enc::Sop2, 0x04) => Some("s_addc_u32"),
        (Rdna2Enc::Sop2, 0x0E) => Some("s_and_b32"),
        (Rdna2Enc::Sop2, 0x10) => Some("s_or_b32"),
        (Rdna2Enc::Sop2, 0x12) => Some("s_xor_b32"),
        (Rdna2Enc::Sop2, 0x1E) => Some("s_lshl_b32"),
        (Rdna2Enc::Sop2, 0x20) => Some("s_lshr_b32"),
        (Rdna2Enc::Sop2, 0x22) => Some("s_ashr_i32"),
        (Rdna2Enc::Sop2, 0x2C) => Some("s_mul_i32"),
        // SOPK
        (Rdna2Enc::Sopk, 0x00) => Some("s_movk_i32"),
        (Rdna2Enc::Sopk, 0x0F) => Some("s_addk_i32"),
        (Rdna2Enc::Sopk, 0x10) => Some("s_mulk_i32"),
        // SOPC
        (Rdna2Enc::Sopc, 0x00) => Some("s_cmp_eq_i32"),
        (Rdna2Enc::Sopc, 0x01) => Some("s_cmp_lg_i32"),
        (Rdna2Enc::Sopc, 0x02) => Some("s_cmp_gt_i32"),
        (Rdna2Enc::Sopc, 0x04) => Some("s_cmp_lt_i32"),
        (Rdna2Enc::Sopc, 0x06) => Some("s_cmp_eq_u32"),
        (Rdna2Enc::Sopc, 0x07) => Some("s_cmp_lg_u32"),
        // SMEM
        (Rdna2Enc::Smem, 0x00) => Some("s_load_dword"),
        (Rdna2Enc::Smem, 0x01) => Some("s_load_dwordx2"),
        (Rdna2Enc::Smem, 0x02) => Some("s_load_dwordx4"),
        (Rdna2Enc::Smem, 0x03) => Some("s_load_dwordx8"),
        (Rdna2Enc::Smem, 0x04) => Some("s_load_dwordx16"),
        (Rdna2Enc::Smem, 0x08) => Some("s_buffer_load_dword"),
        (Rdna2Enc::Smem, 0x09) => Some("s_buffer_load_dwordx2"),
        (Rdna2Enc::Smem, 0x0A) => Some("s_buffer_load_dwordx4"),
        (Rdna2Enc::Smem, 0x0B) => Some("s_buffer_load_dwordx8"),
        (Rdna2Enc::Smem, 0x0C) => Some("s_buffer_load_dwordx16"),
        // VOP1
        (Rdna2Enc::Vop1, 0x00) => Some("v_nop"),
        (Rdna2Enc::Vop1, 0x01) => Some("v_mov_b32"),
        (Rdna2Enc::Vop1, 0x02) => Some("v_readfirstlane_b32"),
        (Rdna2Enc::Vop1, 0x05) => Some("v_cvt_f32_i32"),
        (Rdna2Enc::Vop1, 0x06) => Some("v_cvt_f32_u32"),
        (Rdna2Enc::Vop1, 0x07) => Some("v_cvt_u32_f32"),
        (Rdna2Enc::Vop1, 0x08) => Some("v_cvt_i32_f32"),
        (Rdna2Enc::Vop1, 0x1B) => Some("v_fract_f32"),
        (Rdna2Enc::Vop1, 0x21) => Some("v_trunc_f32"),
        (Rdna2Enc::Vop1, 0x22) => Some("v_ceil_f32"),
        (Rdna2Enc::Vop1, 0x24) => Some("v_floor_f32"),
        (Rdna2Enc::Vop1, 0x25) => Some("v_exp_f32"),
        (Rdna2Enc::Vop1, 0x27) => Some("v_log_f32"),
        (Rdna2Enc::Vop1, 0x2A) => Some("v_rcp_f32"),
        (Rdna2Enc::Vop1, 0x2E) => Some("v_rsq_f32"),
        (Rdna2Enc::Vop1, 0x33) => Some("v_sqrt_f32"),
        (Rdna2Enc::Vop1, 0x35) => Some("v_sin_f32"),
        (Rdna2Enc::Vop1, 0x36) => Some("v_cos_f32"),
        (Rdna2Enc::Vop1, 0x37) => Some("v_not_b32"),
        // VOP2
        (Rdna2Enc::Vop2, 0x01) => Some("v_cndmask_b32"),
        (Rdna2Enc::Vop2, 0x03) => Some("v_add_f32"),
        (Rdna2Enc::Vop2, 0x04) => Some("v_sub_f32"),
        (Rdna2Enc::Vop2, 0x05) => Some("v_subrev_f32"),
        (Rdna2Enc::Vop2, 0x08) => Some("v_mul_f32"),
        (Rdna2Enc::Vop2, 0x0F) => Some("v_min_f32"),
        (Rdna2Enc::Vop2, 0x10) => Some("v_max_f32"),
        (Rdna2Enc::Vop2, 0x18) => Some("v_lshrrev_b32"),
        (Rdna2Enc::Vop2, 0x1A) => Some("v_lshlrev_b32"),
        (Rdna2Enc::Vop2, 0x1B) => Some("v_and_b32"),
        (Rdna2Enc::Vop2, 0x1C) => Some("v_or_b32"),
        (Rdna2Enc::Vop2, 0x1D) => Some("v_xor_b32"),
        (Rdna2Enc::Vop2, 0x25) => Some("v_add_nc_u32"),
        (Rdna2Enc::Vop2, 0x26) => Some("v_sub_nc_u32"),
        (Rdna2Enc::Vop2, 0x2B) => Some("v_fmac_f32"),
        (Rdna2Enc::Vop2, 0x2C) => Some("v_fmamk_f32"),
        (Rdna2Enc::Vop2, 0x2D) => Some("v_fmaak_f32"),
        // VOPC
        (Rdna2Enc::Vopc, 0x41) => Some("v_cmp_lt_f32"),
        (Rdna2Enc::Vopc, 0x42) => Some("v_cmp_eq_f32"),
        (Rdna2Enc::Vopc, 0x43) => Some("v_cmp_le_f32"),
        (Rdna2Enc::Vopc, 0x44) => Some("v_cmp_gt_f32"),
        (Rdna2Enc::Vopc, 0x46) => Some("v_cmp_ge_f32"),
        // VOP3
        (Rdna2Enc::Vop3, 0x141) => Some("v_mad_f32"),
        (Rdna2Enc::Vop3, 0x14B) => Some("v_fma_f32"),
        (Rdna2Enc::Vop3, 0x169) => Some("v_mul_lo_u32"),
        // VINTRP
        (Rdna2Enc::Vinterp, 0x00) => Some("v_interp_p1_f32"),
        (Rdna2Enc::Vinterp, 0x01) => Some("v_interp_p2_f32"),
        (Rdna2Enc::Vinterp, 0x02) => Some("v_interp_mov_f32"),
        // MUBUF
        (Rdna2Enc::Mubuf, 0x14) => Some("buffer_load_dword"),
        (Rdna2Enc::Mubuf, 0x15) => Some("buffer_load_dwordx2"),
        (Rdna2Enc::Mubuf, 0x16) => Some("buffer_load_dwordx3"),
        (Rdna2Enc::Mubuf, 0x17) => Some("buffer_load_dwordx4"),
        (Rdna2Enc::Mubuf, 0x1C) => Some("buffer_store_dword"),
        (Rdna2Enc::Mubuf, 0x1D) => Some("buffer_store_dwordx2"),
        (Rdna2Enc::Mubuf, 0x1E) => Some("buffer_store_dwordx3"),
        (Rdna2Enc::Mubuf, 0x1F) => Some("buffer_store_dwordx4"),
        // EXP
        (Rdna2Enc::Exp, _) => Some("exp"),
        _ => None,
    };
    match known {
        Some(name) => name.to_string(),
        None => format!("{enc:?}<0x{op:x}>").to_ascii_lowercase(),
    }
}

/// Decode a raw RDNA 2 ISA byte stream into structured instructions.
pub fn disassemble_rdna2(isa: &[u8]) -> Vec<Rdna2Insn> {
    let words: Vec<u32> = isa
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        let enc = classify(w);
        let next = words.get(i + 1).copied();
        let (base_len, lit) = instr_len(enc, w, next);
        let total = (base_len + usize::from(lit)).min(words.len() - i).max(1);
        let raw = words[i..i + total].to_vec();
        let op = opcode_field(enc, w, next);
        let mnemonic = mnemonic_for(enc, op);
        let (dst, src, aux) = decode_operands(enc, &raw);
        out.push(Rdna2Insn {
            offset: i * 4,
            dwords: total,
            enc,
            op,
            raw,
            mnemonic,
            dst,
            src,
            aux,
        });
        i += total;
    }
    out
}

/// Format a decoded instruction list into human-readable RDNA 2 assembly text.
pub fn format_rdna2_disassembly(insns: &[Rdna2Insn]) -> String {
    let mut out = String::new();
    for insn in insns {
        out.push_str(&insn.format_line());
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Minimal RDNA 2 instruction encoders (useful for unit tests & synthetic shaders)
// ---------------------------------------------------------------------------

pub mod encode {
    /// Encode `s_endpgm` (SOPP op 0x01).
    pub const fn s_endpgm() -> u32 {
        (0b1_0111_1111u32 << 23) | (0x01u32 << 16)
    }

    /// Encode `s_waitcnt vmcnt(vm) expcnt(exp) lgkmcnt(lgkm)`.
    pub const fn s_waitcnt(vm: u16, exp: u16, lgkm: u16) -> u32 {
        let imm = ((vm & 0xF) | ((exp & 0x7) << 4) | ((lgkm & 0x3F) << 8)) as u32;
        (0b1_0111_1111u32 << 23) | (0x0Cu32 << 16) | imm
    }

    /// Encode `s_mov_b32 sdst, ssrc0` (SOP1 op 0x03). If `ssrc0 == 255`, follow with a literal dword.
    pub const fn s_mov_b32(sdst: u8, ssrc0: u8) -> u32 {
        (0b1_0111_1101u32 << 23) | ((sdst as u32) << 16) | (0x03u32 << 8) | (ssrc0 as u32)
    }

    /// Encode `v_mov_b32 vdst, src0` (VOP1 op 0x01). For VGPR `n`, pass `256 + n`; for literal, pass `255`.
    pub const fn v_mov_b32(vdst: u8, src0: u16) -> u32 {
        (0b011_1111u32 << 25) | ((vdst as u32) << 17) | (0x01u32 << 9) | ((src0 as u32) & 0x1FF)
    }

    /// Encode `v_add_f32 vdst, src0, vsrc1` (VOP2 op 0x03).
    pub const fn v_add_f32(vdst: u8, src0: u16, vsrc1: u8) -> u32 {
        (0x03u32 << 25) | ((vdst as u32) << 17) | ((vsrc1 as u32) << 9) | ((src0 as u32) & 0x1FF)
    }

    /// Encode `v_mul_f32 vdst, src0, vsrc1` (VOP2 op 0x08).
    pub const fn v_mul_f32(vdst: u8, src0: u16, vsrc1: u8) -> u32 {
        (0x08u32 << 25) | ((vdst as u32) << 17) | ((vsrc1 as u32) << 9) | ((src0 as u32) & 0x1FF)
    }

    /// Encode `v_fma_f32 vdst, src0, src1, src2` (VOP3 op 0x14B) -> 2 dwords.
    pub const fn v_fma_f32(vdst: u8, src0: u16, src1: u16, src2: u16) -> [u32; 2] {
        let d0 = (0b110101u32 << 26) | (0x14Bu32 << 16) | (vdst as u32);
        let d1 = ((src0 as u32) & 0x1FF)
            | (((src1 as u32) & 0x1FF) << 9)
            | (((src2 as u32) & 0x1FF) << 18);
        [d0, d1]
    }

    /// Encode `exp target, v0, v1, v2, v3` (EXP) -> 2 dwords.
    pub const fn exp(target: u8, en_mask: u8, done: bool, v: [u8; 4]) -> [u32; 2] {
        let d0 = (0b111110u32 << 26)
            | (if done { 1u32 << 11 } else { 0 })
            | (((target as u32) & 0x3F) << 4)
            | ((en_mask as u32) & 0xF);
        let d1 = (v[0] as u32)
            | ((v[1] as u32) << 8)
            | ((v[2] as u32) << 16)
            | ((v[3] as u32) << 24);
        [d0, d1]
    }

    /// Pack slice of little-endian dwords into raw ISA bytes.
    pub fn words_to_bytes(words: &[u32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(words.len() * 4);
        for &w in words {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_rdna2_pixel_shader_with_literals_and_export() {
        let fma = encode::v_fma_f32(3, 256, 257, 258);
        let ex = encode::exp(0, 0xF, true, [0, 1, 2, 3]);
        let words = [
            encode::v_mov_b32(0, 255),
            1.0f32.to_bits(),
            encode::v_mov_b32(1, 240), // inline 0.5
            encode::v_add_f32(2, 256, 1),
            fma[0],
            fma[1],
            ex[0],
            ex[1],
            encode::s_endpgm(),
        ];
        let isa = encode::words_to_bytes(&words);
        let insns = disassemble_rdna2(&isa);
        assert_eq!(insns.len(), 6);
        assert_eq!(insns[0].mnemonic, "v_mov_b32");
        assert_eq!(insns[0].dwords, 2);
        assert_eq!(insns[0].src[0], Rdna2Opnd::Literal(1.0f32.to_bits()));
        assert_eq!(insns[1].src[0], Rdna2Opnd::ConstF32(0.5));
        assert_eq!(insns[2].mnemonic, "v_add_f32");
        assert_eq!(insns[3].mnemonic, "v_fma_f32");
        assert_eq!(insns[4].mnemonic, "exp");
        assert_eq!(insns[4].aux.imm, 0); // mrt0
        assert!(insns[5].is_endpgm());
        let text = format_rdna2_disassembly(&insns);
        assert!(text.contains("v_fma_f32"));
        assert!(text.contains("mrt0"));
        assert!(text.contains("s_endpgm"));
    }
}
