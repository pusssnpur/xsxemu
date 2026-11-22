//! `acphal.dll` / `xapu.dll` — Xbox Audio Coprocessor HAL & SHAPE/APU Heap HLE.
//!
//! Real Xbox Series X engines (XDefiant, Concept Destruction, Wwise/FMOD Scarlett backends)
//! call `ApuCreateHeap`, `ApuAlloc`, `AcpHalAllocateShapeContexts`, and `AcpHalCreate` during
//! audio boot. Failing these calls causes titles to disable their entire audio subsystem.
//!
//! Implements all 10 verified console exports plus the 8-slot `IAcpHal` COM channel:
//! - `ApuCreateHeap`, `ApuAlloc`, `ApuFree`, `ApuMapVirtualAddress`, `ApuMapApuAddress`,
//!   `ApuIsVirtualAddressValid`, `ApuHeapGetState`
//! - `AcpHalAllocateShapeContexts`, `AcpHalReleaseShapeContexts`, `AcpHalCreate`
//! - `IAcpHal`: `Connect`, `Disconnect`, `SubmitCommand`, `PopMessage` (queues command-done
//!   & 128-sample/48kHz audio frame ticks + `0x80` disconnect), `Release`.

use core::ffi::c_void;
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use xsx_abi::com::{HResult, E_INVALIDARG, E_OUTOFMEMORY, S_OK};

use crate::registry::ModuleRegistry;
use crate::runtime::current_runtime;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ApuHeapState {
    pub bytes_free: u32,
    pub bytes_allocated: u32,
    pub bytes_lost: u32,
    pub maximum_block_size_available: u32,
    pub allocation_count: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AcpShapeContextCounts {
    pub num_src_contexts: u32,      // +0x00 (stride 32B)
    pub num_eq_comp_contexts: u32,  // +0x04 (stride 64B)
    pub num_filt_vol_contexts: u32, // +0x08 (stride 64B)
    pub num_dma_contexts: u32,      // +0x0C (stride 16B)
    pub num_xma_contexts: u32,      // +0x10 (stride 64B)
    pub num_pcm_contexts: u32,      // +0x14 (stride 64B)
    pub src_contexts: *mut c_void,  // +0x18
    pub eq_comp_contexts: *mut c_void,
    pub filt_vol_contexts: *mut c_void,
    pub dma_contexts: *mut c_void,
    pub xma_contexts: *mut c_void,
    pub pcm_contexts: *mut c_void,
}

impl Default for AcpShapeContextCounts {
    fn default() -> Self {
        Self {
            num_src_contexts: 0,
            num_eq_comp_contexts: 0,
            num_filt_vol_contexts: 0,
            num_dma_contexts: 0,
            num_xma_contexts: 0,
            num_pcm_contexts: 0,
            src_contexts: core::ptr::null_mut(),
            eq_comp_contexts: core::ptr::null_mut(),
            filt_vol_contexts: core::ptr::null_mut(),
            dma_contexts: core::ptr::null_mut(),
            xma_contexts: core::ptr::null_mut(),
            pcm_contexts: core::ptr::null_mut(),
        }
    }
}

struct ApuAllocation {
    va: usize,
    pa: u32,
    size: u32,
    align: usize,
}

struct ShapeBuf {
    ptr: usize,
    layout: Layout,
}

struct ApuGlobalState {
    next_pa: u32,
    heap_capacity: u32,
    bytes_allocated: u32,
    by_va: HashMap<usize, ApuAllocation>,
    by_pa: HashMap<u32, usize>,
    shape_bufs: Vec<ShapeBuf>,
}

impl Default for ApuGlobalState {
    fn default() -> Self {
        Self {
            next_pa: 0x1000_0000,
            heap_capacity: 64 * 1024 * 1024,
            bytes_allocated: 0,
            by_va: HashMap::new(),
            by_pa: HashMap::new(),
            shape_bufs: Vec::new(),
        }
    }
}

fn apu_state() -> &'static Mutex<ApuGlobalState> {
    static S: OnceLock<Mutex<ApuGlobalState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(ApuGlobalState::default()))
}

