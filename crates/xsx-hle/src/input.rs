//! `GameInput.dll` & `XInput1_4.dll` / `XInput9_1_0.dll` HLE bridge.
//!
//! Supports live Windows XInput polling (`xinput1_4.dll`) when a physical gamepad
//! is attached on Windows, plus a thread-safe `VirtualGamepad` override/fallback
//! for deterministic testing and headless operation.

use core::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use xsx_abi::com::{
    Guid, HResult, IUnknownVtbl, E_INVALIDARG, E_NOINTERFACE, E_POINTER, IID_IGAMEINPUT,
    IID_IUNKNOWN, S_OK,
};
use xsx_abi::services::{
    GameInputGamepadState, IGameInputVtbl, XInputGamepad, XInputState, XInputVibration,
    ERROR_DEVICE_NOT_CONNECTED, ERROR_SUCCESS,
};

use crate::registry::ModuleRegistry;
use crate::runtime::{current_runtime, GuestRuntime};

#[derive(Debug, Default, Clone)]
pub struct VirtualGamepadState {
    pub connected: bool,
    pub packet_number: u32,
    pub gamepad: XInputGamepad,
    pub last_vibration: XInputVibration,
}

impl VirtualGamepadState {
    pub fn connected_default() -> Self {
        Self {
            connected: true,
            packet_number: 1,
            gamepad: XInputGamepad::default(),
            last_vibration: XInputVibration::default(),
        }
    }
}

pub fn register(reg: &mut ModuleRegistry) {
    for dll in ["xinput1_4.dll", "xinput9_1_0.dll", "xinput1_3.dll"] {
        reg.register_export(dll, "XInputGetState", hle_xinput_get_state as *const () as usize);
        reg.register_export(dll, "XInputSetState", hle_xinput_set_state as *const () as usize);
    }
    reg.register_export(
        "gameinput.dll",
        "GameInputCreate",
        hle_gameinput_create as *const () as usize,
    );
}

