//! Offline deterministic Xbox HTTP / cURL networking shims:
//! - `xcurl.dll` (`curl_global_*`, `curl_easy_*`, `curl_multi_*`, `curl_slist_*`, mock registry)
//! - `libHttpClient.GDK.dll` (`HCInitialize`, `HCCleanup`, `HCHttpCall*`, mock registry)
//!
//! Real Xbox titles routinely probe telemetry, title-config, or auth endpoints at startup.
//! Providing deterministic offline responses (default HTTP 503 JSON, or custom mock rules)
//! prevents hangs and crashes without opening unauthorized network connections.

use core::ffi::{c_char, c_void};
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString};
use std::sync::{Mutex, OnceLock};

use xsx_abi::com::{HResult, E_INVALIDARG, S_OK};
use xsx_abi::gdk::XAsyncBlock;

use crate::registry::ModuleRegistry;

pub const CURLE_OK: i32 = 0;
pub const CURLE_URL_MALFORMAT: i32 = 3;
pub const CURLE_WRITE_ERROR: i32 = 23;
pub const CURLE_BAD_FUNCTION_ARGUMENT: i32 = 43;

pub const CURLOPT_WRITEDATA: i32 = 10001;
pub const CURLOPT_URL: i32 = 10002;
pub const CURLOPT_POSTFIELDS: i32 = 10015;
pub const CURLOPT_USERAGENT: i32 = 10018;
pub const CURLOPT_HTTPHEADER: i32 = 10023;
pub const CURLOPT_CUSTOMREQUEST: i32 = 10036;
pub const CURLOPT_WRITEFUNCTION: i32 = 20011;
pub const CURLOPT_TIMEOUT: i32 = 13;
pub const CURLOPT_POST: i32 = 47;

pub const CURLINFO_RESPONSE_CODE: i32 = 0x20_0002;
pub const CURLINFO_SIZE_DOWNLOAD_T: i32 = 0x60_0008;
pub const CURLINFO_CONTENT_TYPE: i32 = 0x10_0012;

pub const CURLM_OK: i32 = 0;
pub const CURLM_BAD_HANDLE: i32 = 1;
pub const CURLM_BAD_EASY_HANDLE: i32 = 2;
pub const CURLMSG_DONE: i32 = 1;

pub type CurlWriteCallback = unsafe extern "win64" fn(
    ptr: *mut c_char,
    size: usize,
    nmemb: usize,
    userdata: *mut c_void,
) -> usize;