pub fn register(reg: &mut ModuleRegistry) {
    for (name, addr) in [
        ("AcpHalAllocateShapeContexts", hle_acphal_allocate_shape_contexts as *const () as usize),
        ("AcpHalCreate", hle_acphal_create as *const () as usize),
        ("AcpHalReleaseShapeContexts", hle_acphal_release_shape_contexts as *const () as usize),
        ("ApuCreateHeap", hle_apu_create_heap as *const () as usize),
        ("ApuAlloc", hle_apu_alloc as *const () as usize),
        ("ApuFree", hle_apu_free as *const () as usize),
        ("ApuHeapGetState", hle_apu_heap_get_state as *const () as usize),
        ("ApuIsVirtualAddressValid", hle_apu_is_virtual_address_valid as *const () as usize),
        ("ApuMapApuAddress", hle_apu_map_apu_address as *const () as usize),
        ("ApuMapVirtualAddress", hle_apu_map_virtual_address as *const () as usize),
    ] {
        reg.register_export("acphal.dll", name, addr);
    }
}

// ---------------------------------------------------------------------------
// IAcpHal 8-slot non-IUnknown COM interface
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AcpHalMessage {
    pub msg_type: u32, // 0x01 = AUDIO_FRAME, 0x02 = COMMAND_DONE, 0x80 = DISCONNECTED
    pub id: u32,
    pub param0: u32,
    pub param1: u32,
    pub timestamp_samples: u64,
}

#[repr(C)]
struct IAcpHalVtbl {
    connect: unsafe extern "win64" fn(this: *mut c_void, cmd_len: u32, msg_len: u32) -> HResult,
    disconnect: unsafe extern "win64" fn(this: *mut c_void) -> HResult,
    submit_command: unsafe extern "win64" fn(
        this: *mut c_void,
        cmd_type: u32,
        id: u32,
        flags: u32,
        data: *const c_void,
        extra: u32,
    ) -> HResult,
    pop_message: unsafe extern "win64" fn(this: *mut c_void, msg: *mut AcpHalMessage) -> u8,
    get_num_messages: unsafe extern "win64" fn(this: *mut c_void) -> u32,
    release: unsafe extern "win64" fn(this: *mut c_void) -> u32,
    stub_slot6: unsafe extern "win64" fn(this: *mut c_void) -> HResult,
    stub_slot7: unsafe extern "win64" fn(this: *mut c_void) -> HResult,
}

#[repr(C)]
struct AcpHalObj {
    lp_vtbl: *const IAcpHalVtbl,
    ref_count: AtomicU32,
    state: Mutex<AcpChannelState>,
}

#[derive(Default)]
struct AcpChannelState {
    connected: bool,
    sample_clock: u64,
    queue: VecDeque<AcpHalMessage>,
}

static ACP_HAL_VTBL: IAcpHalVtbl = IAcpHalVtbl {
    connect: acp_connect,
    disconnect: acp_disconnect,
    submit_command: acp_submit_command,
    pop_message: acp_pop_message,
    get_num_messages: acp_get_num_messages,
    release: acp_release,
    stub_slot6: acp_stub_ok,
    stub_slot7: acp_stub_ok,
};

unsafe extern "win64" fn acp_connect(this: *mut c_void, cmd_len: u32, msg_len: u32) -> HResult {
    if this.is_null() {
        return E_INVALIDARG;
    }
    let obj = &*(this as *const AcpHalObj);
    let mut st = obj.state.lock().unwrap();
    st.connected = true;
    st.queue.clear();
    // Emit initial 128-sample / 48 kHz audio frame notification
    st.sample_clock += 128;
    let ts = st.sample_clock;
    st.queue.push_back(AcpHalMessage {
        msg_type: 0x01, // MSG_AUDIO_FRAME
        id: 0,
        param0: 128,
        param1: 48_000,
        timestamp_samples: ts,
    });
    if let Some(rt) = current_runtime() {
        rt.trace(format!("acphal!IAcpHal::Connect(cmd={cmd_len}, msg={msg_len}) -> S_OK"));
    }
    S_OK
}

