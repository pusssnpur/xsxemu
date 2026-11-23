//! Xbox Series X native RDNA 2 shader container unpacking (`DXBC`/`DXIL`/`XDAT`/`XBCH`/`FSSM` & `FSPX`).
//!
//! Real Xbox Series X games ship precompiled RDNA 2 (`gfx1013`) shaders inside two formats:
//! 1. **FSSM**: A `DXBC` container whose `DXIL` part holds an `XDAT` table whose `0x0D` entry
//!    is an `XBCH` header followed by a zlib-compressed stream inflating to an `FSSM` RDNA 2 program.
//! 2. **FSPX**: Used by `amd_fidelityfx_upscaler_xbox.dll` (and Xbox FidelityFX compute shaders),
//!    carrying a directory of typed records where kind `0` is dispatch state and kind `6` is the
//!    raw RDNA 2 compute shader machine code.
//!
//! This module implements a zero-dependency RFC 1950/1951 zlib inflater and FIPS 180-4 SHA-256
//! hasher so `xsxemu` can unpack, hash, cache, and inspect real Xbox Series X shader binaries
//! offline with zero external crate dependencies.

use crate::shader::ShaderContainer;

// ---------------------------------------------------------------------------
// Zero-dependency RFC 1950/1951 Zlib Inflate
// ---------------------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    bits_in_buf: u32,
    failed: bool,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            buf: 0,
            bits_in_buf: 0,
            failed: false,
        }
    }

    fn read_bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        while self.bits_in_buf < n {
            if self.pos >= self.data.len() {
                self.failed = true;
                return 0;
            }
            self.buf |= (self.data[self.pos] as u64) << self.bits_in_buf;
            self.pos += 1;
            self.bits_in_buf += 8;
        }
        let val = (self.buf & ((1u64 << n) - 1)) as u32;
        self.buf >>= n;
        self.bits_in_buf -= n;
        val
    }

    fn align_byte(&mut self) {
        let discard = self.bits_in_buf % 8;
        if discard > 0 {
            self.buf >>= discard;
            self.bits_in_buf -= discard;
        }
    }

    fn read_byte(&mut self) -> Option<u8> {
        if self.bits_in_buf >= 8 {
            let b = (self.buf & 0xFF) as u8;
            self.buf >>= 8;
            self.bits_in_buf -= 8;
            return Some(b);
        }
        if self.pos >= self.data.len() {
            self.failed = true;
            return None;
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Some(b)
    }

    fn byte_position(&self) -> usize {
        self.pos - (self.bits_in_buf / 8) as usize
    }
}

struct Huffman {
    count: [i16; 16],
    symbol: [i16; 288],
}

impl Huffman {
    fn build(lengths: &[i16]) -> Option<Self> {
        let mut h = Self {
            count: [0; 16],
            symbol: [0; 288],
        };
        for &len in lengths {
            if !(0..16).contains(&len) {
                return None;
            }
            h.count[len as usize] += 1;
        }
        if h.count[0] as usize == lengths.len() {
            return Some(h);
        }
        let mut left = 1i32;
        for len in 1..16 {
            left <<= 1;
            left -= h.count[len] as i32;
            if left < 0 {
                return None;
            }
        }
        let mut offsets = [0i16; 16];
        for len in 1..15 {
            offsets[len + 1] = offsets[len] + h.count[len];
        }
        for (i, &len) in lengths.iter().enumerate() {
            if len != 0 {
                let idx = offsets[len as usize] as usize;
                h.symbol[idx] = i as i16;
                offsets[len as usize] += 1;
            }
        }
        Some(h)
    }

    fn decode(&self, reader: &mut BitReader<'_>) -> Option<i32> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..16 {
            code |= reader.read_bits(1) as i32;
            let cnt = self.count[len] as i32;
            if code - cnt < first {
                return Some(self.symbol[(index + (code - first)) as usize] as i32);
            }
            index += cnt;
            first = (first + cnt) << 1;
            code <<= 1;
            if reader.failed {
                return None;
            }
        }
        None
    }
}

