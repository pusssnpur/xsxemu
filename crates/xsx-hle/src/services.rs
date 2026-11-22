//! Offline clean-room Xbox Live / GDK services (`XUser`, `XGameSave`, `XStore`).
//!
//! - `XUser`: provides a local offline Xbox profile (`Gamertag = "XsxPlayer1"`, XUID `0x0009_0000_0000_0001`)
//! - `XGameSave`: persists named containers and binary save blobs directly inside `VirtualFs::save_root` (`S:\`)
//! - `XStore`: provides an active offline game license stub

use core::ffi::c_char;
use std::collections::HashMap;
use std::ffi::CStr;
use std::fs;
use std::path::PathBuf;

use xsx_abi::com::{HResult, E_FAIL, E_INVALIDARG, S_OK};
use xsx_abi::gdk::Handle;
use xsx_abi::services::{XAsyncBlock, XStoreGameLicense};

use crate::registry::ModuleRegistry;
use crate::runtime::{current_runtime, KernelObject};

pub const LOCAL_GAMERTAG: &str = "XsxPlayer1";
pub const LOCAL_XUID: u64 = 0x0009_0000_0000_0001;

#[derive(Debug, Clone)]
pub struct SaveContainerState {
    pub name: String,
    pub host_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SaveUpdateState {
    pub container_dir: PathBuf,
    pub writes: HashMap<String, Vec<u8>>,
    pub deletes: Vec<String>,
}

pub fn register(reg: &mut ModuleRegistry) {
    // XUser (exported by xgameruntime.dll / xuser.dll)
    reg.register_export("xgameruntime.dll", "XUserAddAsync", hle_xuser_add_async as *const () as usize);
    reg.register_export("xgameruntime.dll", "XUserAddResult", hle_xuser_add_result as *const () as usize);
    reg.register_export("xgameruntime.dll", "XUserGetGamertag", hle_xuser_get_gamertag as *const () as usize);
    reg.register_export("xgameruntime.dll", "XUserGetId", hle_xuser_get_id as *const () as usize);
    reg.register_export("xgameruntime.dll", "XUserCloseHandle", hle_xuser_close_handle as *const () as usize);

    // XGameSave (persisted in VirtualFs::save_root)
    reg.register_export("xgameruntime.dll", "XGameSaveInitializeProvider", hle_xgamesave_init_provider as *const () as usize);
    reg.register_export("xgameruntime.dll", "XGameSaveCloseProvider", hle_xgamesave_close_handle as *const () as usize);
    reg.register_export("xgameruntime.dll", "XGameSaveCreateContainer", hle_xgamesave_create_container as *const () as usize);
    reg.register_export("xgameruntime.dll", "XGameSaveCloseContainer", hle_xgamesave_close_handle as *const () as usize);
    reg.register_export("xgameruntime.dll", "XGameSaveCreateUpdate", hle_xgamesave_create_update as *const () as usize);
    reg.register_export("xgameruntime.dll", "XGameSaveSubmitBlobWrite", hle_xgamesave_submit_blob_write as *const () as usize);
    reg.register_export("xgameruntime.dll", "XGameSaveSubmitUpdate", hle_xgamesave_submit_update as *const () as usize);
    reg.register_export("xgameruntime.dll", "XGameSaveCloseUpdate", hle_xgamesave_close_handle as *const () as usize);
    reg.register_export("xgameruntime.dll", "XGameSaveReadBlobData", hle_xgamesave_read_blob_data as *const () as usize);

    // XStore (offline active license)
    reg.register_export("xgameruntime.dll", "XStoreCreateContext", hle_xstore_create_context as *const () as usize);
    reg.register_export("xgameruntime.dll", "XStoreCloseContext", hle_xgamesave_close_handle as *const () as usize);
    reg.register_export("xgameruntime.dll", "XStoreQueryGameLicense", hle_xstore_query_game_license as *const () as usize);
}

// ===========================================================================
// XUser
// ===========================================================================

unsafe extern "win64" fn hle_xuser_add_async(
    _options: u32,
    async_block: *mut XAsyncBlock,
) -> HResult {
    if let Some(rt) = current_runtime() {
        rt.trace("xgameruntime!XUserAddAsync() -> completed immediately (offline local user)");
    }
    if !async_block.is_null() {
        if let Some(cb) = (*async_block).callback {
            cb(async_block);
        }
    }
    S_OK
}

unsafe extern "win64" fn hle_xuser_add_result(
    _async_block: *mut XAsyncBlock,
    new_user: *mut Handle,
) -> HResult {
    if new_user.is_null() {
        return E_INVALIDARG;
    }
    let Some(rt) = current_runtime() else {
        return E_FAIL;
    };
    let h = rt.insert_handle(KernelObject::User {
        xuid: LOCAL_XUID,
        gamertag: LOCAL_GAMERTAG.to_string(),
    });
    *new_user = h;
    rt.trace(format!(
        "xgameruntime!XUserAddResult() -> {h:?} ({LOCAL_GAMERTAG}, XUID=0x{LOCAL_XUID:016X})"
    ));
    S_OK
}

unsafe extern "win64" fn hle_xuser_get_gamertag(
    _user: Handle,
    _component: u32,
    gamertag_size: usize,
    gamertag: *mut c_char,
    gamertag_used: *mut usize,
) -> HResult {
    if gamertag.is_null() || gamertag_size == 0 {
        return E_INVALIDARG;
    }
    let bytes = LOCAL_GAMERTAG.as_bytes();
    let copy_len = bytes.len().min(gamertag_size - 1);
    core::ptr::copy_nonoverlapping(bytes.as_ptr(), gamertag as *mut u8, copy_len);
    *gamertag.add(copy_len) = 0;
    if !gamertag_used.is_null() {
        *gamertag_used = copy_len + 1;
    }
    S_OK
}

unsafe extern "win64" fn hle_xuser_get_id(_user: Handle, user_id: *mut u64) -> HResult {
    if user_id.is_null() {
        return E_INVALIDARG;
    }
    *user_id = LOCAL_XUID;
    S_OK
}

unsafe extern "win64" fn hle_xuser_close_handle(user: Handle) {
    if let Some(rt) = current_runtime() {
        rt.close_handle(user);
    }
}

// ===========================================================================
// XGameSave (Local Disk Persistence in VirtualFs::save_root)
// ===========================================================================

fn sanitize_name(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
        .collect()
}

unsafe extern "win64" fn hle_xgamesave_init_provider(
    _requesting_user: Handle,
    _scid: *const c_char,
    _sync_on_demand: i32,
    provider_out: *mut Handle,
) -> HResult {
    if provider_out.is_null() {
        return E_INVALIDARG;
    }
    let Some(rt) = current_runtime() else {
        return E_FAIL;
    };
    let _ = rt.vfs.ensure_roots();
    let h = rt.insert_handle(KernelObject::SaveProvider {
        root_dir: rt.vfs.save_root.clone(),
    });
    *provider_out = h;
    rt.trace(format!(
        "xgameruntime!XGameSaveInitializeProvider() -> {h:?} ({})",
        rt.vfs.save_root.display()
    ));
    S_OK
}

unsafe extern "win64" fn hle_xgamesave_create_container(
    provider: Handle,
    name: *const c_char,
    container_out: *mut Handle,
) -> HResult {
    if name.is_null() || container_out.is_null() {
        return E_INVALIDARG;
    }
    let Some(rt) = current_runtime() else {
        return E_FAIL;
    };
    let root = rt
        .get_save_provider_root(provider)
        .unwrap_or_else(|| rt.vfs.save_root.clone());
    let cname = sanitize_name(&CStr::from_ptr(name).to_string_lossy());
    if cname.is_empty() {
        return E_INVALIDARG;
    }
    let host_dir = root.join(&cname);
    if fs::create_dir_all(&host_dir).is_err() {
        return E_FAIL;
    }
    let h = rt.insert_handle(KernelObject::SaveContainer(SaveContainerState {
        name: cname.clone(),
        host_dir,
    }));
    *container_out = h;
    rt.trace(format!(
        "xgameruntime!XGameSaveCreateContainer({cname:?}) -> {h:?}"
    ));
    S_OK
}

unsafe extern "win64" fn hle_xgamesave_create_update(
    container: Handle,
    _display_name: *const c_char,
    update_out: *mut Handle,
) -> HResult {
    if update_out.is_null() {
        return E_INVALIDARG;
    }
    let Some(rt) = current_runtime() else {
        return E_FAIL;
    };
    let Some(c) = rt.get_save_container(container) else {
        return E_INVALIDARG;
    };
    let h = rt.insert_handle(KernelObject::SaveUpdate(SaveUpdateState {
        container_dir: c.host_dir,
        writes: HashMap::new(),
        deletes: Vec::new(),
    }));
    *update_out = h;
    S_OK
}

unsafe extern "win64" fn hle_xgamesave_submit_blob_write(
    update: Handle,
    blob_name: *const c_char,
    data: *const u8,
    byte_count: usize,
) -> HResult {
    if blob_name.is_null() || (data.is_null() && byte_count > 0) {
        return E_INVALIDARG;
    }
    let Some(rt) = current_runtime() else {
        return E_FAIL;
    };
    let bname = sanitize_name(&CStr::from_ptr(blob_name).to_string_lossy());
    if bname.is_empty() {
        return E_INVALIDARG;
    }
    let bytes = if byte_count > 0 {
        std::slice::from_raw_parts(data, byte_count).to_vec()
    } else {
        Vec::new()
    };
    if rt.stage_save_blob_write(update, bname, bytes) {
        S_OK
    } else {
        E_INVALIDARG
    }
}

unsafe extern "win64" fn hle_xgamesave_submit_update(update: Handle) -> HResult {
    let Some(rt) = current_runtime() else {
        return E_FAIL;
    };
    match rt.commit_save_update(update) {
        Ok(count) => {
            rt.trace(format!("xgameruntime!XGameSaveSubmitUpdate() -> committed {count} blob(s)"));
            S_OK
        }
        Err(_) => E_FAIL,
    }
}

unsafe extern "win64" fn hle_xgamesave_read_blob_data(
    container: Handle,
    blob_name: *const c_char,
    buffer: *mut u8,
    buffer_size: usize,
    bytes_read_out: *mut usize,
) -> HResult {
    if blob_name.is_null() || buffer.is_null() {
        return E_INVALIDARG;
    }
    let Some(rt) = current_runtime() else {
        return E_FAIL;
    };
    let Some(c) = rt.get_save_container(container) else {
        return E_INVALIDARG;
    };
    let bname = sanitize_name(&CStr::from_ptr(blob_name).to_string_lossy());
    let path = c.host_dir.join(&bname);
    let Ok(bytes) = fs::read(&path) else {
        return E_FAIL;
    };
    let n = bytes.len().min(buffer_size);
    core::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, n);
    if !bytes_read_out.is_null() {
        *bytes_read_out = n;
    }
    rt.trace(format!(
        "xgameruntime!XGameSaveReadBlobData({bname:?}) -> {n} bytes"
    ));
    S_OK
}