#[repr(C)]
pub struct CurlSlist {
    pub data: *mut c_char,
    pub next: *mut CurlSlist,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CurlMsg {
    pub msg: i32,
    pub _pad: u32,
    pub easy_handle: *mut c_void,
    pub result: usize,
}
unsafe impl Send for CurlMsg {}

pub fn register(reg: &mut ModuleRegistry) {
    // xcurl.dll
    for (name, addr) in [
        ("curl_global_init", hle_curl_global_init as *const () as usize),
        ("curl_global_cleanup", hle_curl_global_cleanup as *const () as usize),
        ("curl_easy_init", hle_curl_easy_init as *const () as usize),
        ("curl_easy_cleanup", hle_curl_easy_cleanup as *const () as usize),
        ("curl_easy_reset", hle_curl_easy_reset as *const () as usize),
        ("curl_easy_setopt", hle_curl_easy_setopt as *const () as usize),
        ("curl_easy_perform", hle_curl_easy_perform as *const () as usize),
        ("curl_easy_getinfo", hle_curl_easy_getinfo as *const () as usize),
        ("curl_easy_strerror", hle_curl_easy_strerror as *const () as usize),
        ("curl_slist_append", hle_curl_slist_append as *const () as usize),
        ("curl_slist_free_all", hle_curl_slist_free_all as *const () as usize),
        ("curl_version", hle_curl_version as *const () as usize),
        ("curl_multi_init", hle_curl_multi_init as *const () as usize),
        ("curl_multi_add_handle", hle_curl_multi_add_handle as *const () as usize),
        ("curl_multi_remove_handle", hle_curl_multi_remove_handle as *const () as usize),
        ("curl_multi_perform", hle_curl_multi_perform as *const () as usize),
        ("curl_multi_info_read", hle_curl_multi_info_read as *const () as usize),
        ("curl_multi_poll", hle_curl_multi_poll as *const () as usize),
        ("curl_multi_cleanup", hle_curl_multi_cleanup as *const () as usize),
        ("xcurl_register_mock_response", hle_xcurl_register_mock as *const () as usize),
        ("xcurl_clear_mocks", hle_xcurl_clear_mocks as *const () as usize),
    ] {
        reg.register_export("xcurl.dll", name, addr);
    }

    // libHttpClient.GDK.dll
    for (name, addr) in [
        ("HCInitialize", hle_hc_initialize as *const () as usize),
        ("HCCleanup", hle_hc_cleanup as *const () as usize),
        ("HCIsInitialized", hle_hc_is_initialized as *const () as usize),
        ("HCGetLibVersion", hle_hc_get_lib_version as *const () as usize),
        ("HCHttpCallCreate", hle_hc_call_create as *const () as usize),
        ("HCHttpCallCloseHandle", hle_hc_call_close as *const () as usize),
        ("HCHttpCallRequestSetUrl", hle_hc_request_set_url as *const () as usize),
        ("HCHttpCallRequestSetHeader", hle_hc_request_set_header as *const () as usize),
        ("HCHttpCallRequestSetRequestBodyBytes", hle_hc_request_set_body as *const () as usize),
        ("HCHttpCallRequestSetTimeout", hle_hc_request_set_timeout as *const () as usize),
        ("HCHttpCallPerformAsync", hle_hc_call_perform_async as *const () as usize),
        ("HCHttpCallResponseGetStatusCode", hle_hc_response_get_status as *const () as usize),
        ("HCHttpCallResponseGetNetworkErrorCode", hle_hc_response_get_net_err as *const () as usize),
        ("HCHttpCallResponseGetResponseString", hle_hc_response_get_string as *const () as usize),
        ("HCHttpCallResponseGetResponseBodyBytesSize", hle_hc_response_get_body_size as *const () as usize),
        ("HCHttpCallResponseGetResponseBodyBytes", hle_hc_response_get_body as *const () as usize),
        ("HCHttpCallResponseGetHeader", hle_hc_response_get_header as *const () as usize),
        ("HCHttpCallRegisterMock", hle_hc_register_mock as *const () as usize),
        ("HCHttpCallClearMocks", hle_hc_clear_mocks as *const () as usize),
    ] {
        reg.register_export("libhttpclient.gdk.dll", name, addr);
    }
}

// ===========================================================================
// 1. xcurl.dll implementation
// ===========================================================================

#[derive(Default)]
struct EasyHandle {
    url: String,
    method: String,
    user_agent: String,
    post_data: Vec<u8>,
    write_cb: Option<CurlWriteCallback>,
    write_data: usize,
    timeout_sec: i64,
    response_code: i64,
    downloaded_bytes: i64,
    content_type_cstr: Option<CString>,
}

struct CurlMock {
    prefix: String,
    status: i64,
    content_type: String,
    body: Vec<u8>,
}

struct MultiHandle {
    attached: Vec<usize>,
    pending: Vec<usize>,
    done: VecDeque<CurlMsg>,
    current: Box<CurlMsg>,
}

struct CurlGlobal {
    next_id: usize,
    inited: bool,
    easy: HashMap<usize, EasyHandle>,
    multi: HashMap<usize, MultiHandle>,
    mocks: Vec<CurlMock>,
}

fn curl_global() -> &'static Mutex<CurlGlobal> {
    static C: OnceLock<Mutex<CurlGlobal>> = OnceLock::new();
    C.get_or_init(|| {
        Mutex::new(CurlGlobal {
            next_id: 0xC000_0010,
            inited: false,
            easy: HashMap::new(),
            multi: HashMap::new(),
            mocks: Vec::new(),
        })
    })
}

static CURL_VERSION_STR: &[u8] = b"xsxemu-xcurl/8.4.0-offline\0";
static CURL_OK_STR: &[u8] = b"No error\0";
static CURL_ERR_STR: &[u8] = b"xcurl error\0";

pub unsafe extern "win64" fn hle_curl_global_init(_flags: i64) -> i32 {
    curl_global().lock().unwrap().inited = true;
    CURLE_OK
}

pub unsafe extern "win64" fn hle_curl_global_cleanup() {
    let mut g = curl_global().lock().unwrap();
    g.easy.clear();
    g.multi.clear();
    g.inited = false;
}

