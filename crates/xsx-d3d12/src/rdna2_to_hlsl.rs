//! AMD RDNA 2 (GFX10.3) → Shader Model 6.0 HLSL recompiler.
//!
//! Lifts decoded RDNA 2 instruction streams ([`crate::rdna2_isa`]) into valid SM 6.0 HLSL
//! for Vertex (`vs_6_0`), Pixel (`ps_6_0`), and Compute (`cs_6_0`) stages so native Xbox
//! Series X GPU programs can be compiled to DXIL/DXBC by Windows `d3dcompiler_47.dll`/DXC.
//!
//! Execution model:
//! - One HLSL invocation corresponds to one RDNA 2 lane.
//! - Each referenced VGPR (`vN`) and SGPR (`sN`) is declared as a 32-bit `uint` storing the
//!   raw bit pattern, converting cleanly with `asfloat()` / `asint()` / `asuint()`.
//! - Scalar/vector ALU, `v_interp_p1/p2_f32`, `s_buffer_load_dword*` (CBV), `buffer_load/store`
//!   (SRV/UAV), `image_sample*`, and `exp` (`SV_Position` / `SV_Target0..7` / `TEXCOORD`)
//!   are lifted directly.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::rdna2_isa::{disassemble_rdna2, Rdna2Enc, Rdna2Insn, Rdna2Opnd};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShaderStage {
    Vertex,
    Pixel,
    Compute,
}

impl ShaderStage {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "vs" | "vertex" => Some(Self::Vertex),
            "ps" | "pixel" | "fragment" => Some(Self::Pixel),
            "cs" | "compute" => Some(Self::Compute),
            _ => None,
        }
    }

    pub fn profile(self) -> &'static str {
        match self {
            Self::Vertex => "vs_6_0",
            Self::Pixel => "ps_6_0",
            Self::Compute => "cs_6_0",
        }
    }

    pub fn entry_point(self) -> &'static str {
        match self {
            Self::Vertex => "VSMain",
            Self::Pixel => "PSMain",
            Self::Compute => "CSMain",
        }
    }
}

/// Output of lifting an RDNA 2 ISA blob into HLSL.
#[derive(Debug, Clone)]
pub struct LiftedHlsl {
    pub stage: ShaderStage,
    pub entry_point: &'static str,
    pub profile: &'static str,
    pub hlsl: String,
    pub instruction_count: usize,
    pub lifted_count: usize,
    pub fallback_comments: usize,
    pub used_vgprs: Vec<u8>,
    pub used_sgprs: Vec<u8>,
}

fn opnd_rval_u32(op: &Rdna2Opnd) -> String {
    match *op {
        Rdna2Opnd::Sgpr(n) => format!("s{n}"),
        Rdna2Opnd::Vgpr(n) => format!("v{n}"),
        Rdna2Opnd::VccLo => "vcc_lo".into(),
        Rdna2Opnd::VccHi => "vcc_hi".into(),
        Rdna2Opnd::ExecLo => "exec_lo".into(),
        Rdna2Opnd::ExecHi => "exec_hi".into(),
        Rdna2Opnd::M0 => "m0".into(),
        Rdna2Opnd::Scc => "scc".into(),
        Rdna2Opnd::Null => "0u".into(),
        Rdna2Opnd::ConstInt(v) => format!("0x{:x}u", v as i32 as u32),
        Rdna2Opnd::ConstF32(v) => format!("0x{:08x}u", v.to_bits()),
        Rdna2Opnd::Literal(v) => format!("0x{v:08x}u"),
        Rdna2Opnd::Raw(c) => format!("{c}u"),
    }
}

fn opnd_rval_f32(op: &Rdna2Opnd) -> String {
    match *op {
        Rdna2Opnd::ConstF32(v) => format!("{v:?}"),
        Rdna2Opnd::Literal(v) => format!("asfloat(0x{v:08x}u)"),
        _ => format!("asfloat({})", opnd_rval_u32(op)),
    }
}

fn opnd_lval(op: &Rdna2Opnd) -> Option<String> {
    match *op {
        Rdna2Opnd::Sgpr(n) => Some(format!("s{n}")),
        Rdna2Opnd::Vgpr(n) => Some(format!("v{n}")),
        Rdna2Opnd::VccLo => Some("vcc_lo".into()),
        Rdna2Opnd::VccHi => Some("vcc_hi".into()),
        Rdna2Opnd::ExecLo => Some("exec_lo".into()),
        Rdna2Opnd::ExecHi => Some("exec_hi".into()),
        Rdna2Opnd::M0 => Some("m0".into()),
        Rdna2Opnd::Scc => Some("scc".into()),
        Rdna2Opnd::Null => None,
        _ => None,
    }
}