unsafe extern "win64" fn acp_disconnect(this: *mut c_void) -> HResult {
    if this.is_null() {
        return E_INVALIDARG;
    }
    let obj = &*(this as *const AcpHalObj);
    let mut st = obj.state.lock().unwrap();
    st.connected = false;
    let ts = st.sample_clock;
    st.queue.push_back(AcpHalMessage {
        msg_type: 0x80, // Verified Xbox disconnect sentinel
        id: 0,
        param0: 0,
        param1: 0,
        timestamp_samples: ts,
    });
    S_OK
}

unsafe extern "win64" fn acp_submit_command(
    this: *mut c_void,
    cmd_type: u32,
    id: u32,
    _flags: u32,
    _data: *const c_void,
    _extra: u32,
) -> HResult {
    if this.is_null() {
        return E_INVALIDARG;
    }
    let obj = &*(this as *const AcpHalObj);
    let mut st = obj.state.lock().unwrap();
    st.sample_clock += 128;
    let ts = st.sample_clock;
    st.queue.push_back(AcpHalMessage {
        msg_type: 0x02, // MSG_COMMAND_DONE
        id,
        param0: cmd_type,
        param1: 0,
        timestamp_samples: ts,
    });
    S_OK
}

unsafe extern "win64" fn acp_pop_message(this: *mut c_void, msg: *mut AcpHalMessage) -> u8 {
    if this.is_null() {
        return 0;
    }
    let obj = &*(this as *const AcpHalObj);
    let mut st = obj.state.lock().unwrap();
    if let Some(m) = st.queue.pop_front() {
        if !msg.is_null() {
            *msg = m;
        }
        1
    } else {
        0
    }
}

unsafe extern "win64" fn acp_get_num_messages(this: *mut c_void) -> u32 {
    if this.is_null() {
        return 0;
    }
    let obj = &*(this as *const AcpHalObj);
    obj.state.lock().unwrap().queue.len() as u32
}

unsafe extern "win64" fn acp_release(this: *mut c_void) -> u32 {
    if this.is_null() {
        return 0;
    }
    let obj = &*(this as *const AcpHalObj);
    let rem = obj.ref_count.fetch_sub(1, Ordering::AcqRel) - 1;
    if rem == 0 {
        drop(Box::from_raw(this as *mut AcpHalObj));
    }
    rem
}

unsafe extern "win64" fn acp_stub_ok(_this: *mut c_void) -> HResult {
    S_OK
}

// ---------------------------------------------------------------------------
// Exported Win64 ABI thunks
// ---------------------------------------------------------------------------

pub unsafe extern "win64" fn hle_acphal_create(out: *mut *mut c_void) -> HResult {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let obj = Box::new(AcpHalObj {
        lp_vtbl: &ACP_HAL_VTBL,
        ref_count: AtomicU32::new(1),
        state: Mutex::new(AcpChannelState::default()),
    });
    *out = Box::into_raw(obj) as *mut c_void;
    if let Some(rt) = current_runtime() {
        rt.trace("acphal!AcpHalCreate() -> S_OK");
    }
    S_OK
}

fn alloc_shape_array(count: u32, stride: usize, bufs: &mut Vec<ShapeBuf>) -> *mut c_void {
    if count == 0 {
        return core::ptr::null_mut();
    }
    let bytes = (count as usize).saturating_mul(stride).max(64);
    let Ok(layout) = Layout::from_size_align(bytes, 64) else {
        return core::ptr::null_mut();
    };
    let ptr = unsafe { alloc_zeroed(layout) };
    if ptr.is_null() {
        return core::ptr::null_mut();
    }
    bufs.push(ShapeBuf {
        ptr: ptr as usize,
        layout,
    });
    ptr as *mut c_void
}