pub unsafe extern "win64" fn hle_curl_easy_init() -> *mut c_void {
    let mut g = curl_global().lock().unwrap();
    let id = g.next_id;
    g.next_id += 0x10;
    g.easy.insert(
        id,
        EasyHandle {
            method: "GET".into(),
            timeout_sec: 30,
            ..Default::default()
        },
    );
    id as *mut c_void
}

pub unsafe extern "win64" fn hle_curl_easy_cleanup(curl: *mut c_void) {
    if curl.is_null() {
        return;
    }
    let id = curl as usize;
    let mut g = curl_global().lock().unwrap();
    g.easy.remove(&id);
    for m in g.multi.values_mut() {
        m.attached.retain(|&x| x != id);
        m.pending.retain(|&x| x != id);
        m.done.retain(|msg| msg.easy_handle as usize != id);
    }
}

pub unsafe extern "win64" fn hle_curl_easy_reset(curl: *mut c_void) {
    if curl.is_null() {
        return;
    }
    let mut g = curl_global().lock().unwrap();
    if let Some(h) = g.easy.get_mut(&(curl as usize)) {
        *h = EasyHandle {
            method: "GET".into(),
            timeout_sec: 30,
            ..Default::default()
        };
    }
}

/// On Win64 x86_64 ABI, variadic 3rd argument arrives directly in `r8`.
pub unsafe extern "win64" fn hle_curl_easy_setopt(
    curl: *mut c_void,
    option: i32,
    arg: usize,
) -> i32 {
    if curl.is_null() {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    }
    let mut g = curl_global().lock().unwrap();
    let Some(h) = g.easy.get_mut(&(curl as usize)) else {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    };
    match option {
        CURLOPT_URL => {
            let p = arg as *const c_char;
            h.url = if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            };
        }
        CURLOPT_CUSTOMREQUEST => {
            let p = arg as *const c_char;
            if !p.is_null() {
                h.method = CStr::from_ptr(p).to_string_lossy().into_owned();
            }
        }
        CURLOPT_USERAGENT => {
            let p = arg as *const c_char;
            if !p.is_null() {
                h.user_agent = CStr::from_ptr(p).to_string_lossy().into_owned();
            }
        }
        CURLOPT_POST => {
            if arg != 0 {
                h.method = "POST".into();
            }
        }
        CURLOPT_POSTFIELDS => {
            let p = arg as *const c_char;
            if !p.is_null() {
                h.post_data = CStr::from_ptr(p).to_bytes().to_vec();
            }
        }
        CURLOPT_WRITEFUNCTION => {
            h.write_cb = if arg == 0 {
                None
            } else {
                Some(core::mem::transmute::<usize, CurlWriteCallback>(arg))
            };
        }
        CURLOPT_WRITEDATA => {
            h.write_data = arg;
        }
        CURLOPT_TIMEOUT => {
            h.timeout_sec = arg as i64;
        }
        CURLOPT_HTTPHEADER | _ => {}
    }
    CURLE_OK
}

pub unsafe extern "win64" fn hle_curl_easy_perform(curl: *mut c_void) -> i32 {
    if curl.is_null() {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    }
    let (cb, ud, mut body) = {
        let mut g = curl_global().lock().unwrap();
        let url = match g.easy.get(&(curl as usize)) {
            Some(h) if !h.url.is_empty() => h.url.clone(),
            Some(_) => return CURLE_URL_MALFORMAT,
            None => return CURLE_BAD_FUNCTION_ARGUMENT,
        };
        let matched = g
            .mocks
            .iter()
            .find(|m| url.starts_with(&m.prefix))
            .map(|m| (m.status, m.content_type.clone(), m.body.clone()));
        let (status, ct, body) = matched.unwrap_or_else(|| {
            (
                503,
                "application/json".to_string(),
                br#"{"status":"offline","emulator":"xsxemu-xcurl"}"#.to_vec(),
            )
        });
        let h = g.easy.get_mut(&(curl as usize)).unwrap();
        h.response_code = status;
        h.downloaded_bytes = body.len() as i64;
        h.content_type_cstr = CString::new(ct).ok();
        (h.write_cb, h.write_data, body)
    };

    if let Some(write_fn) = cb {
        if !body.is_empty() {
            let written = write_fn(
                body.as_mut_ptr() as *mut c_char,
                1,
                body.len(),
                ud as *mut c_void,
            );
            if written != body.len() {
                return CURLE_WRITE_ERROR;
            }
        }
    }
    CURLE_OK
}