#[cfg(target_os = "windows")]
fn try_windows_xinput_get_state(user_index: u32) -> Option<XInputState> {
    use core::ffi::c_char;
    type FnGetState = unsafe extern "system" fn(u32, *mut XInputState) -> u32;
    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryA(name: *const c_char) -> *mut c_void;
        fn GetProcAddress(hmod: *mut c_void, name: *const c_char) -> *mut c_void;
    }
    unsafe {
        let dll = LoadLibraryA(b"xinput1_4.dll\0".as_ptr() as *const c_char);
        if dll.is_null() {
            return None;
        }
        let proc = GetProcAddress(dll, b"XInputGetState\0".as_ptr() as *const c_char);
        if proc.is_null() {
            return None;
        }
        let func: FnGetState = std::mem::transmute(proc);
        let mut st = XInputState::default();
        if func(user_index, &mut st) == ERROR_SUCCESS {
            Some(st)
        } else {
            None
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn try_windows_xinput_get_state(_user_index: u32) -> Option<XInputState> {
    None
}

unsafe extern "win64" fn hle_xinput_get_state(
    dw_user_index: u32,
    p_state: *mut XInputState,
) -> u32 {
    if p_state.is_null() || dw_user_index >= 4 {
        return ERROR_DEVICE_NOT_CONNECTED;
    }
    let Some(rt) = current_runtime() else {
        return ERROR_DEVICE_NOT_CONNECTED;
    };

    // Prefer injected virtual gamepad buttons when active, else poll Windows XInput
    let vstate = rt.get_gamepad_state(dw_user_index);
    if vstate.connected {
        let mut out = XInputState {
            dw_packet_number: vstate.packet_number,
            gamepad: vstate.gamepad,
        };
        if out.gamepad.w_buttons == 0 {
            if let Some(hw) = try_windows_xinput_get_state(dw_user_index) {
                out = hw;
            }
        }
        *p_state = out;
        rt.trace(format!(
            "xinput!XInputGetState(user={dw_user_index}) -> buttons=0x{:04X}",
            out.gamepad.w_buttons
        ));
        return ERROR_SUCCESS;
    }
    ERROR_DEVICE_NOT_CONNECTED
}

unsafe extern "win64" fn hle_xinput_set_state(
    dw_user_index: u32,
    p_vibration: *const XInputVibration,
) -> u32 {
    if p_vibration.is_null() || dw_user_index >= 4 {
        return ERROR_DEVICE_NOT_CONNECTED;
    }
    let Some(rt) = current_runtime() else {
        return ERROR_DEVICE_NOT_CONNECTED;
    };
    let vib = *p_vibration;
    rt.set_gamepad_vibration(dw_user_index, vib);
    rt.trace(format!(
        "xinput!XInputSetState(user={dw_user_index}, L={}, R={})",
        vib.w_left_motor_speed, vib.w_right_motor_speed
    ));
    ERROR_SUCCESS
}

// ===========================================================================
// IGameInput COM Object
// ===========================================================================

#[repr(C)]
struct ComGameInput {
    lp_vtbl: *const IGameInputVtbl,
    ref_count: AtomicU32,
    runtime: Arc<GuestRuntime>,
}

static GAMEINPUT_VTBL: IGameInputVtbl = IGameInputVtbl {
    base: IUnknownVtbl {
        query_interface: gi_qi,
        add_ref: gi_add_ref,
        release: gi_release,
    },
    get_current_timestamp: gi_get_timestamp,
    get_current_gamepad_state: gi_get_gamepad_state,
};

unsafe extern "win64" fn hle_gameinput_create(pp_game_input: *mut *mut c_void) -> HResult {
    if pp_game_input.is_null() {
        return E_INVALIDARG;
    }
    let Some(rt) = current_runtime() else {
        return E_INVALIDARG;
    };
    rt.trace("gameinput!GameInputCreate() -> S_OK");
    let obj = Box::new(ComGameInput {
        lp_vtbl: &GAMEINPUT_VTBL,
        ref_count: AtomicU32::new(1),
        runtime: rt,
    });
    *pp_game_input = Box::into_raw(obj) as *mut c_void;
    S_OK
}

unsafe extern "win64" fn gi_qi(
    this: *mut c_void,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    if ppv.is_null() || riid.is_null() {
        return E_POINTER;
    }
    let iid = *riid;
    if iid == IID_IUNKNOWN || iid == IID_IGAMEINPUT {
        gi_add_ref(this);
        *ppv = this;
        S_OK
    } else {
        *ppv = ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "win64" fn gi_add_ref(this: *mut c_void) -> u32 {
    let obj = &*(this as *const ComGameInput);
    obj.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "win64" fn gi_release(this: *mut c_void) -> u32 {
    let obj = &*(this as *const ComGameInput);
    let prev = obj.ref_count.fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        drop(Box::from_raw(this as *mut ComGameInput));
        0
    } else {
        prev - 1
    }
}

unsafe extern "win64" fn gi_get_timestamp(this: *mut c_void) -> u64 {
    let obj = &*(this as *const ComGameInput);
    obj.runtime.elapsed_us()
}

unsafe extern "win64" fn gi_get_gamepad_state(
    this: *mut c_void,
    user_index: u32,
    out_state: *mut GameInputGamepadState,
) -> HResult {
    if this.is_null() || out_state.is_null() {
        return E_INVALIDARG;
    }
    let obj = &*(this as *const ComGameInput);
    let st = obj.runtime.get_gamepad_state(user_index);
    *out_state = GameInputGamepadState {
        buttons: st.gamepad.w_buttons as u32,
        left_trigger: st.gamepad.b_left_trigger as f32 / 255.0,
        right_trigger: st.gamepad.b_right_trigger as f32 / 255.0,
        left_thumbstick_x: st.gamepad.s_thumb_lx as f32 / 32767.0,
        left_thumbstick_y: st.gamepad.s_thumb_ly as f32 / 32767.0,
        right_thumbstick_x: st.gamepad.s_thumb_rx as f32 / 32767.0,
        right_thumbstick_y: st.gamepad.s_thumb_ry as f32 / 32767.0,
    };
    S_OK
}