pub unsafe extern "win64" fn hle_acphal_allocate_shape_contexts(
    ctx: *mut AcpShapeContextCounts,
) -> HResult {
    if ctx.is_null() {
        return E_INVALIDARG;
    }
    let mut st = apu_state().lock().unwrap();
    for b in st.shape_bufs.drain(..) {
        dealloc(b.ptr as *mut u8, b.layout);
    }
    let c = &mut *ctx;
    c.src_contexts = alloc_shape_array(c.num_src_contexts, 32, &mut st.shape_bufs);
    c.eq_comp_contexts = alloc_shape_array(c.num_eq_comp_contexts, 64, &mut st.shape_bufs);
    c.filt_vol_contexts = alloc_shape_array(c.num_filt_vol_contexts, 64, &mut st.shape_bufs);
    c.dma_contexts = alloc_shape_array(c.num_dma_contexts, 16, &mut st.shape_bufs);
    c.xma_contexts = alloc_shape_array(c.num_xma_contexts, 64, &mut st.shape_bufs);
    c.pcm_contexts = alloc_shape_array(c.num_pcm_contexts, 64, &mut st.shape_bufs);
    if let Some(rt) = current_runtime() {
        rt.trace(format!(
            "acphal!AcpHalAllocateShapeContexts(src={}, dma={}, xma={}, pcm={}) -> S_OK",
            c.num_src_contexts, c.num_dma_contexts, c.num_xma_contexts, c.num_pcm_contexts
        ));
    }
    S_OK
}

pub unsafe extern "win64" fn hle_acphal_release_shape_contexts() -> HResult {
    let mut st = apu_state().lock().unwrap();
    for b in st.shape_bufs.drain(..) {
        dealloc(b.ptr as *mut u8, b.layout);
    }
    S_OK
}

pub unsafe extern "win64" fn hle_apu_create_heap(
    cached_size_bytes: u32,
    non_cached_size_bytes: u32,
) -> HResult {
    let mut st = apu_state().lock().unwrap();
    let total = (cached_size_bytes as u64) + (non_cached_size_bytes as u64);
    if total > 0 {
        st.heap_capacity = total.min(0x7FFF_FFFF) as u32;
    }
    if let Some(rt) = current_runtime() {
        rt.trace(format!(
            "acphal!ApuCreateHeap(cached={cached_size_bytes}, non_cached={non_cached_size_bytes}) -> S_OK"
        ));
    }
    S_OK
}

pub unsafe extern "win64" fn hle_apu_alloc(
    virtual_address: *mut *mut c_void,
    physical_address: *mut u32,
    size_in_bytes: u32,
    alignment_in_bytes: u32,
    _flags: u32,
) -> HResult {
    if virtual_address.is_null() || size_in_bytes == 0 {
        return E_INVALIDARG;
    }
    let align = (alignment_in_bytes as usize).max(64).next_power_of_two();
    let Ok(layout) = Layout::from_size_align(size_in_bytes as usize, align) else {
        return E_INVALIDARG;
    };
    let mem = alloc_zeroed(layout);
    if mem.is_null() {
        return E_OUTOFMEMORY;
    }

    let mut st = apu_state().lock().unwrap();
    let align_u32 = align as u32;
    let pa = (st.next_pa.wrapping_add(align_u32 - 1)) & !(align_u32 - 1);
    st.next_pa = pa.wrapping_add(size_in_bytes);
    st.bytes_allocated = st.bytes_allocated.saturating_add(size_in_bytes);

    let va = mem as usize;
    st.by_va.insert(
        va,
        ApuAllocation {
            va,
            pa,
            size: size_in_bytes,
            align,
        },
    );
    st.by_pa.insert(pa, va);

    *virtual_address = mem as *mut c_void;
    if !physical_address.is_null() {
        *physical_address = pa;
    }
    S_OK
}

pub unsafe extern "win64" fn hle_apu_free(virtual_address: *mut c_void) -> HResult {
    if virtual_address.is_null() {
        return S_OK;
    }
    let va = virtual_address as usize;
    let mut st = apu_state().lock().unwrap();
    if let Some(rec) = st.by_va.remove(&va) {
        st.by_pa.remove(&rec.pa);
        st.bytes_allocated = st.bytes_allocated.saturating_sub(rec.size);
        if let Ok(layout) = Layout::from_size_align(rec.size as usize, rec.align) {
            dealloc(rec.va as *mut u8, layout);
        }
        S_OK
    } else {
        E_INVALIDARG
    }
}

pub unsafe extern "win64" fn hle_apu_map_virtual_address(virtual_address: *const c_void) -> u32 {
    if virtual_address.is_null() {
        return 0;
    }
    let va = virtual_address as usize;
    let st = apu_state().lock().unwrap();
    for rec in st.by_va.values() {
        if va >= rec.va && va < rec.va + rec.size as usize {
            return rec.pa + (va - rec.va) as u32;
        }
    }
    0
}