pub unsafe extern "win64" fn hle_curl_easy_getinfo(
    curl: *mut c_void,
    info: i32,
    out_ptr: usize,
) -> i32 {
    if curl.is_null() || out_ptr == 0 {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    }
    let g = curl_global().lock().unwrap();
    let Some(h) = g.easy.get(&(curl as usize)) else {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    };
    match info {
        CURLINFO_RESPONSE_CODE => {
            *(out_ptr as *mut i64) = h.response_code;
            CURLE_OK
        }
        CURLINFO_SIZE_DOWNLOAD_T => {
            *(out_ptr as *mut i64) = h.downloaded_bytes;
            CURLE_OK
        }
        CURLINFO_CONTENT_TYPE => {
            let ptr = h
                .content_type_cstr
                .as_ref()
                .map(|c| c.as_ptr())
                .unwrap_or(core::ptr::null());
            *(out_ptr as *mut *const c_char) = ptr;
            CURLE_OK
        }
        _ => CURLE_BAD_FUNCTION_ARGUMENT,
    }
}

pub unsafe extern "win64" fn hle_curl_easy_strerror(code: i32) -> *const c_char {
    if code == CURLE_OK {
        CURL_OK_STR.as_ptr() as *const c_char
    } else {
        CURL_ERR_STR.as_ptr() as *const c_char
    }
}

pub unsafe extern "win64" fn hle_curl_slist_append(
    list: *mut CurlSlist,
    string: *const c_char,
) -> *mut CurlSlist {
    if string.is_null() {
        return list;
    }
    let Ok(copy) = CString::new(CStr::from_ptr(string).to_bytes()) else {
        return list;
    };
    let node = Box::into_raw(Box::new(CurlSlist {
        data: copy.into_raw(),
        next: core::ptr::null_mut(),
    }));
    if list.is_null() {
        return node;
    }
    let mut cur = list;
    while !(*cur).next.is_null() {
        cur = (*cur).next;
    }
    (*cur).next = node;
    list
}

pub unsafe extern "win64" fn hle_curl_slist_free_all(mut list: *mut CurlSlist) {
    while !list.is_null() {
        let next = (*list).next;
        if !(*list).data.is_null() {
            drop(CString::from_raw((*list).data));
        }
        drop(Box::from_raw(list));
        list = next;
    }
}

pub unsafe extern "win64" fn hle_curl_version() -> *const c_char {
    CURL_VERSION_STR.as_ptr() as *const c_char
}

pub unsafe extern "win64" fn hle_curl_multi_init() -> *mut c_void {
    let mut g = curl_global().lock().unwrap();
    let id = g.next_id;
    g.next_id += 0x10;
    g.multi.insert(
        id,
        MultiHandle {
            attached: Vec::new(),
            pending: Vec::new(),
            done: VecDeque::new(),
            current: Box::new(CurlMsg {
                msg: 0,
                _pad: 0,
                easy_handle: core::ptr::null_mut(),
                result: 0,
            }),
        },
    );
    id as *mut c_void
}

pub unsafe extern "win64" fn hle_curl_multi_add_handle(
    multi: *mut c_void,
    easy: *mut c_void,
) -> i32 {
    if multi.is_null() {
        return CURLM_BAD_HANDLE;
    }
    if easy.is_null() {
        return CURLM_BAD_EASY_HANDLE;
    }
    let mut g = curl_global().lock().unwrap();
    if !g.easy.contains_key(&(easy as usize)) {
        return CURLM_BAD_EASY_HANDLE;
    }
    let Some(m) = g.multi.get_mut(&(multi as usize)) else {
        return CURLM_BAD_HANDLE;
    };
    let eid = easy as usize;
    if !m.attached.contains(&eid) {
        m.attached.push(eid);
        m.pending.push(eid);
    }
    CURLM_OK
}

pub unsafe extern "win64" fn hle_curl_multi_remove_handle(
    multi: *mut c_void,
    easy: *mut c_void,
) -> i32 {
    if multi.is_null() {
        return CURLM_BAD_HANDLE;
    }
    let mut g = curl_global().lock().unwrap();
    let Some(m) = g.multi.get_mut(&(multi as usize)) else {
        return CURLM_BAD_HANDLE;
    };
    let eid = easy as usize;
    m.attached.retain(|&x| x != eid);
    m.pending.retain(|&x| x != eid);
    m.done.retain(|msg| msg.easy_handle as usize != eid);
    CURLM_OK
}