fn inflate_codes(
    reader: &mut BitReader<'_>,
    out: &mut Vec<u8>,
    lit_len: &Huffman,
    dist: &Huffman,
    limit: usize,
) -> bool {
    const LENGTH_BASE: [u16; 29] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
        131, 163, 195, 227, 258,
    ];
    const LENGTH_EXTRA: [u32; 29] = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
    ];
    const DIST_BASE: [u16; 30] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
        2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
    ];
    const DIST_EXTRA: [u32; 30] = [
        0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
        13, 13,
    ];

    loop {
        let Some(sym) = lit_len.decode(reader) else {
            return false;
        };
        if reader.failed {
            return false;
        }
        if sym < 256 {
            if out.len() >= limit {
                return false;
            }
            out.push(sym as u8);
        } else if sym == 256 {
            return true;
        } else {
            let idx = (sym - 257) as usize;
            if idx >= 29 {
                return false;
            }
            let length = LENGTH_BASE[idx] as usize + reader.read_bits(LENGTH_EXTRA[idx]) as usize;
            let Some(d_sym) = dist.decode(reader) else {
                return false;
            };
            let d_idx = d_sym as usize;
            if d_idx >= 30 {
                return false;
            }
            let distance =
                DIST_BASE[d_idx] as usize + reader.read_bits(DIST_EXTRA[d_idx]) as usize;
            if reader.failed || distance == 0 || distance > out.len() || out.len() + length > limit
            {
                return false;
            }
            let from = out.len() - distance;
            for i in 0..length {
                let b = out[from + i];
                out.push(b);
            }
        }
    }
}

/// Inflate an RFC 1950 zlib stream (2-byte header + deflate blocks + big-endian Adler-32).
pub fn zlib_inflate(input: &[u8], max_output: usize) -> Option<Vec<u8>> {
    if input.len() < 6 {
        return None;
    }
    let cmf = input[0] as u32;
    let flg = input[1] as u32;
    if (cmf & 0x0F) != 8 || (cmf >> 4) > 7 || ((cmf << 8) | flg) % 31 != 0 || (flg & 0x20) != 0 {
        return None;
    }

    let mut out = Vec::new();
    let mut reader = BitReader::new(&input[2..]);

    loop {
        let is_last = reader.read_bits(1) != 0;
        let btype = reader.read_bits(2);
        if reader.failed {
            return None;
        }
        match btype {
            0 => {
                // Uncompressed stored block
                reader.align_byte();
                let b0 = reader.read_byte()? as u16;
                let b1 = reader.read_byte()? as u16;
                let b2 = reader.read_byte()? as u16;
                let b3 = reader.read_byte()? as u16;
                let len = b0 | (b1 << 8);
                let nlen = b2 | (b3 << 8);
                if len != !nlen || out.len() + len as usize > max_output {
                    return None;
                }
                for _ in 0..len {
                    out.push(reader.read_byte()?);
                }
            }
            1 => {
                // Fixed Huffman
                let mut lit = [0i16; 288];
                for x in &mut lit[0..144] {
                    *x = 8;
                }
                for x in &mut lit[144..256] {
                    *x = 9;
                }
                for x in &mut lit[256..280] {
                    *x = 7;
                }
                for x in &mut lit[280..288] {
                    *x = 8;
                }
                let lit_h = Huffman::build(&lit)?;
                let dist_h = Huffman::build(&[5i16; 30])?;
                if !inflate_codes(&mut reader, &mut out, &lit_h, &dist_h, max_output) {
                    return None;
                }
            }
            2 => {
                // Dynamic Huffman
                let hlit = reader.read_bits(5) as usize + 257;
                let hdist = reader.read_bits(5) as usize + 1;
                let hclen = reader.read_bits(4) as usize + 4;
                if hlit > 286 || hdist > 30 {
                    return None;
                }
                const ORDER: [usize; 19] = [
                    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
                ];
                let mut lengths = [0i16; 320];
                for i in 0..hclen {
                    lengths[ORDER[i]] = reader.read_bits(3) as i16;
                }
                let code_h = Huffman::build(&lengths[..19])?;
                let total = hlit + hdist;
                let mut i = 0;
                while i < total {
                    let sym = code_h.decode(&mut reader)?;
                    if sym < 16 {
                        lengths[i] = sym as i16;
                        i += 1;
                    } else {
                        let (val, repeat) = match sym {
                            16 => {
                                if i == 0 {
                                    return None;
                                }
                                (lengths[i - 1], 3 + reader.read_bits(2) as usize)
                            }
                            17 => (0, 3 + reader.read_bits(3) as usize),
                            18 => (0, 11 + reader.read_bits(7) as usize),
                            _ => return None,
                        };
                        if i + repeat > total {
                            return None;
                        }
                        for _ in 0..repeat {
                            lengths[i] = val;
                            i += 1;
                        }
                    }
                }
                if lengths[256] == 0 {
                    return None;
                }
                let lit_h = Huffman::build(&lengths[..hlit])?;
                let dist_h = Huffman::build(&lengths[hlit..hlit + hdist])?;
                if !inflate_codes(&mut reader, &mut out, &lit_h, &dist_h, max_output) {
                    return None;
                }
            }
            _ => return None,
        }
        if reader.failed {
            return None;
        }
        if is_last {
            break;
        }
    }

    reader.align_byte();
    let adler_off = 2 + reader.byte_position();
    if adler_off + 4 > input.len() {
        return None;
    }
    let expected = u32::from_be_bytes(input[adler_off..adler_off + 4].try_into().ok()?);
    if adler32(&out) != expected {
        return None;
    }
    Some(out)
}

