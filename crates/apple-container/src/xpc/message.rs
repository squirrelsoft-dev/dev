use std::ffi::{CStr, CString};
use std::os::fd::RawFd;

use crate::error::AppleContainerError;
use crate::routes::ROUTE_KEY;
use crate::xpc::ffi;

/// A wrapper around an XPC dictionary object used for request/reply messages.
pub struct XpcMessage {
    inner: ffi::xpc_object_t,
}

impl XpcMessage {
    /// Create a new empty XPC dictionary message.
    pub fn new() -> Self {
        let inner = unsafe { ffi::xpc_dictionary_create(std::ptr::null(), std::ptr::null(), 0) };
        Self { inner }
    }

    /// Create a new message with the route key pre-set.
    pub fn with_route(route: &str) -> Self {
        let msg = Self::new();
        msg.set_string(ROUTE_KEY, route);
        msg
    }

    /// Get the raw XPC object pointer.
    pub fn as_ptr(&self) -> ffi::xpc_object_t {
        self.inner
    }

    /// Adopt an existing XPC object pointer. The message takes ownership.
    ///
    /// # Safety
    /// `ptr` must be a valid XPC dictionary object. Caller transfers ownership.
    pub unsafe fn from_raw(ptr: ffi::xpc_object_t) -> Self {
        Self { inner: ptr }
    }

    // -- Setters --

    pub fn set_string(&self, key: &str, val: &str) {
        let c_key = CString::new(key).expect("key contains null byte");
        let c_val = CString::new(val).expect("value contains null byte");
        unsafe { ffi::xpc_dictionary_set_string(self.inner, c_key.as_ptr(), c_val.as_ptr()) };
    }

    pub fn set_data(&self, key: &str, val: &[u8]) {
        let c_key = CString::new(key).expect("key contains null byte");
        unsafe { ffi::xpc_dictionary_set_data(self.inner, c_key.as_ptr(), val.as_ptr(), val.len()) };
    }

    pub fn set_bool(&self, key: &str, val: bool) {
        let c_key = CString::new(key).expect("key contains null byte");
        unsafe { ffi::xpc_dictionary_set_bool(self.inner, c_key.as_ptr(), val) };
    }

    pub fn set_int64(&self, key: &str, val: i64) {
        let c_key = CString::new(key).expect("key contains null byte");
        unsafe { ffi::xpc_dictionary_set_int64(self.inner, c_key.as_ptr(), val) };
    }

    pub fn set_uint64(&self, key: &str, val: u64) {
        let c_key = CString::new(key).expect("key contains null byte");
        unsafe { ffi::xpc_dictionary_set_uint64(self.inner, c_key.as_ptr(), val) };
    }

    pub fn set_fd(&self, key: &str, fd: RawFd) {
        let c_key = CString::new(key).expect("key contains null byte");
        unsafe { ffi::xpc_dictionary_set_fd(self.inner, c_key.as_ptr(), fd) };
    }

    // -- Getters --

    pub fn get_string(&self, key: &str) -> Option<String> {
        let c_key = CString::new(key).expect("key contains null byte");
        let ptr = unsafe { ffi::xpc_dictionary_get_string(self.inner, c_key.as_ptr()) };
        if ptr.is_null() {
            return None;
        }
        let cstr = unsafe { CStr::from_ptr(ptr) };
        Some(cstr.to_string_lossy().into_owned())
    }

    pub fn get_data(&self, key: &str) -> Option<Vec<u8>> {
        let c_key = CString::new(key).expect("key contains null byte");
        let mut len: usize = 0;
        let ptr = unsafe { ffi::xpc_dictionary_get_data(self.inner, c_key.as_ptr(), &mut len) };
        if ptr.is_null() || len == 0 {
            return None;
        }
        let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
        Some(slice.to_vec())
    }

    pub fn get_bool(&self, key: &str) -> bool {
        let c_key = CString::new(key).expect("key contains null byte");
        unsafe { ffi::xpc_dictionary_get_bool(self.inner, c_key.as_ptr()) }
    }

    pub fn get_int64(&self, key: &str) -> i64 {
        let c_key = CString::new(key).expect("key contains null byte");
        unsafe { ffi::xpc_dictionary_get_int64(self.inner, c_key.as_ptr()) }
    }

    pub fn get_uint64(&self, key: &str) -> u64 {
        let c_key = CString::new(key).expect("key contains null byte");
        unsafe { ffi::xpc_dictionary_get_uint64(self.inner, c_key.as_ptr()) }
    }

    pub fn dup_fd(&self, key: &str) -> Option<RawFd> {
        let c_key = CString::new(key).expect("key contains null byte");
        let fd = unsafe { ffi::xpc_dictionary_dup_fd(self.inner, c_key.as_ptr()) };
        if fd < 0 { None } else { Some(fd) }
    }

    /// Check if the value for a key exists (is non-null).
    pub fn has_key(&self, key: &str) -> bool {
        let c_key = CString::new(key).expect("key contains null byte");
        let val = unsafe { ffi::xpc_dictionary_get_value(self.inner, c_key.as_ptr()) };
        !val.is_null()
    }

    /// Check if this message is an XPC error object.
    pub fn is_error(&self) -> bool {
        unsafe { ffi::xpc_object_is_error(self.inner) }
    }

    /// Check the reply for XPC-level errors or application-level error keys.
    pub fn check_error(&self) -> Result<(), AppleContainerError> {
        if self.is_error() {
            return Err(AppleContainerError::XpcError(
                "XPC returned an error object".to_string(),
            ));
        }
        if let Some(err) = self.get_string(crate::routes::ERROR_KEY) {
            return Err(AppleContainerError::XpcError(err));
        }
        Ok(())
    }
}

impl Drop for XpcMessage {
    fn drop(&mut self) {
        if !self.inner.is_null() && !unsafe { ffi::xpc_object_is_error(self.inner) } {
            unsafe { ffi::xpc_release(self.inner) };
        }
    }
}

// xpc_object_t is thread-safe.
unsafe impl Send for XpcMessage {}
unsafe impl Sync for XpcMessage {}
