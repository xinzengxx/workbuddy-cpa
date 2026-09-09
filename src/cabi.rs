use std::ffi::{c_char, c_void, CStr};
use std::sync::OnceLock;

pub const ABI_VERSION: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CliproxyBuffer {
    pub ptr: *mut c_void,
    pub len: usize,
}

pub type HostCallFn = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *const u8,
    usize,
    *mut CliproxyBuffer,
) -> i32;
pub type HostFreeFn = unsafe extern "C" fn(*mut c_void, usize);
pub type PluginCallFn =
    unsafe extern "C" fn(*mut c_char, *mut u8, usize, *mut CliproxyBuffer) -> i32;
pub type PluginFreeFn = unsafe extern "C" fn(*mut c_void, usize);
pub type PluginShutdownFn = unsafe extern "C" fn();

#[repr(C)]
pub struct CliproxyHostApi {
    pub abi_version: u32,
    pub host_ctx: *mut c_void,
    pub call: Option<HostCallFn>,
    pub free_buffer: Option<HostFreeFn>,
}

#[repr(C)]
pub struct CliproxyPluginApi {
    pub abi_version: u32,
    pub call: Option<PluginCallFn>,
    pub free_buffer: Option<PluginFreeFn>,
    pub shutdown: Option<PluginShutdownFn>,
}

struct HostApi {
    ctx: *mut c_void,
    call: HostCallFn,
    free: HostFreeFn,
}
unsafe impl Send for HostApi {}
unsafe impl Sync for HostApi {}

static HOST: OnceLock<HostApi> = OnceLock::new();

fn host() -> Option<&'static HostApi> {
    HOST.get()
}

/// Invoke a host RPC method via the function-pointer table captured at init.
pub fn host_call(method: &str, request: &[u8]) -> Result<Vec<u8>, String> {
    let h = host().ok_or_else(|| "host API unavailable".to_string())?;
    let c_method = std::ffi::CString::new(method).map_err(|e| e.to_string())?;
    let mut resp = CliproxyBuffer {
        ptr: std::ptr::null_mut(),
        len: 0,
    };
    let (req_ptr, req_len) = if request.is_empty() {
        (std::ptr::null::<u8>(), 0usize)
    } else {
        (request.as_ptr(), request.len())
    };
    let rc = unsafe { (h.call)(h.ctx, c_method.as_ptr(), req_ptr, req_len, &mut resp) };
    let out = if resp.ptr.is_null() || resp.len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(resp.ptr as *const u8, resp.len) }.to_vec()
    };
    unsafe { (h.free)(resp.ptr, resp.len) };
    if rc != 0 {
        return Err(format!("host call {method} returned {rc}"));
    }
    Ok(out)
}

pub fn host_log(level: &str, message: &str) {
    let body = serde_json::json!({ "level": level, "message": message });
    let _ = host_call("host.log", body.to_string().as_bytes());
}

#[no_mangle]
pub extern "C" fn cliproxy_plugin_init(
    host: *const CliproxyHostApi,
    plugin: *mut CliproxyPluginApi,
) -> i32 {
    if plugin.is_null() {
        return 1;
    }
    unsafe {
        let h = &*host;
        if h.call.is_none() || h.free_buffer.is_none() {
            return 1;
        }
        let _ = HOST.set(HostApi {
            ctx: h.host_ctx,
            call: h.call.unwrap(),
            free: h.free_buffer.unwrap(),
        });
        (*plugin).abi_version = ABI_VERSION;
        (*plugin).call = Some(cliproxyPluginCall);
        (*plugin).free_buffer = Some(cliproxyPluginFree);
        (*plugin).shutdown = Some(cliproxyPluginShutdown);
    }
    0
}

#[no_mangle]
pub extern "C" fn cliproxyPluginCall(
    method: *mut c_char,
    request: *mut u8,
    request_len: usize,
    response: *mut CliproxyBuffer,
) -> i32 {
    if !response.is_null() {
        unsafe {
            (*response).ptr = std::ptr::null_mut();
            (*response).len = 0;
        }
    }
    if method.is_null() {
        write_response(
            response,
            crate::rpc::error_envelope("invalid_method", "method is required").as_bytes(),
        );
        return 1;
    }
    let m = unsafe { CStr::from_ptr(method as *const c_char) }
        .to_string_lossy()
        .into_owned();
    let req: &[u8] = if request.is_null() || request_len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(request, request_len) }
    };
    match crate::dispatch::handle(&m, req) {
        Ok(raw) => {
            write_response(response, &raw);
            0
        }
        Err(e) => {
            write_response(
                response,
                crate::rpc::error_envelope("plugin_error", &e).as_bytes(),
            );
            1
        }
    }
}

#[no_mangle]
pub extern "C" fn cliproxyPluginFree(ptr: *mut c_void, len: usize) {
    if !ptr.is_null() {
        unsafe { drop(Vec::from_raw_parts(ptr as *mut u8, len, len)) };
    }
}

#[no_mangle]
pub extern "C" fn cliproxyPluginShutdown() {}

/// Hand a byte buffer to the host. The buffer comes from a Vec whose length
/// must match exactly: the host frees it later with cliproxyPluginFree.
fn write_response(response: *mut CliproxyBuffer, bytes: &[u8]) {
    if response.is_null() || bytes.is_empty() {
        return;
    }
    let mut v = bytes.to_vec();
    let ptr = v.as_mut_ptr() as *mut c_void;
    let len = v.len();
    std::mem::forget(v);
    unsafe {
        (*response).ptr = ptr;
        (*response).len = len;
    }
}
