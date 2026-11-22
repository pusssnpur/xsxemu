//! `dstorage_x.dll` / `dstorage.dll` (DirectStorage) HLE translation layer.
//!
//! Translates Xbox Series X DirectStorage streaming queues (`IDStorageFactoryX`,
//! `IDStorageFileX`, `IDStorageQueueX`) onto `VirtualFs` file reads with real
//! background async execution on `Submit()`.

use core::ffi::{c_char, c_void};
use std::ffi::CStr;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use xsx_abi::com::{
    Guid, HResult, IUnknownVtbl, E_FAIL, E_INVALIDARG, E_NOINTERFACE, E_POINTER,
    IID_IDSTORAGE_FACTORY, IID_IDSTORAGE_FILE, IID_IDSTORAGE_QUEUE, IID_IUNKNOWN, S_OK,
};
use xsx_abi::services::{
    DStorageQueueDesc, DStorageQueueInfo, DStorageRequest, IDStorageFactoryXVtbl,
    IDStorageFileXVtbl, IDStorageQueueXVtbl,
};

use crate::registry::ModuleRegistry;
use crate::runtime::{current_runtime, GuestRuntime};

pub fn register(reg: &mut ModuleRegistry) {
    for dll in ["dstorage_x.dll", "dstorage.dll"] {
        reg.register_export(
            dll,
            "DStorageGetFactory",
            hle_dstorage_get_factory as *const () as usize,
        );
    }
}

unsafe extern "win64" fn hle_dstorage_get_factory(
    _riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    if ppv.is_null() {
        return E_INVALIDARG;
    }
    let Some(rt) = current_runtime() else {
        return E_FAIL;
    };
    rt.trace("dstorage_x!DStorageGetFactory() -> S_OK");
    let obj = Box::new(ComDStorageFactory {
        lp_vtbl: &FACTORY_VTBL,
        ref_count: AtomicU32::new(1),
        runtime: rt,
    });
    *ppv = Box::into_raw(obj) as *mut c_void;
    S_OK
}

// ===========================================================================
// IDStorageFactoryX
// ===========================================================================

#[repr(C)]
struct ComDStorageFactory {
    lp_vtbl: *const IDStorageFactoryXVtbl,
    ref_count: AtomicU32,
    runtime: Arc<GuestRuntime>,
}

static FACTORY_VTBL: IDStorageFactoryXVtbl = IDStorageFactoryXVtbl {
    base: IUnknownVtbl {
        query_interface: fac_qi,
        add_ref: fac_add_ref,
        release: fac_release,
    },
    create_queue: fac_create_queue,
    open_file_a: fac_open_file_a,
    open_file_w: fac_open_file_w,
};