/// Compute RFC 1950 Adler-32 checksum.
pub fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// Wrap `data` in a minimal valid RFC 1950 uncompressed zlib stream (handy for tests & packaging).
pub fn zlib_store(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    out.push(0x78); // CMF: deflate, 32K window
    out.push(0x01); // FLG: no dict, check bits valid (0x7801 % 31 == 0)
    let mut remaining = data;
    loop {
        let chunk_len = remaining.len().min(65535);
        let is_last = chunk_len == remaining.len();
        out.push(if is_last { 0x01 } else { 0x00 });
        let len_u16 = chunk_len as u16;
        out.extend_from_slice(&len_u16.to_le_bytes());
        out.extend_from_slice(&(!len_u16).to_le_bytes());
        out.extend_from_slice(&remaining[..chunk_len]);
        remaining = &remaining[chunk_len..];
        if is_last {
            break;
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

// ---------------------------------------------------------------------------
// Zero-dependency FIPS 180-4 SHA-256
// ---------------------------------------------------------------------------

pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = Vec::with_capacity(data.len() + 72);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(block[i * 4..(i + 1) * 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, w) in h.iter().enumerate() {
        out[i * 4..(i + 1) * 4].copy_from_slice(&w.to_be_bytes());
    }
    out
}

pub fn sha256_hex(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = sha256(data);
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xF) as usize] as char);
    }
    s
}

// ---------------------------------------------------------------------------
// Xbox Series X FSSM (DXBC -> DXIL -> XDAT -> 0x0D XBCH -> zlib -> FSSM)
// ---------------------------------------------------------------------------

/// Extract the native RDNA 2 `FSSM` program from either a bare `FSSM` blob or an
/// Xbox Series X `DXBC` container containing `DXIL` -> `XDAT` -> tag `0x0D` (`XBCH` + zlib).
///
/// When multiple `0x0D` entries exist, the last one wins (matching real Xbox Series X
/// shader corpora).
pub fn extract_fssm_program(bytecode: &[u8]) -> Option<Vec<u8>> {
    if bytecode.len() >= 4 && &bytecode[0..4] == b"FSSM" {
        return Some(bytecode.to_vec());
    }
    let container = ShaderContainer::parse(bytecode).ok()?;
    let dxil = container.chunks.iter().find(|c| &c.fourcc == b"DXIL")?.data.as_slice();

    // Locate "XDAT" marker inside DXIL chunk
    let xdat = dxil.windows(4).position(|w| w == b"XDAT")?;
    if xdat + 12 > dxil.len() {
        return None;
    }
    let count = u32::from_le_bytes(dxil[xdat + 4..xdat + 8].try_into().ok()?) as usize;
    let mut chosen: Option<&[u8]> = None;
    for i in 0..count {
        let entry = xdat + 12 + i * 12;
        if entry + 12 > dxil.len() {
            return None;
        }
        let tag = u32::from_le_bytes(dxil[entry..entry + 4].try_into().ok()?);
        let offset = u32::from_le_bytes(dxil[entry + 4..entry + 8].try_into().ok()?) as usize;
        let size = u32::from_le_bytes(dxil[entry + 8..entry + 12].try_into().ok()?) as usize;
        if tag != 0x0D {
            continue;
        }
        if offset.checked_add(size)? > dxil.len() || size < 24 {
            return None;
        }
        chosen = Some(&dxil[offset..offset + size]);
    }
    let xbch = chosen?;
    if &xbch[0..4] != b"XBCH" {
        return None;
    }
    let unpacked_size = u32::from_le_bytes(xbch[16..20].try_into().ok()?) as usize;
    let inflated = zlib_inflate(&xbch[24..], unpacked_size)?;
    if inflated.len() != unpacked_size {
        return None;
    }
    Some(inflated)
}