pub unsafe extern "win64" fn hle_curl_multi_perform(
    multi: *mut c_void,
    running_handles: *mut i32,
) -> i32 {
    if multi.is_null() {
        return CURLM_BAD_HANDLE;
    }
    let pending: Vec<usize> = {
        let mut g = curl_global().lock().unwrap();
        let Some(m) = g.multi.get_mut(&(multi as usize)) else {
            return CURLM_BAD_HANDLE;
        };
        core::mem::take(&mut m.pending)
    };
    let mut finished = Vec::with_capacity(pending.len());
    for eid in pending {
        let rc = hle_curl_easy_perform(eid as *mut c_void);
        finished.push(CurlMsg {
            msg: CURLMSG_DONE,
            _pad: 0,
            easy_handle: eid as *mut c_void,
            result: rc as usize,
        });
    }
    {
        let mut g = curl_global().lock().unwrap();
        if let Some(m) = g.multi.get_mut(&(multi as usize)) {
            m.done.extend(finished);
        }
    }
    if !running_handles.is_null() {
        *running_handles = 0;
    }
    CURLM_OK
}

pub unsafe extern "win64" fn hle_curl_multi_info_read(
    multi: *mut c_void,
    msgs_in_queue: *mut i32,
) -> *mut CurlMsg {
    if !msgs_in_queue.is_null() {
        *msgs_in_queue = 0;
    }
    if multi.is_null() {
        return core::ptr::null_mut();
    }
    let mut g = curl_global().lock().unwrap();
    let Some(m) = g.multi.get_mut(&(multi as usize)) else {
        return core::ptr::null_mut();
    };
    let Some(msg) = m.done.pop_front() else {
        return core::ptr::null_mut();
    };
    *m.current = msg;
    if !msgs_in_queue.is_null() {
        *msgs_in_queue = m.done.len() as i32;
    }
    &mut *m.current as *mut CurlMsg
}

pub unsafe extern "win64" fn hle_curl_multi_poll(
    _multi: *mut c_void,
    _extra_fds: *mut c_void,
    _extra_nfds: u32,
    _timeout_ms: i32,
    numfds: *mut i32,
) -> i32 {
    if !numfds.is_null() {
        *numfds = 0;
    }
    CURLM_OK
}

pub unsafe extern "win64" fn hle_curl_multi_cleanup(multi: *mut c_void) -> i32 {
    if multi.is_null() {
        return CURLM_BAD_HANDLE;
    }
    curl_global().lock().unwrap().multi.remove(&(multi as usize));
    CURLM_OK
}

pub unsafe extern "win64" fn hle_xcurl_register_mock(
    url_prefix: *const c_char,
    response_code: i64,
    content_type: *const c_char,
    body: *const u8,
    body_size: usize,
) -> i32 {
    if url_prefix.is_null() {
        return CURLE_BAD_FUNCTION_ARGUMENT;
    }
    let prefix = CStr::from_ptr(url_prefix).to_string_lossy().into_owned();
    let ct = if content_type.is_null() {
        "application/json".to_string()
    } else {
        CStr::from_ptr(content_type).to_string_lossy().into_owned()
    };
    let data = if !body.is_null() && body_size > 0 {
        core::slice::from_raw_parts(body, body_size).to_vec()
    } else {
        Vec::new()
    };
    curl_global().lock().unwrap().mocks.push(CurlMock {
        prefix,
        status: response_code,
        content_type: ct,
        body: data,
    });
    CURLE_OK
}

pub unsafe extern "win64" fn hle_xcurl_clear_mocks() {
    curl_global().lock().unwrap().mocks.clear();
}

// ===========================================================================
// 2. libHttpClient.GDK.dll implementation
// ===========================================================================

struct HcCall {
    method: String,
    url: String,
    req_headers: HashMap<String, String>,
    req_body: Vec<u8>,
    timeout_sec: u32,
    status_code: u32,
    net_err: HResult,
    resp_body: Vec<u8>,
    resp_cstr: CString,
    resp_headers: HashMap<String, CString>,
}

struct HcMock {
    method: String,
    prefix: String,
    status_code: u32,
    body: Vec<u8>,
    content_type: String,
}

struct HcGlobal {
    next_id: usize,
    inited: bool,
    calls: HashMap<usize, HcCall>,
    mocks: Vec<HcMock>,
}

