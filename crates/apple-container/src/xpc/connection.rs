use std::ffi::CString;

use crate::error::AppleContainerError;
use crate::xpc::ffi;
use crate::xpc::message::XpcMessage;

/// A connection to an XPC Mach service.
pub struct XpcConnection {
    conn: ffi::xpc_connection_t,
    _queue: ffi::dispatch_queue_t,
}

impl XpcConnection {
    /// Connect to the named XPC Mach service.
    pub fn connect(service: &str) -> Result<Self, AppleContainerError> {
        let c_service = CString::new(service).map_err(|e| {
            AppleContainerError::ConnectionFailed(format!("Invalid service name: {e}"))
        })?;

        let label = CString::new(format!("{service}.queue")).map_err(|e| {
            AppleContainerError::ConnectionFailed(format!("Invalid queue label: {e}"))
        })?;

        let queue =
            unsafe { ffi::dispatch_queue_create(label.as_ptr(), std::ptr::null()) };
        if queue.is_null() {
            return Err(AppleContainerError::ConnectionFailed(
                "Failed to create dispatch queue".to_string(),
            ));
        }

        // Use flags=0 (non-privileged) — the Apple Container daemon is a user-level service.
        let conn = unsafe {
            ffi::xpc_connection_create_mach_service(c_service.as_ptr(), queue, 0)
        };
        if conn.is_null() {
            unsafe { ffi::dispatch_release(queue) };
            return Err(AppleContainerError::ConnectionFailed(
                "Failed to create XPC connection".to_string(),
            ));
        }

        // XPC requires an event handler to be set before resume. Without one,
        // the process receives SIGTRAP when XPC tries to deliver error events.
        // Leak the handler block so it outlives the connection — XPC may invoke
        // it asynchronously after xpc_connection_cancel (final INVALID event).
        let handler_block = Box::leak(Box::new(ffi::noop_event_handler_block()));
        unsafe {
            ffi::xpc_connection_set_event_handler(
                conn,
                handler_block as *const ffi::BlockLiteral as *const std::ffi::c_void,
            );
        }

        // Resume the connection to start processing messages.
        unsafe { ffi::xpc_connection_resume(conn) };

        Ok(Self {
            conn,
            _queue: queue,
        })
    }

    /// Send a message synchronously and return the reply.
    pub fn send(&self, msg: &XpcMessage) -> Result<XpcMessage, AppleContainerError> {
        let reply = unsafe {
            ffi::xpc_connection_send_message_with_reply_sync(self.conn, msg.as_ptr())
        };
        if reply.is_null() {
            return Err(AppleContainerError::SendFailed(
                "XPC send returned null".to_string(),
            ));
        }
        let reply_msg = unsafe { XpcMessage::from_raw(reply) };
        Ok(reply_msg)
    }

    /// Send a message asynchronously by wrapping the synchronous call in `spawn_blocking`.
    pub async fn send_async(&self, msg: &XpcMessage) -> Result<XpcMessage, AppleContainerError> {
        // Cast raw pointers to usize so the closure is Send.
        // Both xpc_connection_t and xpc_object_t are thread-safe on macOS.
        let conn_addr = self.conn as usize;
        let msg_addr = msg.as_ptr() as usize;

        // Retain the message so it stays alive in the blocking task.
        unsafe { ffi::xpc_retain(msg.as_ptr()) };

        let result = tokio::task::spawn_blocking(move || {
            let conn_ptr = conn_addr as *mut std::ffi::c_void;
            let msg_ptr = msg_addr as *mut std::ffi::c_void;

            let reply = unsafe {
                ffi::xpc_connection_send_message_with_reply_sync(conn_ptr, msg_ptr)
            };
            // Release our retained copy of the message.
            unsafe { ffi::xpc_release(msg_ptr) };

            if reply.is_null() {
                return Err(AppleContainerError::SendFailed(
                    "XPC send returned null".to_string(),
                ));
            }
            let reply_msg = unsafe { XpcMessage::from_raw(reply) };
            Ok(reply_msg)
        })
        .await
        .map_err(|e| AppleContainerError::SendFailed(format!("Task join error: {e}")))?;

        result
    }
}

impl Drop for XpcConnection {
    fn drop(&mut self) {
        unsafe {
            ffi::xpc_connection_cancel(self.conn);
            // The connection object is released implicitly when cancelled and
            // all references are dropped. We release the dispatch queue.
            ffi::dispatch_release(self._queue);
        }
    }
}

// XPC connections are thread-safe.
unsafe impl Send for XpcConnection {}
unsafe impl Sync for XpcConnection {}
