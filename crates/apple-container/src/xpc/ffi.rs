//! Raw FFI bindings to macOS libxpc and libdispatch.

#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int, c_void};

/// Opaque XPC object pointer.
pub type xpc_object_t = *mut c_void;

/// Opaque XPC connection pointer (same underlying type).
pub type xpc_connection_t = *mut c_void;

/// Opaque XPC type descriptor.
pub type xpc_type_t = *const c_void;

/// Opaque dispatch queue pointer.
pub type dispatch_queue_t = *mut c_void;

/// Block type used by `xpc_connection_set_event_handler`.
/// This matches the layout of an Objective-C block literal.
pub type xpc_handler_t = *const c_void;

#[link(name = "System")]
unsafe extern "C" {
    // -- Connection --
    pub fn xpc_connection_create_mach_service(
        name: *const c_char,
        targetq: dispatch_queue_t,
        flags: u64,
    ) -> xpc_connection_t;

    pub fn xpc_connection_set_event_handler(
        connection: xpc_connection_t,
        handler: xpc_handler_t,
    );

    pub fn xpc_connection_resume(connection: xpc_connection_t);
    pub fn xpc_connection_cancel(connection: xpc_connection_t);

    pub fn xpc_connection_send_message_with_reply_sync(
        connection: xpc_connection_t,
        message: xpc_object_t,
    ) -> xpc_object_t;

    // -- Dictionary --
    pub fn xpc_dictionary_create(
        keys: *const *const c_char,
        values: *const xpc_object_t,
        count: usize,
    ) -> xpc_object_t;

    pub fn xpc_dictionary_set_string(
        xdict: xpc_object_t,
        key: *const c_char,
        string: *const c_char,
    );

    pub fn xpc_dictionary_get_string(xdict: xpc_object_t, key: *const c_char) -> *const c_char;

    pub fn xpc_dictionary_set_data(
        xdict: xpc_object_t,
        key: *const c_char,
        bytes: *const u8,
        length: usize,
    );

    pub fn xpc_dictionary_get_data(
        xdict: xpc_object_t,
        key: *const c_char,
        length: *mut usize,
    ) -> *const u8;

    pub fn xpc_dictionary_set_bool(xdict: xpc_object_t, key: *const c_char, value: bool);
    pub fn xpc_dictionary_get_bool(xdict: xpc_object_t, key: *const c_char) -> bool;

    pub fn xpc_dictionary_set_int64(xdict: xpc_object_t, key: *const c_char, value: i64);
    pub fn xpc_dictionary_get_int64(xdict: xpc_object_t, key: *const c_char) -> i64;

    pub fn xpc_dictionary_set_uint64(xdict: xpc_object_t, key: *const c_char, value: u64);
    pub fn xpc_dictionary_get_uint64(xdict: xpc_object_t, key: *const c_char) -> u64;

    pub fn xpc_dictionary_set_fd(xdict: xpc_object_t, key: *const c_char, fd: c_int);
    pub fn xpc_dictionary_dup_fd(xdict: xpc_object_t, key: *const c_char) -> c_int;

    pub fn xpc_dictionary_get_value(xdict: xpc_object_t, key: *const c_char) -> xpc_object_t;

    // -- Lifecycle --
    pub fn xpc_retain(object: xpc_object_t) -> xpc_object_t;
    pub fn xpc_release(object: xpc_object_t);

    // -- Type checking --
    pub fn xpc_get_type(object: xpc_object_t) -> xpc_type_t;

    // -- Error sentinel objects --
    pub static _xpc_type_error: xpc_type_t;

    // -- Global block class (for constructing block literals) --
    pub static _NSConcreteGlobalBlock: *const c_void;

    // -- Dispatch --
    pub fn dispatch_queue_create(
        label: *const c_char,
        attr: *const c_void, // DISPATCH_QUEUE_SERIAL = NULL
    ) -> dispatch_queue_t;

    pub fn dispatch_release(object: *mut c_void);
}

/// Check if an XPC object is an error type.
///
/// # Safety
/// `obj` must be a valid XPC object pointer.
pub unsafe fn xpc_object_is_error(obj: xpc_object_t) -> bool {
    if obj.is_null() {
        return true;
    }
    unsafe { xpc_get_type(obj) == _xpc_type_error }
}

// -- Block literal construction for XPC event handler --

/// Layout of an Objective-C block literal (global, no captures).
#[repr(C)]
pub struct BlockLiteral {
    pub isa: *const c_void,
    pub flags: i32,
    pub reserved: i32,
    pub invoke: unsafe extern "C" fn(*mut BlockLiteral, xpc_object_t),
    pub descriptor: *const BlockDescriptor,
}

// Global blocks with no captures are safe to share across threads.
unsafe impl Sync for BlockLiteral {}

#[repr(C)]
pub struct BlockDescriptor {
    pub reserved: u64,
    pub size: u64,
}

/// No-op event handler invocation function.
unsafe extern "C" fn noop_event_handler(_block: *mut BlockLiteral, _event: xpc_object_t) {}

static NOOP_BLOCK_DESCRIPTOR: BlockDescriptor = BlockDescriptor {
    reserved: 0,
    size: std::mem::size_of::<BlockLiteral>() as u64,
};

/// A global, no-op block that can be used as an XPC event handler.
/// This satisfies XPC's requirement that an event handler be set before resume.
pub static NOOP_EVENT_HANDLER: BlockLiteral = BlockLiteral {
    // SAFETY: _NSConcreteGlobalBlock is set by the dynamic linker at load time.
    // Using a zero pointer here and patching at runtime in `noop_event_handler_ptr`.
    isa: std::ptr::null(),
    flags: (1 << 28), // BLOCK_IS_GLOBAL
    reserved: 0,
    invoke: noop_event_handler,
    descriptor: &NOOP_BLOCK_DESCRIPTOR,
};

/// Get a pointer to the no-op event handler block, with the `isa` pointer set correctly.
///
/// We can't use `_NSConcreteGlobalBlock` in a static initializer because it's an extern,
/// so we construct the block on the stack with the correct isa pointer.
pub fn noop_event_handler_block() -> BlockLiteral {
    BlockLiteral {
        isa: unsafe { _NSConcreteGlobalBlock },
        flags: (1 << 28), // BLOCK_IS_GLOBAL
        reserved: 0,
        invoke: noop_event_handler,
        descriptor: &NOOP_BLOCK_DESCRIPTOR,
    }
}