pub unsafe extern "win64" fn hle_apu_map_apu_address(apu_pa: u32) -> *mut c_void {
    let st = apu_state().lock().unwrap();
    for rec in st.by_va.values() {
        if apu_pa >= rec.pa && apu_pa < rec.pa.saturating_add(rec.size) {
            return (rec.va + (apu_pa - rec.pa) as usize) as *mut c_void;
        }
    }
    core::ptr::null_mut()
}

pub unsafe extern "win64" fn hle_apu_is_virtual_address_valid(
    virtual_address: *const c_void,
    size_in_bytes: u32,
) -> i32 {
    if virtual_address.is_null() {
        return 0;
    }
    let va = virtual_address as usize;
    let st = apu_state().lock().unwrap();
    for rec in st.by_va.values() {
        if va >= rec.va && va.saturating_add(size_in_bytes as usize) <= rec.va + rec.size as usize {
            return 1;
        }
    }
    0
}

pub unsafe extern "win64" fn hle_apu_heap_get_state(
    out: *mut ApuHeapState,
    _flags: u32,
) -> HResult {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let st = apu_state().lock().unwrap();
    let free = st.heap_capacity.saturating_sub(st.bytes_allocated);
    *out = ApuHeapState {
        bytes_free: free,
        bytes_allocated: st.bytes_allocated,
        bytes_lost: 0,
        maximum_block_size_available: free,
        allocation_count: st.by_va.len() as u32,
    };
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apu_heap_alloc_map_and_acp_channel_roundtrip() {
        unsafe {
            assert_eq!(hle_apu_create_heap(16 * 1024 * 1024, 4 * 1024 * 1024), S_OK);
            let mut va: *mut c_void = core::ptr::null_mut();
            let mut pa: u32 = 0;
            assert_eq!(hle_apu_alloc(&mut va, &mut pa, 4096, 256, 0), S_OK);
            assert!(!va.is_null());
            assert_ne!(pa, 0);
            assert_eq!(hle_apu_map_virtual_address(va), pa);
            assert_eq!(hle_apu_map_apu_address(pa), va);
            assert_eq!(hle_apu_is_virtual_address_valid(va, 4096), 1);

            let mut state = ApuHeapState::default();
            assert_eq!(hle_apu_heap_get_state(&mut state, 0), S_OK);
            assert!(state.bytes_allocated >= 4096);

            let mut shapes = AcpShapeContextCounts {
                num_src_contexts: 4,
                num_dma_contexts: 2,
                num_xma_contexts: 1,
                ..Default::default()
            };
            assert_eq!(hle_acphal_allocate_shape_contexts(&mut shapes), S_OK);
            assert!(!shapes.src_contexts.is_null());
            assert!(!shapes.dma_contexts.is_null());
            assert!(!shapes.xma_contexts.is_null());
            assert_eq!(hle_acphal_release_shape_contexts(), S_OK);

            let mut acp: *mut c_void = core::ptr::null_mut();
            assert_eq!(hle_acphal_create(&mut acp), S_OK);
            let vtbl = &**(acp as *const *const IAcpHalVtbl);
            assert_eq!((vtbl.connect)(acp, 64, 64), S_OK);
            assert_eq!((vtbl.submit_command)(acp, 0x10, 42, 0, core::ptr::null(), 0), S_OK);
            assert_eq!((vtbl.get_num_messages)(acp), 2);
            let mut m = AcpHalMessage::default();
            assert_eq!((vtbl.pop_message)(acp, &mut m), 1);
            assert_eq!(m.msg_type, 0x01); // AUDIO_FRAME
            assert_eq!((vtbl.pop_message)(acp, &mut m), 1);
            assert_eq!(m.msg_type, 0x02); // COMMAND_DONE
            assert_eq!(m.id, 42);
            assert_eq!((vtbl.disconnect)(acp), S_OK);
            assert_eq!((vtbl.pop_message)(acp, &mut m), 1);
            assert_eq!(m.msg_type, 0x80); // DISCONNECTED
            assert_eq!((vtbl.release)(acp), 0);
            assert_eq!(hle_apu_free(va), S_OK);
        }
    }
}
