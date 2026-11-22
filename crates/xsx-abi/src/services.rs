//! Phase 4 ABI definitions: GameInput / XInput, XAudio2, DirectStorage (`dstorage_x`),
//! and Microsoft GDK Services (`XUser`, `XGameSave`, `XStore`).

use core::ffi::{c_char, c_void};

use crate::com::{Guid, HResult, IUnknownVtbl};
use crate::gdk::Handle;

// ===========================================================================
// XInput & GameInput ABI
// ===========================================================================

pub const XINPUT_GAMEPAD_DPAD_UP: u16 = 0x0001;
pub const XINPUT_GAMEPAD_DPAD_DOWN: u16 = 0x0002;
pub const XINPUT_GAMEPAD_DPAD_LEFT: u16 = 0x0004;
pub const XINPUT_GAMEPAD_DPAD_RIGHT: u16 = 0x0008;
pub const XINPUT_GAMEPAD_START: u16 = 0x0010;
pub const XINPUT_GAMEPAD_BACK: u16 = 0x0020;
pub const XINPUT_GAMEPAD_A: u16 = 0x1000;
pub const XINPUT_GAMEPAD_B: u16 = 0x2000;
pub const XINPUT_GAMEPAD_X: u16 = 0x4000;
pub const XINPUT_GAMEPAD_Y: u16 = 0x8000;