unsafe extern "win64" fn hle_xgamesave_close_handle(h: Handle) {
    if let Some(rt) = current_runtime() {
        rt.close_handle(h);
    }
}

// ===========================================================================
// XStore
// ===========================================================================

unsafe extern "win64" fn hle_xstore_create_context(
    _user: Handle,
    context_out: *mut Handle,
) -> HResult {
    if context_out.is_null() {
        return E_INVALIDARG;
    }
    let Some(rt) = current_runtime() else {
        return E_FAIL;
    };
    let h = rt.insert_handle(KernelObject::StoreContext);
    *context_out = h;
    rt.trace(format!("xgameruntime!XStoreCreateContext() -> {h:?}"));
    S_OK
}

unsafe extern "win64" fn hle_xstore_query_game_license(
    _store_context: Handle,
    license_out: *mut XStoreGameLicense,
) -> HResult {
    if license_out.is_null() {
        return E_INVALIDARG;
    }
    let mut lic = XStoreGameLicense {
        sku_store_id: [0; 18],
        is_active: 1,
        is_trial_owned_by_this_user: 0,
        is_disc_license: 0,
        is_trial: 0,
    };
    let tag = b"XSXEMU_OFFLINE";
    lic.sku_store_id[..tag.len()].copy_from_slice(tag);
    *license_out = lic;
    if let Some(rt) = current_runtime() {
        rt.trace("xgameruntime!XStoreQueryGameLicense() -> active offline license");
    }
    S_OK
}