// ---------------------------------------------------------------------------
// Xbox FidelityFX FSPX Payload Unpacker (Kind 0 dispatch state + Kind 6 RDNA 2 code)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct FspxParts<'a> {
    pub dispatch_state: &'a [u8],
    pub code: &'a [u8],
}

/// Parse an `FSPX` compute shader container (as used by `amd_fidelityfx_upscaler_xbox.dll`).
/// Returns the kind-0 dispatch state and kind-6 RDNA 2 machine code slices.
pub fn extract_fspx_parts(payload: &[u8]) -> Option<FspxParts<'_>> {
    if payload.len() < 16 || &payload[0..4] != b"FSPX" {
        return None;
    }
    let entry_count = u32::from_le_bytes(payload[8..12].try_into().ok()?) as u64;
    let declared_total = u32::from_le_bytes(payload[12..16].try_into().ok()?) as u64;
    let total = declared_total.min(payload.len() as u64);
    let end_dir = 16u64.checked_add(entry_count.checked_mul(16)?)?;
    if total < end_dir {
        return None;
    }

    let mut state: Option<&[u8]> = None;
    let mut code: Option<&[u8]> = None;
    let mut at = 16usize;
    while (at as u64) < end_dir {
        let kind = u32::from_le_bytes(payload[at..at + 4].try_into().ok()?);
        let offset = u32::from_le_bytes(payload[at + 4..at + 8].try_into().ok()?) as u64;
        let size = u32::from_le_bytes(payload[at + 8..at + 12].try_into().ok()?) as u64;
        at += 16;
        if offset < end_dir || offset.saturating_add(size) > total || size == 0 {
            continue;
        }
        let slice = &payload[offset as usize..(offset + size) as usize];
        match kind {
            0 => state = Some(slice),
            6 => code = Some(slice),
            _ => {}
        }
    }

    Some(FspxParts {
        dispatch_state: state?,
        code: code?,
    })
}

/// Detect whether `code` has the 36-byte RDNA 2 FidelityFX bare kind-6 prologue
/// (three VOP3 words with opcode `0x180`, upper 16 bits = `0xD580`, at dwords 0, 3, and 6).
pub fn is_fspx_bare_program(code: &[u8]) -> bool {
    if code.len() < 36 || code.len() % 4 != 0 {
        return false;
    }
    for word_idx in [0usize, 3, 6] {
        let w = u32::from_le_bytes(code[word_idx * 4..word_idx * 4 + 4].try_into().unwrap());
        if (w >> 16) != 0xD580 {
            return false;
        }
    }
    true
}

/// High-level inspection summary for any Xbox Series X shader blob.
#[derive(Debug, Clone)]
pub struct XboxShaderInspection {
    pub kind: &'static str,
    pub total_bytes: usize,
    pub payload_sha256: String,
    pub extracted_rdna2_bytes: usize,
    pub chunks: Vec<String>,
}

