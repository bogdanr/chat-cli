use std::{
    ffi::{CStr, CString},
    os::raw::{c_char, c_void},
};

pub type ClientHandle = u64;

type MessageCallback = unsafe extern "C" fn(message: *const c_char, user_data: *mut c_void);

unsafe extern "C" {
    fn C_NewClient(
        db_path: *const c_char,
        sync_scope: *const c_char,
        log_path: *const c_char,
    ) -> u64;
    fn C_Connect(client_id: u64) -> u8;
    fn C_SetMessageCallback(cb: Option<MessageCallback>, user_data: *mut c_void);
    fn C_SendText(client_id: u64, chat_jid: *const c_char, text: *const c_char) -> *mut c_char;
    fn C_FireSyntheticMessage(message: *const c_char) -> u8;
    fn C_FreeString(value: *mut c_char);
    fn C_Disconnect(client_id: u64);
}

pub fn new_client(
    db_path: &str,
    sync_scope: &str,
    log_path: Option<&str>,
) -> anyhow::Result<ClientHandle> {
    let db_path = CString::new(db_path)?;
    let sync_scope = CString::new(sync_scope)?;
    let log_path = CString::new(log_path.unwrap_or_default())?;
    let handle = unsafe { C_NewClient(db_path.as_ptr(), sync_scope.as_ptr(), log_path.as_ptr()) };
    Ok(handle)
}

pub fn connect(handle: ClientHandle) -> bool {
    unsafe { C_Connect(handle) != 0 }
}

/// Registers the callback trampoline with an opaque sender pointer.
///
/// # Safety
///
/// `user_data` must be either null or a valid pointer to a
/// `tokio::sync::mpsc::UnboundedSender<String>` that remains alive until the
/// callback is replaced or no further callbacks can occur.
pub unsafe fn set_message_callback(user_data: *mut c_void) {
    unsafe { C_SetMessageCallback(Some(message_callback_trampoline), user_data) }
}

/// Clears the registered callback.
///
/// # Safety
///
/// Callers must ensure no callback invocation is still using the previous
/// `user_data` pointer before freeing it.
pub unsafe fn clear_message_callback() {
    unsafe { C_SetMessageCallback(None, std::ptr::null_mut()) }
}

pub fn send_text(handle: ClientHandle, chat_jid: &str, text: &str) -> anyhow::Result<String> {
    let chat_jid = CString::new(chat_jid)?;
    let text = CString::new(text)?;
    let response = unsafe { C_SendText(handle, chat_jid.as_ptr(), text.as_ptr()) };
    if response.is_null() {
        return Ok(String::new());
    }

    let response_text = unsafe { CStr::from_ptr(response) }
        .to_string_lossy()
        .into_owned();
    unsafe {
        C_FreeString(response);
    }
    Ok(response_text)
}

pub fn fire_synthetic_message(message: &str) -> anyhow::Result<bool> {
    let message = CString::new(message)?;
    Ok(unsafe { C_FireSyntheticMessage(message.as_ptr()) != 0 })
}

pub fn disconnect(handle: ClientHandle) {
    unsafe { C_Disconnect(handle) }
}

unsafe extern "C" fn message_callback_trampoline(message: *const c_char, user_data: *mut c_void) {
    if message.is_null() || user_data.is_null() {
        return;
    }

    let message = unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned();
    let sender = unsafe { &*(user_data as *const tokio::sync::mpsc::UnboundedSender<String>) };
    let _ = sender.send(message);
}