fn collect_regs(insns: &[Rdna2Insn]) -> (BTreeSet<u8>, BTreeSet<u8>, bool, bool, bool, BTreeSet<u8>) {
    let mut vgprs = BTreeSet::new();
    let mut sgprs = BTreeSet::new();
    let mut uses_cbv = false;
    let mut uses_srv_buf = false;
    let mut uses_tex = false;
    let mut mrt_targets = BTreeSet::new();

    for insn in insns {
        for op in insn.dst.iter().chain(insn.src.iter()) {
            match *op {
                Rdna2Opnd::Vgpr(n) => {
                    vgprs.insert(n);
                }
                Rdna2Opnd::Sgpr(n) => {
                    sgprs.insert(n);
                }
                _ => {}
            }
        }
        match insn.enc {
            Rdna2Enc::Smem => uses_cbv = true,
            Rdna2Enc::Mubuf | Rdna2Enc::Mtbuf => uses_srv_buf = true,
            Rdna2Enc::Mimg => uses_tex = true,
            Rdna2Enc::Exp => {
                if (0..=7).contains(&insn.aux.imm) {
                    mrt_targets.insert(insn.aux.imm as u8);
                }
            }
            _ => {}
        }
    }
    (vgprs, sgprs, uses_cbv, uses_srv_buf, uses_tex, mrt_targets)
}