fn hc_global() -> &'static Mutex<HcGlobal> {
    static H: OnceLock<Mutex<HcGlobal>> = OnceLock::new();
    H.get_or_init(|| {
        Mutex::new(HcGlobal {
            next_id: 0xD000_0010,
            inited: false,
            calls: HashMap::new(),
            mocks: Vec::new(),
        })
    })
}

static HC_VER: &[u8] = b"xsxemu-libHttpClient-GDK-1.0\0";

pub unsafe extern "win64" fn hle_hc_initialize(_args: *mut c_void) -> HResult {
    hc_global().lock().unwrap().inited = true;
    S_OK
}

pub unsafe extern "win64" fn hle_hc_cleanup() {
    let mut g = hc_global().lock().unwrap();
    g.calls.clear();
    g.inited = false;
}

pub unsafe extern "win64" fn hle_hc_is_initialized() -> u32 {
    u32::from(hc_global().lock().unwrap().inited)
}

pub unsafe extern "win64" fn hle_hc_get_lib_version(out: *mut *const c_char) -> HResult {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = HC_VER.as_ptr() as *const c_char;
    S_OK
}

pub unsafe extern "win64" fn hle_hc_call_create(out: *mut *mut c_void) -> HResult {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let mut g = hc_global().lock().unwrap();
    let id = g.next_id;
    g.next_id += 0x10;
    g.calls.insert(
        id,
        HcCall {
            method: "GET".into(),
            url: String::new(),
            req_headers: HashMap::new(),
            req_body: Vec::new(),
            timeout_sec: 30,
            status_code: 0,
            net_err: S_OK,
            resp_body: Vec::new(),
            resp_cstr: CString::new("").unwrap(),
            resp_headers: HashMap::new(),
        },
    );
    *out = id as *mut c_void;
    S_OK
}

pub unsafe extern "win64" fn hle_hc_call_close(call: *mut c_void) -> HResult {
    if call.is_null() {
        return E_INVALIDARG;
    }
    if hc_global().lock().unwrap().calls.remove(&(call as usize)).is_some() {
        S_OK
    } else {
        E_INVALIDARG
    }
}

pub unsafe extern "win64" fn hle_hc_request_set_url(
    call: *mut c_void,
    method: *const c_char,
    url: *const c_char,
) -> HResult {
    if call.is_null() || method.is_null() || url.is_null() {
        return E_INVALIDARG;
    }
    let mut g = hc_global().lock().unwrap();
    let Some(c) = g.calls.get_mut(&(call as usize)) else {
        return E_INVALIDARG;
    };
    c.method = CStr::from_ptr(method).to_string_lossy().into_owned();
    c.url = CStr::from_ptr(url).to_string_lossy().into_owned();
    S_OK
}

pub unsafe extern "win64" fn hle_hc_request_set_header(
    call: *mut c_void,
    name: *const c_char,
    value: *const c_char,
    _allow_tracing: u32,
) -> HResult {
    if call.is_null() || name.is_null() || value.is_null() {
        return E_INVALIDARG;
    }
    let mut g = hc_global().lock().unwrap();
    let Some(c) = g.calls.get_mut(&(call as usize)) else {
        return E_INVALIDARG;
    };
    c.req_headers.insert(
        CStr::from_ptr(name).to_string_lossy().into_owned(),
        CStr::from_ptr(value).to_string_lossy().into_owned(),
    );
    S_OK
}

pub unsafe extern "win64" fn hle_hc_request_set_body(
    call: *mut c_void,
    bytes: *const u8,
    size: u32,
) -> HResult {
    if call.is_null() || (bytes.is_null() && size > 0) {
        return E_INVALIDARG;
    }
    let mut g = hc_global().lock().unwrap();
    let Some(c) = g.calls.get_mut(&(call as usize)) else {
        return E_INVALIDARG;
    };
    c.req_body = if size > 0 {
        core::slice::from_raw_parts(bytes, size as usize).to_vec()
    } else {
        Vec::new()
    };
    S_OK
}

pub unsafe extern "win64" fn hle_hc_request_set_timeout(call: *mut c_void, secs: u32) -> HResult {
    if call.is_null() {
        return E_INVALIDARG;
    }
    let mut g = hc_global().lock().unwrap();
    let Some(c) = g.calls.get_mut(&(call as usize)) else {
        return E_INVALIDARG;
    };
    c.timeout_sec = secs;
    S_OK
}

