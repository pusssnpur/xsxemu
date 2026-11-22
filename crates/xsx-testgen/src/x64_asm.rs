//! Minimal x86_64 machine-code assembler for generating Win64 ABI test executables.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Reg64 {
    Rax = 0,
    Rcx = 1,
    Rdx = 2,
    Rbx = 3,
    Rsp = 4,
    Rbp = 5,
    Rsi = 6,
    Rdi = 7,
    R8 = 8,
    R9 = 9,
    R10 = 10,
    R11 = 11,
}

pub struct X64Assembler {
    text_rva: u32,
    code: Vec<u8>,
    patches: Vec<(usize, u32)>, // (offset of disp32 in code, target_rva)
}

impl X64Assembler {
    pub fn new(text_rva: u32) -> Self {
        Self {
            text_rva,
            code: Vec::new(),
            patches: Vec::new(),
        }
    }

    pub fn current_rva(&self) -> u32 {
        self.text_rva + self.code.len() as u32
    }

    pub fn push(&mut self, reg: Reg64) {
        let r = reg as u8;
        if r >= 8 {
            self.code.push(0x41);
        }
        self.code.push(0x50 + (r & 7));
    }

    pub fn pop(&mut self, reg: Reg64) {
        let r = reg as u8;
        if r >= 8 {
            self.code.push(0x41);
        }
        self.code.push(0x58 + (r & 7));
    }

    pub fn ret(&mut self) {
        self.code.push(0xC3);
    }

    pub fn sub_rsp_imm8(&mut self, imm: u8) {
        self.code.extend_from_slice(&[0x48, 0x83, 0xEC, imm]);
    }

    pub fn add_rsp_imm8(&mut self, imm: u8) {
        self.code.extend_from_slice(&[0x48, 0x83, 0xC4, imm]);
    }

    pub fn mov_reg_imm32(&mut self, reg: Reg64, imm: u32) {
        let r = reg as u8;
        if r >= 8 {
            self.code.push(0x41);
        }
        self.code.push(0xB8 + (r & 7));
        self.code.extend_from_slice(&imm.to_le_bytes());
    }

    pub fn xor_reg32_self(&mut self, reg: Reg64) {
        let r = reg as u8;
        if r >= 8 {
            self.code.push(0x45);
        }
        self.code.push(0x31);
        let low = r & 7;
        self.code.push(0xC0 | (low << 3) | low);
    }

    /// `lea reg, [rip + disp32]` targeting `target_rva`.
    pub fn lea_rip_rva(&mut self, reg: Reg64, target_rva: u32) {
        let r = reg as u8;
        let rex = 0x48 | ((r >> 3) << 2);
        self.code.push(rex);
        self.code.push(0x8D);
        self.code.push(0x05 | ((r & 7) << 3));
        self.emit_rip_disp32(target_rva);
    }

    /// `mov reg, qword ptr [rip + disp32]` loading 64-bit value from `target_rva`.
    pub fn mov_reg_from_rva(&mut self, reg: Reg64, target_rva: u32) {
        let r = reg as u8;
        let rex = 0x48 | ((r >> 3) << 2);
        self.code.push(rex);
        self.code.push(0x8B);
        self.code.push(0x05 | ((r & 7) << 3));
        self.emit_rip_disp32(target_rva);
    }

    /// `mov qword ptr [rip + disp32], reg` storing 64-bit value to `target_rva`.
    pub fn mov_rva_from_reg(&mut self, target_rva: u32, reg: Reg64) {
        let r = reg as u8;
        let rex = 0x48 | ((r >> 3) << 2);
        self.code.push(rex);
        self.code.push(0x89);
        self.code.push(0x05 | ((r & 7) << 3));
        self.emit_rip_disp32(target_rva);
    }

    /// `mov qword ptr [rsp + disp8], reg` for Win64 stack arguments (5th+ arg).
    pub fn mov_rsp_off_reg(&mut self, disp8: u8, reg: Reg64) {
        let r = reg as u8;
        let rex = 0x48 | ((r >> 3) << 2);
        self.code.push(rex);
        self.code.push(0x89);
        self.code.push(0x44 | ((r & 7) << 3));
        self.code.push(0x24);
        self.code.push(disp8);
    }

    /// `mov qword ptr [rsp + disp8], imm32` (sign-extended to 64-bit).
    pub fn mov_rsp_off_imm32(&mut self, disp8: u8, imm: i32) {
        self.code.extend_from_slice(&[0x48, 0xC7, 0x44, 0x24, disp8]);
        self.code.extend_from_slice(&imm.to_le_bytes());
    }

    /// `call qword ptr [rip + disp32]` targeting an IAT slot at `iat_rva`.
    pub fn call_iat(&mut self, iat_rva: u32) {
        self.code.extend_from_slice(&[0xFF, 0x15]);
        self.emit_rip_disp32(iat_rva);
    }

    /// Call COM vtable slot `slot_index` on the interface pointer currently in `RCX`:
    /// `mov rax, [rcx]; call qword ptr [rax + slot_index*8]`.
    pub fn call_com_rcx_slot(&mut self, slot_index: u32) {
        self.code.extend_from_slice(&[0x48, 0x8B, 0x01]);
        let byte_off = slot_index * 8;
        if byte_off == 0 {
            self.code.extend_from_slice(&[0xFF, 0x10]);
        } else if byte_off <= 127 {
            self.code.extend_from_slice(&[0xFF, 0x50, byte_off as u8]);
        } else {
            self.code.extend_from_slice(&[0xFF, 0x90]);
            self.code.extend_from_slice(&(byte_off as i32).to_le_bytes());
        }
    }

    /// `cmp eax, imm32`
    pub fn cmp_eax_imm32(&mut self, imm: u32) {
        self.code.push(0x3D);
        self.code.extend_from_slice(&imm.to_le_bytes());
    }

    /// `test eax, eax`
    pub fn test_eax_eax(&mut self) {
        self.code.extend_from_slice(&[0x85, 0xC0]);
    }

    /// Emit `js rel32` returning the patch site offset for `patch_branch_here`.
    pub fn js_placeholder(&mut self) -> usize {
        self.code.extend_from_slice(&[0x0F, 0x88, 0, 0, 0, 0]);
        self.code.len() - 4
    }

    /// Emit `jne rel32` returning the patch site offset for `patch_branch_here`.
    pub fn jne_placeholder(&mut self) -> usize {
        self.code.extend_from_slice(&[0x0F, 0x85, 0, 0, 0, 0]);
        self.code.len() - 4
    }

    /// Patch a previously emitted conditional branch to jump to the current code position.
    pub fn patch_branch_here(&mut self, patch_off: usize) {
        let next_ip = patch_off + 4;
        let rel = (self.code.len() as isize - next_ip as isize) as i32;
        self.code[patch_off..patch_off + 4].copy_from_slice(&rel.to_le_bytes());
    }

    fn emit_rip_disp32(&mut self, target_rva: u32) {
        let off = self.code.len();
        self.code.extend_from_slice(&[0, 0, 0, 0]);
        self.patches.push((off, target_rva));
    }

    pub fn finish(mut self) -> Vec<u8> {
        for (off, target_rva) in self.patches {
            let next_ip_rva = self.text_rva + (off as u32) + 4;
            let disp = (target_rva as i64 - next_ip_rva as i64) as i32;
            self.code[off..off + 4].copy_from_slice(&disp.to_le_bytes());
        }
        self.code
    }
}