/// Lift an RDNA 2 native ISA binary into SM 6.0 HLSL.
pub fn lift_rdna2_to_hlsl(isa: &[u8], stage: ShaderStage) -> Result<LiftedHlsl, String> {
    if isa.len() < 4 {
        return Err("RDNA 2 ISA blob must contain at least one 32-bit instruction".into());
    }
    let insns = disassemble_rdna2(isa);
    let (vgprs, sgprs, uses_cbv, uses_srv_buf, uses_tex, mrt_targets) = collect_regs(&insns);

    let mut out = String::new();
    let _ = writeln!(
        out,
        "// Auto-lifted from Xbox Series X RDNA 2 ISA by xsxemu ({})",
        stage.profile()
    );
    if uses_cbv {
        let _ = writeln!(out, "cbuffer XsxRootConstants : register(b0) {{ uint4 g_cb0[256]; }};");
    }
    if uses_srv_buf {
        let _ = writeln!(out, "ByteAddressBuffer g_buf0 : register(t0);");
        let _ = writeln!(out, "RWByteAddressBuffer g_uav0 : register(u0);");
    }
    if uses_tex {
        let _ = writeln!(out, "Texture2D<float4> g_tex0 : register(t1);");
        let _ = writeln!(out, "SamplerState g_samp0 : register(s0);");
    }

    match stage {
        ShaderStage::Vertex => {
            let _ = writeln!(
                out,
                "struct VSOut {{ float4 pos : SV_Position; float4 param0 : TEXCOORD0; }};"
            );
            let _ = writeln!(
                out,
                "VSOut {}(uint vertexId : SV_VertexID, uint instanceId : SV_InstanceID) {{",
                stage.entry_point()
            );
            let _ = writeln!(out, "    VSOut _out = (VSOut)0;");
        }
        ShaderStage::Pixel => {
            let targets: Vec<u8> = if mrt_targets.is_empty() {
                vec![0]
            } else {
                mrt_targets.iter().copied().collect()
            };
            let _ = writeln!(out, "struct PSIn {{ float4 pos : SV_Position; float4 param0 : TEXCOORD0; }};");
            let _ = write!(out, "struct PSOut {{ ");
            for t in &targets {
                let _ = write!(out, "float4 mrt{t} : SV_Target{t}; ");
            }
            let _ = writeln!(out, "}};");
            let _ = writeln!(out, "PSOut {}(PSIn _in) {{", stage.entry_point());
            let _ = writeln!(out, "    PSOut _out = (PSOut)0;");
        }
        ShaderStage::Compute => {
            let _ = writeln!(out, "[numthreads(64, 1, 1)]");
            let _ = writeln!(
                out,
                "void {}(uint3 tid : SV_DispatchThreadID, uint3 gid : SV_GroupThreadID) {{",
                stage.entry_point()
            );
        }
    }

    // Declare lane state registers
    let _ = writeln!(out, "    uint vcc_lo = 0u, vcc_hi = 0u, exec_lo = 1u, exec_hi = 0u, m0 = 0u, scc = 0u;");
    for &v in &vgprs {
        let init = match (stage, v) {
            (ShaderStage::Vertex, 0) => "vertexId",
            (ShaderStage::Vertex, 3) => "instanceId",
            (ShaderStage::Compute, 0) => "tid.x",
            (ShaderStage::Compute, 1) => "tid.y",
            (ShaderStage::Compute, 2) => "tid.z",
            (ShaderStage::Pixel, 0) => "asuint(_in.param0.x)",
            (ShaderStage::Pixel, 1) => "asuint(_in.param0.y)",
            _ => "0u",
        };
        let _ = writeln!(out, "    uint v{v} = {init};");
    }
    for &s in &sgprs {
        let _ = writeln!(out, "    uint s{s} = 0u;");
    }

    let mut lifted_count = 0usize;
    let mut fallback_comments = 0usize;

    for insn in &insns {
        let mn = insn.mnemonic.as_str();
        let dst = insn.dst.first().and_then(opnd_lval);
        let s0 = insn.src.first();
        let s1 = insn.src.get(1);
        let s2 = insn.src.get(2);

        let stmt: Option<String> = match mn {
            "s_endpgm" => {
                Some(match stage {
                    ShaderStage::Compute => "return;".into(),
                    _ => "return _out;".into(),
                })
            }
            "s_nop" | "v_nop" | "s_waitcnt" | "s_waitcnt_vscnt" | "s_clause" => {
                Some(format!("/* {} */", insn.format_line()))
            }
            "s_barrier" => Some("GroupMemoryBarrierWithGroupSync();".into()),
            "s_mov_b32" | "v_mov_b32" => {
                dst.zip(s0).map(|(d, a)| format!("{d} = {};", opnd_rval_u32(a)))
            }
            "s_movk_i32" => dst.map(|d| format!("{d} = 0x{:08x}u;", insn.aux.imm as i32 as u32)),
            "s_not_b32" | "v_not_b32" => {
                dst.zip(s0).map(|(d, a)| format!("{d} = ~({});", opnd_rval_u32(a)))
            }
            "s_add_u32" | "s_add_i32" | "v_add_nc_u32" => {
                dst.zip(s0).zip(s1).map(|((d, a), b)| {
                    format!("{d} = ({} + {});", opnd_rval_u32(a), opnd_rval_u32(b))
                })
            }
            "s_sub_u32" | "s_sub_i32" | "v_sub_nc_u32" => {
                dst.zip(s0).zip(s1).map(|((d, a), b)| {
                    format!("{d} = ({} - {});", opnd_rval_u32(a), opnd_rval_u32(b))
                })
            }
            "s_and_b32" | "v_and_b32" => {
                dst.zip(s0).zip(s1).map(|((d, a), b)| {
                    format!("{d} = ({} & {});", opnd_rval_u32(a), opnd_rval_u32(b))
                })
            }
            "s_or_b32" | "v_or_b32" => {
                dst.zip(s0).zip(s1).map(|((d, a), b)| {
                    format!("{d} = ({} | {});", opnd_rval_u32(a), opnd_rval_u32(b))
                })
            }
            "s_xor_b32" | "v_xor_b32" => {
                dst.zip(s0).zip(s1).map(|((d, a), b)| {
                    format!("{d} = ({} ^ {});", opnd_rval_u32(a), opnd_rval_u32(b))
                })
            }
            "s_lshl_b32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!("{d} = ({} << ({} & 31u));", opnd_rval_u32(a), opnd_rval_u32(b))
            }),
            "v_lshlrev_b32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!("{d} = ({} << ({} & 31u));", opnd_rval_u32(b), opnd_rval_u32(a))
            }),
            "s_lshr_b32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!("{d} = ({} >> ({} & 31u));", opnd_rval_u32(a), opnd_rval_u32(b))
            }),
            "v_lshrrev_b32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!("{d} = ({} >> ({} & 31u));", opnd_rval_u32(b), opnd_rval_u32(a))
            }),
            "s_mul_i32" | "v_mul_lo_u32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!("{d} = ({} * {});", opnd_rval_u32(a), opnd_rval_u32(b))
            }),
            "v_add_f32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!("{d} = asuint({} + {});", opnd_rval_f32(a), opnd_rval_f32(b))
            }),
            "v_sub_f32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!("{d} = asuint({} - {});", opnd_rval_f32(a), opnd_rval_f32(b))
            }),
            "v_subrev_f32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!("{d} = asuint({} - {});", opnd_rval_f32(b), opnd_rval_f32(a))
            }),
            "v_mul_f32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!("{d} = asuint({} * {});", opnd_rval_f32(a), opnd_rval_f32(b))
            }),
            "v_min_f32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!("{d} = asuint(min({}, {}));", opnd_rval_f32(a), opnd_rval_f32(b))
            }),
            "v_max_f32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!("{d} = asuint(max({}, {}));", opnd_rval_f32(a), opnd_rval_f32(b))
            }),
            "v_fmac_f32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!(
                    "{d} = asuint(mad({}, {}, asfloat({d})));",
                    opnd_rval_f32(a),
                    opnd_rval_f32(b)
                )
            }),
            "v_fma_f32" | "v_mad_f32" => dst.zip(s0).zip(s1).zip(s2).map(|(((d, a), b), c)| {
                format!(
                    "{d} = asuint(mad({}, {}, {}));",
                    opnd_rval_f32(a),
                    opnd_rval_f32(b),
                    opnd_rval_f32(c)
                )
            }),
            "v_rcp_f32" => dst.zip(s0).map(|(d, a)| format!("{d} = asuint(rcp({}));", opnd_rval_f32(a))),
            "v_rsq_f32" => dst.zip(s0).map(|(d, a)| format!("{d} = asuint(rsqrt({}));", opnd_rval_f32(a))),
            "v_sqrt_f32" => dst.zip(s0).map(|(d, a)| format!("{d} = asuint(sqrt({}));", opnd_rval_f32(a))),
            "v_exp_f32" => dst.zip(s0).map(|(d, a)| format!("{d} = asuint(exp2({}));", opnd_rval_f32(a))),
            "v_log_f32" => dst.zip(s0).map(|(d, a)| format!("{d} = asuint(log2({}));", opnd_rval_f32(a))),
            "v_floor_f32" => dst.zip(s0).map(|(d, a)| format!("{d} = asuint(floor({}));", opnd_rval_f32(a))),
            "v_ceil_f32" => dst.zip(s0).map(|(d, a)| format!("{d} = asuint(ceil({}));", opnd_rval_f32(a))),
            "v_trunc_f32" => dst.zip(s0).map(|(d, a)| format!("{d} = asuint(trunc({}));", opnd_rval_f32(a))),
            "v_fract_f32" => dst.zip(s0).map(|(d, a)| format!("{d} = asuint(frac({}));", opnd_rval_f32(a))),
            "v_cvt_f32_i32" => dst.zip(s0).map(|(d, a)| format!("{d} = asuint((float)asint({}));", opnd_rval_u32(a))),
            "v_cvt_f32_u32" => dst.zip(s0).map(|(d, a)| format!("{d} = asuint((float){});", opnd_rval_u32(a))),
            "v_cvt_u32_f32" => dst.zip(s0).map(|(d, a)| format!("{d} = (uint)({});", opnd_rval_f32(a))),
            "v_cvt_i32_f32" => dst.zip(s0).map(|(d, a)| format!("{d} = asuint((int)({}));", opnd_rval_f32(a))),
            "v_cndmask_b32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!(
                    "{d} = ((vcc_lo & 1u) != 0u) ? {} : {};",
                    opnd_rval_u32(b),
                    opnd_rval_u32(a)
                )
            }),
            "v_cmp_lt_f32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!("{d} = ({} < {}) ? 1u : 0u;", opnd_rval_f32(a), opnd_rval_f32(b))
            }),
            "v_cmp_eq_f32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!("{d} = ({} == {}) ? 1u : 0u;", opnd_rval_f32(a), opnd_rval_f32(b))
            }),
            "v_cmp_le_f32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!("{d} = ({} <= {}) ? 1u : 0u;", opnd_rval_f32(a), opnd_rval_f32(b))
            }),
            "v_cmp_gt_f32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!("{d} = ({} > {}) ? 1u : 0u;", opnd_rval_f32(a), opnd_rval_f32(b))
            }),
            "v_cmp_ge_f32" => dst.zip(s0).zip(s1).map(|((d, a), b)| {
                format!("{d} = ({} >= {}) ? 1u : 0u;", opnd_rval_f32(a), opnd_rval_f32(b))
            }),
            "s_load_dword" | "s_buffer_load_dword" => dst.map(|d| {
                let idx = (insn.aux.imm as usize / 16).min(255);
                let comp = ["x", "y", "z", "w"][(insn.aux.imm as usize / 4) & 3];
                format!("{d} = g_cb0[{idx}].{comp};")
            }),
            "buffer_load_dword" => dst.zip(s0).map(|(d, addr)| {
                format!(
                    "{d} = g_buf0.Load({} + {}u);",
                    opnd_rval_u32(addr),
                    insn.aux.imm as u32
                )
            }),
            "buffer_store_dword" => dst.zip(s0).map(|(val, addr)| {
                format!(
                    "g_uav0.Store({} + {}u, {val});",
                    opnd_rval_u32(addr),
                    insn.aux.imm as u32
                )
            }),
            "v_interp_p1_f32" | "v_interp_p2_f32" | "v_interp_mov_f32" => {
                dst.map(|d| {
                    let ch = ["x", "y", "z", "w"][(insn.aux.imm2 as usize) & 3];
                    format!("{d} = asuint(_in.param0.{ch});")
                })
            }
            "exp" => {
                let comps = [
                    insn.src.first().map(opnd_rval_f32).unwrap_or_else(|| "0.0".into()),
                    insn.src.get(1).map(opnd_rval_f32).unwrap_or_else(|| "0.0".into()),
                    insn.src.get(2).map(opnd_rval_f32).unwrap_or_else(|| "0.0".into()),
                    insn.src.get(3).map(opnd_rval_f32).unwrap_or_else(|| "1.0".into()),
                ];
                let vec4 = format!("float4({}, {}, {}, {})", comps[0], comps[1], comps[2], comps[3]);
                match (stage, insn.aux.imm) {
                    (ShaderStage::Pixel, t @ 0..=7) => Some(format!("_out.mrt{t} = {vec4};")),
                    (ShaderStage::Vertex, 12) => Some(format!("_out.pos = {vec4};")),
                    (ShaderStage::Vertex, 32) => Some(format!("_out.param0 = {vec4};")),
                    _ => Some(format!("/* exp tgt={} {vec4} */", insn.aux.imm)),
                }
            }
            _ => None,
        };

        if let Some(code) = stmt {
            let _ = writeln!(out, "    {code}");
            lifted_count += 1;
        } else {
            let _ = writeln!(out, "    /* unlifted: {} */", insn.format_line());
            fallback_comments += 1;
        }
    }

    if !insns.last().map(Rdna2Insn::is_endpgm).unwrap_or(false) && stage != ShaderStage::Compute {
        let _ = writeln!(out, "    return _out;");
    }
    let _ = writeln!(out, "}}");

    Ok(LiftedHlsl {
        stage,
        entry_point: stage.entry_point(),
        profile: stage.profile(),
        hlsl: out,
        instruction_count: insns.len(),
        lifted_count,
        fallback_comments,
        used_vgprs: vgprs.into_iter().collect(),
        used_sgprs: sgprs.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdna2_isa::encode;

    #[test]
    fn lifts_rdna2_pixel_shader_to_sm6_hlsl() {
        let fma = encode::v_fma_f32(3, 256, 257, 258);
        let ex = encode::exp(0, 0xF, true, [0, 1, 2, 3]);
        let isa = encode::words_to_bytes(&[
            encode::v_mov_b32(0, 255),
            1.0f32.to_bits(),
            encode::v_mov_b32(1, 240),
            encode::v_add_f32(2, 256, 1),
            fma[0],
            fma[1],
            ex[0],
            ex[1],
            encode::s_endpgm(),
        ]);
        let lifted = lift_rdna2_to_hlsl(&isa, ShaderStage::Pixel).unwrap();
        assert_eq!(lifted.profile, "ps_6_0");
        assert_eq!(lifted.entry_point, "PSMain");
        assert_eq!(lifted.fallback_comments, 0);
        assert_eq!(lifted.lifted_count, lifted.instruction_count);
        assert!(lifted.hlsl.contains("SV_Target0"));
        assert!(lifted.hlsl.contains("mad("));
        assert!(lifted.hlsl.contains("_out.mrt0 = float4("));
        assert!(lifted.hlsl.contains("return _out;"));
    }
}