pub unsafe extern "win64" fn hle_hc_call_perform_async(
    call: *mut c_void,
    async_block: *mut XAsyncBlock,
) -> HResult {
    if call.is_null() {
        return E_INVALIDARG;
    }
    {
        let mut g = hc_global().lock().unwrap();
        let (method, url) = match g.calls.get(&(call as usize)) {
            Some(c) => (c.method.clone(), c.url.clone()),
            None => return E_INVALIDARG,
        };
        let matched = g
            .mocks
            .iter()
            .find(|r| {
                (r.method.is_empty() || r.method == "*" || r.method == method)
                    && url.starts_with(&r.prefix)
            })
            .map(|r| (r.status_code, r.body.clone(), r.content_type.clone()));
        let (status, body, ct) = matched.unwrap_or_else(|| {
            (
                503,
                br#"{"status":"offline","emulator":"xsxemu-hc"}"#.to_vec(),
                "application/json".to_string(),
            )
        });
        let c = g.calls.get_mut(&(call as usize)).unwrap();
        c.status_code = status;
        c.net_err = S_OK;
        c.resp_body = body.clone();
        let clean_str: Vec<u8> = body.into_iter().filter(|&b| b != 0).collect();
        c.resp_cstr = CString::new(clean_str).unwrap_or_else(|_| CString::new("").unwrap());
        c.resp_headers.insert(
            "Content-Type".into(),
            CString::new(ct).unwrap_or_else(|_| CString::new("application/json").unwrap()),
        );
        c.resp_headers
            .insert("X-XsxEmu-Offline".into(), CString::new("1").unwrap());
    }
    if !async_block.is_null() {
        if let Some(cb) = (*async_block).callback {
            cb(async_block);
        }
    }
    S_OK
}

pub unsafe extern "win64" fn hle_hc_response_get_status(
    call: *mut c_void,
    out: *mut u32,
) -> HResult {
    if call.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let g = hc_global().lock().unwrap();
    let Some(c) = g.calls.get(&(call as usize)) else {
        return E_INVALIDARG;
    };
    *out = c.status_code;
    S_OK
}

pub unsafe extern "win64" fn hle_hc_response_get_net_err(
    call: *mut c_void,
    net_err: *mut HResult,
    plat_err: *mut u32,
) -> HResult {
    if call.is_null() || net_err.is_null() {
        return E_INVALIDARG;
    }
    let g = hc_global().lock().unwrap();
    let Some(c) = g.calls.get(&(call as usize)) else {
        return E_INVALIDARG;
    };
    *net_err = c.net_err;
    if !plat_err.is_null() {
        *plat_err = 0;
    }
    S_OK
}

pub unsafe extern "win64" fn hle_hc_response_get_string(
    call: *mut c_void,
    out: *mut *const c_char,
) -> HResult {
    if call.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let g = hc_global().lock().unwrap();
    let Some(c) = g.calls.get(&(call as usize)) else {
        return E_INVALIDARG;
    };
    *out = c.resp_cstr.as_ptr();
    S_OK
}

pub unsafe extern "win64" fn hle_hc_response_get_body_size(
    call: *mut c_void,
    out: *mut usize,
) -> HResult {
    if call.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let g = hc_global().lock().unwrap();
    let Some(c) = g.calls.get(&(call as usize)) else {
        return E_INVALIDARG;
    };
    *out = c.resp_body.len();
    S_OK
}

pub unsafe extern "win64" fn hle_hc_response_get_body(
    call: *mut c_void,
    cap: usize,
    buf: *mut u8,
    used: *mut usize,
) -> HResult {
    if call.is_null() || (buf.is_null() && cap > 0) {
        return E_INVALIDARG;
    }
    let g = hc_global().lock().unwrap();
    let Some(c) = g.calls.get(&(call as usize)) else {
        return E_INVALIDARG;
    };
    let n = cap.min(c.resp_body.len());
    if n > 0 {
        core::ptr::copy_nonoverlapping(c.resp_body.as_ptr(), buf, n);
    }
    if !used.is_null() {
        *used = n;
    }
    S_OK
}

pub unsafe extern "win64" fn hle_hc_response_get_header(
    call: *mut c_void,
    name: *const c_char,
    out: *mut *const c_char,
) -> HResult {
    if call.is_null() || name.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let key = CStr::from_ptr(name).to_string_lossy();
    let g = hc_global().lock().unwrap();
    let Some(c) = g.calls.get(&(call as usize)) else {
        return E_INVALIDARG;
    };
    *out = c
        .resp_headers
        .get(key.as_ref())
        .map(|v| v.as_ptr())
        .unwrap_or(core::ptr::null());
    S_OK
}