pub const ERROR_SUCCESS: u32 = 0;
pub const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct XInputGamepad {
    pub w_buttons: u16,
    pub b_left_trigger: u8,
    pub b_right_trigger: u8,
    pub s_thumb_lx: i16,
    pub s_thumb_ly: i16,
    pub s_thumb_rx: i16,
    pub s_thumb_ry: i16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct XInputState {
    pub dw_packet_number: u32,
    pub gamepad: XInputGamepad,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct XInputVibration {
    pub w_left_motor_speed: u16,
    pub w_right_motor_speed: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct GameInputGamepadState {
    pub buttons: u32,
    pub left_trigger: f32,
    pub right_trigger: f32,
    pub left_thumbstick_x: f32,
    pub left_thumbstick_y: f32,
    pub right_thumbstick_x: f32,
    pub right_thumbstick_y: f32,
}

#[repr(C)]
pub struct IGameInputVtbl {
    pub base: IUnknownVtbl,
    // Slot 3
    pub get_current_timestamp: unsafe extern "win64" fn(this: *mut c_void) -> u64,
    // Slot 4
    pub get_current_gamepad_state: unsafe extern "win64" fn(
        this: *mut c_void,
        user_index: u32,
        out_state: *mut GameInputGamepadState,
    ) -> HResult,
}

// ===========================================================================
// XAudio2_9 ABI
// ===========================================================================

#[repr(C, packed(1))]
#[derive(Debug, Clone, Copy, Default)]
pub struct WaveFormatEx {
    pub w_format_tag: u16,
    pub n_channels: u16,
    pub n_samples_per_sec: u32,
    pub n_avg_bytes_per_sec: u32,
    pub n_block_align: u16,
    pub w_bits_per_sample: u16,
    pub cb_size: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct XAudio2Buffer {
    pub flags: u32,
    pub audio_bytes: u32,
    pub p_audio_data: *const u8,
    pub play_begin: u32,
    pub play_length: u32,
    pub loop_begin: u32,
    pub loop_length: u32,
    pub loop_count: u32,
    pub p_context: *mut c_void,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XAudio2VoiceState {
    pub p_current_buffer_context: usize,
    pub buffers_queued: u32,
    pub _pad: u32,
    pub samples_played: u64,
}

#[repr(C)]
pub struct IXAudio2Vtbl {
    pub base: IUnknownVtbl,
    // Slot 3
    pub create_mastering_voice: unsafe extern "win64" fn(
        this: *mut c_void,
        pp_mastering_voice: *mut *mut c_void,
        input_channels: u32,
        input_sample_rate: u32,
        flags: u32,
    ) -> HResult,
    // Slot 4
    pub create_source_voice: unsafe extern "win64" fn(
        this: *mut c_void,
        pp_source_voice: *mut *mut c_void,
        p_source_format: *const WaveFormatEx,
        flags: u32,
    ) -> HResult,
    // Slot 5
    pub start_engine: unsafe extern "win64" fn(this: *mut c_void) -> HResult,
    // Slot 6
    pub stop_engine: unsafe extern "win64" fn(this: *mut c_void),
}

#[repr(C)]
pub struct IXAudio2MasteringVoiceVtbl {
    pub base: IUnknownVtbl,
    // Slot 3
    pub set_volume: unsafe extern "win64" fn(this: *mut c_void, volume: f32) -> HResult,
    // Slot 4
    pub destroy_voice: unsafe extern "win64" fn(this: *mut c_void),
}

#[repr(C)]
pub struct IXAudio2SourceVoiceVtbl {
    pub base: IUnknownVtbl,
    // Slot 3
    pub start: unsafe extern "win64" fn(this: *mut c_void, flags: u32) -> HResult,
    // Slot 4
    pub stop: unsafe extern "win64" fn(this: *mut c_void, flags: u32) -> HResult,
    // Slot 5
    pub submit_source_buffer: unsafe extern "win64" fn(
        this: *mut c_void,
        p_buffer: *const XAudio2Buffer,
    ) -> HResult,
    // Slot 6
    pub get_state: unsafe extern "win64" fn(
        this: *mut c_void,
        p_voice_state: *mut XAudio2VoiceState,
    ),
    // Slot 7
    pub destroy_voice: unsafe extern "win64" fn(this: *mut c_void),
}

// ===========================================================================
// DirectStorage (`dstorage_x.dll`) ABI
// ===========================================================================

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DStorageQueueDesc {
    pub source_type: u32,
    pub capacity: u16,
    pub priority: i8,
    pub _pad: u8,
    pub name: *const c_char,
    pub device: *mut c_void,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DStorageRequest {
    pub source_file: *mut c_void,
    pub file_offset: u64,
    pub size_bytes: u32,
    pub compression_format: u32,
    pub destination_buffer: *mut u8,
    pub destination_size: u32,
    pub _pad: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DStorageQueueInfo {
    pub empty_slot_count: u32,
    pub requests_completed: u32,
    pub error_count: u32,
}

#[repr(C)]
pub struct IDStorageFactoryXVtbl {
    pub base: IUnknownVtbl,
    // Slot 3
    pub create_queue: unsafe extern "win64" fn(
        this: *mut c_void,
        desc: *const DStorageQueueDesc,
        riid: *const Guid,
        ppv: *mut *mut c_void,
    ) -> HResult,
    // Slot 4 (UTF-8 path convenience & UTF-16 OpenFileW)
    pub open_file_a: unsafe extern "win64" fn(
        this: *mut c_void,
        path: *const c_char,
        riid: *const Guid,
        ppv: *mut *mut c_void,
    ) -> HResult,
    // Slot 5
    pub open_file_w: unsafe extern "win64" fn(
        this: *mut c_void,
        path: *const u16,
        riid: *const Guid,
        ppv: *mut *mut c_void,
    ) -> HResult,
}

#[repr(C)]
pub struct IDStorageFileXVtbl {
    pub base: IUnknownVtbl,
    // Slot 3
    pub get_file_size: unsafe extern "win64" fn(this: *mut c_void) -> u64,
}

#[repr(C)]
pub struct IDStorageQueueXVtbl {
    pub base: IUnknownVtbl,
    // Slot 3
    pub enqueue_request:
        unsafe extern "win64" fn(this: *mut c_void, request: *const DStorageRequest),
    // Slot 4
    pub enqueue_signal:
        unsafe extern "win64" fn(this: *mut c_void, fence: *mut c_void, value: u64),
    // Slot 5
    pub submit: unsafe extern "win64" fn(this: *mut c_void),
    // Slot 6
    pub query: unsafe extern "win64" fn(this: *mut c_void, info: *mut DStorageQueueInfo),
}

// ===========================================================================
// Microsoft GDK Services (`XAsyncBlock`, `XUser`, `XGameSave`, `XStore`)
// ===========================================================================

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct XAsyncBlock {
    pub queue: Handle,
    pub context: *mut c_void,
    pub callback: Option<unsafe extern "win64" fn(async_block: *mut XAsyncBlock)>,
    pub internal: [u8; 32],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XStoreGameLicense {
    pub sku_store_id: [u8; 18],
    pub is_active: u8,
    pub is_trial_owned_by_this_user: u8,
    pub is_disc_license: u8,
    pub is_trial: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase4_abi_sizes() {
        assert_eq!(core::mem::size_of::<XInputGamepad>(), 12);
        assert_eq!(core::mem::size_of::<XInputState>(), 16);
        assert_eq!(core::mem::size_of::<XInputVibration>(), 4);
        assert_eq!(core::mem::size_of::<WaveFormatEx>(), 18);
        assert_eq!(core::mem::size_of::<XAudio2VoiceState>(), 24);
    }
}