/// Inspect a raw shader file or memory buffer (`DXBC`/`FSSM`/`FSPX`/bare RDNA 2).
pub fn inspect_xbox_shader_blob(bytes: &[u8]) -> XboxShaderInspection {
    if let Some(parts) = extract_fspx_parts(bytes) {
        return XboxShaderInspection {
            kind: "FSPX (FidelityFX RDNA2 compute)",
            total_bytes: bytes.len(),
            payload_sha256: sha256_hex(parts.code),
            extracted_rdna2_bytes: parts.code.len(),
            chunks: vec![
                format!("kind0({}B)", parts.dispatch_state.len()),
                format!("kind6({}B)", parts.code.len()),
            ],
        };
    }
    if let Some(fssm) = extract_fssm_program(bytes) {
        let chunks = ShaderContainer::parse(bytes)
            .map(|c| c.chunks.iter().map(|ch| ch.fourcc_str()).collect())
            .unwrap_or_else(|_| vec!["FSSM".into()]);
        return XboxShaderInspection {
            kind: "DXBC/FSSM (Xbox Series X RDNA2)",
            total_bytes: bytes.len(),
            payload_sha256: sha256_hex(&fssm),
            extracted_rdna2_bytes: fssm.len(),
            chunks,
        };
    }
    if is_fspx_bare_program(bytes) {
        return XboxShaderInspection {
            kind: "FSPX bare kind-6 RDNA2",
            total_bytes: bytes.len(),
            payload_sha256: sha256_hex(bytes),
            extracted_rdna2_bytes: bytes.len(),
            chunks: vec!["kind6-bare".into()],
        };
    }
    if let Ok(c) = ShaderContainer::parse(bytes) {
        return XboxShaderInspection {
            kind: "DXBC/DXIL standard",
            total_bytes: bytes.len(),
            payload_sha256: sha256_hex(bytes),
            extracted_rdna2_bytes: 0,
            chunks: c.chunks.iter().map(|ch| ch.fourcc_str()).collect(),
        };
    }
    XboxShaderInspection {
        kind: "Unknown binary",
        total_bytes: bytes.len(),
        payload_sha256: sha256_hex(bytes),
        extracted_rdna2_bytes: 0,
        chunks: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_nist_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn extracts_fssm_from_dxbc_xdat_xbch_zlib() {
        let fssm_payload = b"FSSM\x01\x00\x00\x00RDNA2_SHADER_CODE_TEST_PAYLOAD";
        let compressed = zlib_store(fssm_payload);

        // Build XBCH header (24 bytes) + zlib stream
        let mut xbch = vec![0u8; 24];
        xbch[0..4].copy_from_slice(b"XBCH");
        xbch[16..20].copy_from_slice(&(fssm_payload.len() as u32).to_le_bytes());
        xbch.extend_from_slice(&compressed);

        // Build DXIL chunk containing XDAT table with 1 entry (tag 0x0D)
        let mut dxil = vec![0u8; 16]; // leading DXIL prefix
        let xdat_off = dxil.len();
        let entry_offset = (xdat_off + 12 + 12) as u32;
        dxil.extend_from_slice(b"XDAT");
        dxil.extend_from_slice(&1u32.to_le_bytes()); // count = 1
        dxil.extend_from_slice(&0u32.to_le_bytes()); // reserved
        dxil.extend_from_slice(&0x0Du32.to_le_bytes()); // tag = 0x0D
        dxil.extend_from_slice(&entry_offset.to_le_bytes());
        dxil.extend_from_slice(&(xbch.len() as u32).to_le_bytes());
        dxil.extend_from_slice(&xbch);

        let dxbc = ShaderContainer::build(&[(*b"DXIL", &dxil)]);
        let extracted = extract_fssm_program(&dxbc).expect("should unpack FSSM");
        assert_eq!(extracted, fssm_payload);

        let info = inspect_xbox_shader_blob(&dxbc);
        assert_eq!(info.extracted_rdna2_bytes, fssm_payload.len());
        assert_eq!(info.payload_sha256, sha256_hex(fssm_payload));
    }

    #[test]
    fn unpacks_fspx_container_and_detects_bare_prologue() {
        let state = [0x11u8; 28];
        let mut code = vec![0u8; 40];
        for idx in [0usize, 3, 6] {
            code[idx * 4..idx * 4 + 4].copy_from_slice(&0xD580_0001u32.to_le_bytes());
        }
        assert!(is_fspx_bare_program(&code));

        // Header (16B) + 2 directory entries (32B) = 48B
        let state_off = 48u32;
        let code_off = state_off + state.len() as u32;
        let total = code_off + code.len() as u32;

        let mut fspx = Vec::new();
        fspx.extend_from_slice(b"FSPX");
        fspx.extend_from_slice(&1u32.to_le_bytes()); // version
        fspx.extend_from_slice(&2u32.to_le_bytes()); // 2 entries
        fspx.extend_from_slice(&total.to_le_bytes());
        // Entry 0: kind 0
        fspx.extend_from_slice(&0u32.to_le_bytes());
        fspx.extend_from_slice(&state_off.to_le_bytes());
        fspx.extend_from_slice(&(state.len() as u32).to_le_bytes());
        fspx.extend_from_slice(&0u32.to_le_bytes());
        // Entry 1: kind 6
        fspx.extend_from_slice(&6u32.to_le_bytes());
        fspx.extend_from_slice(&code_off.to_le_bytes());
        fspx.extend_from_slice(&(code.len() as u32).to_le_bytes());
        fspx.extend_from_slice(&0u32.to_le_bytes());
        fspx.extend_from_slice(&state);
        fspx.extend_from_slice(&code);

        let parts = extract_fspx_parts(&fspx).expect("should parse FSPX");
        assert_eq!(parts.dispatch_state, &state);
        assert_eq!(parts.code, code.as_slice());
    }
}