pub unsafe extern "win64" fn hle_hc_register_mock(
    method: *const c_char,
    url_prefix: *const c_char,
    status_code: u32,
    body: *const u8,
    body_size: u32,
    content_type: *const c_char,
) -> HResult {
    if url_prefix.is_null() {
        return E_INVALIDARG;
    }
    hc_global().lock().unwrap().mocks.push(HcMock {
        method: if method.is_null() {
            "*".into()
        } else {
            CStr::from_ptr(method).to_string_lossy().into_owned()
        },
        prefix: CStr::from_ptr(url_prefix).to_string_lossy().into_owned(),
        status_code,
        body: if !body.is_null() && body_size > 0 {
            core::slice::from_raw_parts(body, body_size as usize).to_vec()
        } else {
            Vec::new()
        },
        content_type: if content_type.is_null() {
            "application/json".into()
        } else {
            CStr::from_ptr(content_type).to_string_lossy().into_owned()
        },
    });
    S_OK
}

pub unsafe extern "win64" fn hle_hc_clear_mocks() {
    hc_global().lock().unwrap().mocks.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xcurl_and_libhttpclient_offline_mock_and_fallback() {
        unsafe {
            assert_eq!(hle_curl_global_init(0), CURLE_OK);
            let easy = hle_curl_easy_init();
            assert!(!easy.is_null());
            let url = b"https://title.xboxlive.com/config\0";
            assert_eq!(
                hle_curl_easy_setopt(easy, CURLOPT_URL, url.as_ptr() as usize),
                CURLE_OK
            );
            assert_eq!(hle_curl_easy_perform(easy), CURLE_OK);
            let mut status = 0i64;
            assert_eq!(
                hle_curl_easy_getinfo(easy, CURLINFO_RESPONSE_CODE, &mut status as *mut i64 as usize),
                CURLE_OK
            );
            assert_eq!(status, 503);

            // Register mock and execute via curl_multi
            let body = b"{\"ok\":true}";
            assert_eq!(
                hle_xcurl_register_mock(
                    b"https://title.xboxlive.com/\0".as_ptr() as *const c_char,
                    200,
                    b"application/json\0".as_ptr() as *const c_char,
                    body.as_ptr(),
                    body.len()
                ),
                CURLE_OK
            );
            let multi = hle_curl_multi_init();
            assert_eq!(hle_curl_multi_add_handle(multi, easy), CURLM_OK);
            let mut running = 1;
            assert_eq!(hle_curl_multi_perform(multi, &mut running), CURLM_OK);
            assert_eq!(running, 0);
            let mut left = 0;
            let msg = hle_curl_multi_info_read(multi, &mut left);
            assert!(!msg.is_null());
            assert_eq!((*msg).msg, CURLMSG_DONE);
            assert_eq!(
                hle_curl_easy_getinfo(easy, CURLINFO_RESPONSE_CODE, &mut status as *mut i64 as usize),
                CURLE_OK
            );
            assert_eq!(status, 200);
            assert_eq!(hle_curl_multi_cleanup(multi), CURLM_OK);
            hle_curl_easy_cleanup(easy);
            hle_xcurl_clear_mocks();

            // libHttpClient.GDK
            assert_eq!(hle_hc_initialize(core::ptr::null_mut()), S_OK);
            assert_eq!(hle_hc_is_initialized(), 1);
            let mut call: *mut c_void = core::ptr::null_mut();
            assert_eq!(hle_hc_call_create(&mut call), S_OK);
            assert_eq!(
                hle_hc_request_set_url(
                    call,
                    b"GET\0".as_ptr() as *const c_char,
                    b"https://auth.xboxlive.com/token\0".as_ptr() as *const c_char
                ),
                S_OK
            );
            assert_eq!(hle_hc_call_perform_async(call, core::ptr::null_mut()), S_OK);
            let mut hc_status = 0u32;
            assert_eq!(hle_hc_response_get_status(call, &mut hc_status), S_OK);
            assert_eq!(hc_status, 503);
            assert_eq!(hle_hc_call_close(call), S_OK);
            hle_hc_cleanup();
        }
    }
}