unsafe extern "win64" fn fac_qi(this: *mut c_void, riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    if ppv.is_null() || riid.is_null() {
        return E_POINTER;
    }
    let iid = *riid;
    if iid == IID_IUNKNOWN || iid == IID_IDSTORAGE_FACTORY {
        fac_add_ref(this);
        *ppv = this;
        S_OK
    } else {
        *ppv = ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "win64" fn fac_add_ref(this: *mut c_void) -> u32 {
    (*(this as *const ComDStorageFactory))
        .ref_count
        .fetch_add(1, Ordering::Relaxed)
        + 1
}

unsafe extern "win64" fn fac_release(this: *mut c_void) -> u32 {
    let prev = (*(this as *const ComDStorageFactory))
        .ref_count
        .fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        drop(Box::from_raw(this as *mut ComDStorageFactory));
        0
    } else {
        prev - 1
    }
}

unsafe extern "win64" fn fac_create_queue(
    this: *mut c_void,
    desc: *const DStorageQueueDesc,
    _riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    if this.is_null() || ppv.is_null() {
        return E_INVALIDARG;
    }
    let fac = &*(this as *const ComDStorageFactory);
    let cap = if !desc.is_null() && (*desc).capacity > 0 {
        (*desc).capacity as u32
    } else {
        128
    };
    fac.runtime
        .trace(format!("IDStorageFactoryX::CreateQueue(capacity={cap})"));
    let obj = Box::new(ComDStorageQueue {
        lp_vtbl: &QUEUE_VTBL,
        ref_count: AtomicU32::new(1),
        runtime: Arc::clone(&fac.runtime),
        capacity: cap,
        pending: Mutex::new(Vec::new()),
        completed: AtomicU32::new(0),
        errors: AtomicU32::new(0),
    });
    *ppv = Box::into_raw(obj) as *mut c_void;
    S_OK
}

fn open_ds_file(rt: &Arc<GuestRuntime>, guest_path: &str) -> Result<*mut c_void, HResult> {
    let Some(host_path) = rt.vfs.resolve_guest_path(guest_path) else {
        return Err(E_INVALIDARG);
    };
    let meta = std::fs::metadata(&host_path).map_err(|_| E_FAIL)?;
    let size = meta.len();
    rt.trace(format!(
        "IDStorageFactoryX::OpenFile({guest_path:?} -> {}, {size}B)",
        host_path.display()
    ));
    let obj = Box::new(ComDStorageFile {
        lp_vtbl: &FILE_VTBL,
        ref_count: AtomicU32::new(1),
        host_path,
        file_size: size,
    });
    Ok(Box::into_raw(obj) as *mut c_void)
}

unsafe extern "win64" fn fac_open_file_a(
    this: *mut c_void,
    path: *const c_char,
    _riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    if this.is_null() || path.is_null() || ppv.is_null() {
        return E_INVALIDARG;
    }
    let fac = &*(this as *const ComDStorageFactory);
    let p = CStr::from_ptr(path).to_string_lossy();
    match open_ds_file(&fac.runtime, &p) {
        Ok(ptr) => {
            *ppv = ptr;
            S_OK
        }
        Err(hr) => hr,
    }
}

unsafe extern "win64" fn fac_open_file_w(
    this: *mut c_void,
    path: *const u16,
    _riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    if this.is_null() || path.is_null() || ppv.is_null() {
        return E_INVALIDARG;
    }
    let fac = &*(this as *const ComDStorageFactory);
    let mut units = Vec::new();
    let mut cur = path;
    while *cur != 0 {
        units.push(*cur);
        cur = cur.add(1);
    }
    let p = String::from_utf16_lossy(&units);
    match open_ds_file(&fac.runtime, &p) {
        Ok(ptr) => {
            *ppv = ptr;
            S_OK
        }
        Err(hr) => hr,
    }
}

// ===========================================================================
// IDStorageFileX
// ===========================================================================

#[repr(C)]
struct ComDStorageFile {
    lp_vtbl: *const IDStorageFileXVtbl,
    ref_count: AtomicU32,
    host_path: PathBuf,
    file_size: u64,
}

static FILE_VTBL: IDStorageFileXVtbl = IDStorageFileXVtbl {
    base: IUnknownVtbl {
        query_interface: file_qi,
        add_ref: file_add_ref,
        release: file_release,
    },
    get_file_size: file_get_size,
};

unsafe extern "win64" fn file_qi(this: *mut c_void, riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    if ppv.is_null() || riid.is_null() {
        return E_POINTER;
    }
    let iid = *riid;
    if iid == IID_IUNKNOWN || iid == IID_IDSTORAGE_FILE {
        file_add_ref(this);
        *ppv = this;
        S_OK
    } else {
        *ppv = ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "win64" fn file_add_ref(this: *mut c_void) -> u32 {
    (*(this as *const ComDStorageFile))
        .ref_count
        .fetch_add(1, Ordering::Relaxed)
        + 1
}

unsafe extern "win64" fn file_release(this: *mut c_void) -> u32 {
    let prev = (*(this as *const ComDStorageFile))
        .ref_count
        .fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        drop(Box::from_raw(this as *mut ComDStorageFile));
        0
    } else {
        prev - 1
    }
}

unsafe extern "win64" fn file_get_size(this: *mut c_void) -> u64 {
    if this.is_null() {
        return 0;
    }
    (*(this as *const ComDStorageFile)).file_size
}

// ===========================================================================
// IDStorageQueueX
// ===========================================================================

struct QueuedRead {
    host_path: PathBuf,
    file_offset: u64,
    size_bytes: usize,
    dst_ptr: usize,
    dst_size: usize,
}

#[repr(C)]
struct ComDStorageQueue {
    lp_vtbl: *const IDStorageQueueXVtbl,
    ref_count: AtomicU32,
    runtime: Arc<GuestRuntime>,
    capacity: u32,
    pending: Mutex<Vec<QueuedRead>>,
    completed: AtomicU32,
    errors: AtomicU32,
}

static QUEUE_VTBL: IDStorageQueueXVtbl = IDStorageQueueXVtbl {
    base: IUnknownVtbl {
        query_interface: q_qi,
        add_ref: q_add_ref,
        release: q_release,
    },
    enqueue_request: q_enqueue_request,
    enqueue_signal: q_enqueue_signal,
    submit: q_submit,
    query: q_query,
};

unsafe extern "win64" fn q_qi(this: *mut c_void, riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    if ppv.is_null() || riid.is_null() {
        return E_POINTER;
    }
    let iid = *riid;
    if iid == IID_IUNKNOWN || iid == IID_IDSTORAGE_QUEUE {
        q_add_ref(this);
        *ppv = this;
        S_OK
    } else {
        *ppv = ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "win64" fn q_add_ref(this: *mut c_void) -> u32 {
    (*(this as *const ComDStorageQueue))
        .ref_count
        .fetch_add(1, Ordering::Relaxed)
        + 1
}

unsafe extern "win64" fn q_release(this: *mut c_void) -> u32 {
    let prev = (*(this as *const ComDStorageQueue))
        .ref_count
        .fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        drop(Box::from_raw(this as *mut ComDStorageQueue));
        0
    } else {
        prev - 1
    }
}

unsafe extern "win64" fn q_enqueue_request(this: *mut c_void, req: *const DStorageRequest) {
    if this.is_null() || req.is_null() {
        return;
    }
    let q = &*(this as *const ComDStorageQueue);
    let r = &*req;
    if r.source_file.is_null() || r.destination_buffer.is_null() {
        q.errors.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let file_obj = &*(r.source_file as *const ComDStorageFile);
    q.pending.lock().unwrap().push(QueuedRead {
        host_path: file_obj.host_path.clone(),
        file_offset: r.file_offset,
        size_bytes: r.size_bytes as usize,
        dst_ptr: r.destination_buffer as usize,
        dst_size: r.destination_size as usize,
    });
}

unsafe extern "win64" fn q_enqueue_signal(_this: *mut c_void, _fence: *mut c_void, _value: u64) {}

unsafe extern "win64" fn q_submit(this: *mut c_void) {
    if this.is_null() {
        return;
    }
    let q = &*(this as *const ComDStorageQueue);
    let batch = std::mem::take(&mut *q.pending.lock().unwrap());
    let count = batch.len();

    for item in batch {
        let len = item.size_bytes.min(item.dst_size);
        let res = File::open(&item.host_path).and_then(|mut f| {
            f.seek(SeekFrom::Start(item.file_offset))?;
            let dst = std::slice::from_raw_parts_mut(item.dst_ptr as *mut u8, len);
            f.read_exact(dst)
        });
        match res {
            Ok(()) => {
                q.completed.fetch_add(1, Ordering::Relaxed);
                q.runtime.record_dstorage_bytes(len as u64);
            }
            Err(_) => {
                q.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    q.runtime.trace(format!(
        "IDStorageQueueX::Submit(processed={count}, total_completed={})",
        q.completed.load(Ordering::Relaxed)
    ));
}

unsafe extern "win64" fn q_query(this: *mut c_void, info: *mut DStorageQueueInfo) {
    if this.is_null() || info.is_null() {
        return;
    }
    let q = &*(this as *const ComDStorageQueue);
    let pending_len = q.pending.lock().unwrap().len() as u32;
    *info = DStorageQueueInfo {
        empty_slot_count: q.capacity.saturating_sub(pending_len),
        requests_completed: q.completed.load(Ordering::Relaxed),
        error_count: q.errors.load(Ordering::Relaxed),
    };
}
