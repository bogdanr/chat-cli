use std::{
    ffi::{CStr, CString},
    os::raw::{c_char, c_void},
};

pub type ClientHandle = u64;

/// Describes the message an outgoing send is replying to, so the bridge can
/// attach a WhatsApp `ContextInfo` and have the recipient render a native
/// quoted reply. `participant` is the quoted sender's JID (or "me"/empty for
/// our own messages) and `quoted_text` is a plain-text fallback preview.
#[derive(Clone, Debug, Default)]
pub struct ReplyTarget {
    pub id: String,
    pub participant: String,
    pub quoted_text: String,
}

type MessageCallback = unsafe extern "C" fn(message: *const c_char, user_data: *mut c_void);

unsafe extern "C" {
    fn C_NewClient(
        db_path: *const c_char,
        sync_scope: *const c_char,
        log_path: *const c_char,
    ) -> u64;
    fn C_Connect(client_id: u64) -> u8;
    fn C_SetMessageCallback(cb: Option<MessageCallback>, user_data: *mut c_void);
    fn C_SendText(
        client_id: u64,
        chat_jid: *const c_char,
        text: *const c_char,
        reply_id: *const c_char,
        reply_participant: *const c_char,
        reply_text: *const c_char,
        mentioned_jids: *const c_char,
    ) -> *mut c_char;
    fn C_RequestHistory(
        client_id: u64,
        chat_jid: *const c_char,
        oldest_msg_id: *const c_char,
        oldest_from_me: u8,
        oldest_timestamp_unix: i64,
        count: i32,
    ) -> *mut c_char;
    fn C_SendMedia(
        client_id: u64,
        chat_jid: *const c_char,
        path: *const c_char,
        mime_type: *const c_char,
        file_name: *const c_char,
        caption: *const c_char,
        content_type: *const c_char,
        reply_id: *const c_char,
        reply_participant: *const c_char,
        reply_text: *const c_char,
    ) -> *mut c_char;
    fn C_SendReaction(
        client_id: u64,
        chat_jid: *const c_char,
        sender_jid: *const c_char,
        message_id: *const c_char,
        emoji: *const c_char,
    ) -> *mut c_char;
    fn C_SendPollVote(
        client_id: u64,
        chat_jid: *const c_char,
        sender_jid: *const c_char,
        message_id: *const c_char,
        selected_options_json: *const c_char,
    ) -> *mut c_char;
    fn C_MarkRead(
        client_id: u64,
        chat_jid: *const c_char,
        messages_json: *const c_char,
    ) -> *mut c_char;
    fn C_SearchContacts(client_id: u64, query: *const c_char, limit: i32) -> *mut c_char;
    fn C_GroupMembers(client_id: u64, chat_jid: *const c_char) -> *mut c_char;
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

pub fn send_text(
    handle: ClientHandle,
    chat_jid: &str,
    text: &str,
    reply: Option<&ReplyTarget>,
    mentioned_jids: &[String],
) -> anyhow::Result<String> {
    let chat_jid = CString::new(chat_jid)?;
    let text = CString::new(text)?;
    let reply_id = CString::new(reply.map(|r| r.id.as_str()).unwrap_or_default())?;
    let reply_participant =
        CString::new(reply.map(|r| r.participant.as_str()).unwrap_or_default())?;
    let reply_text = CString::new(reply.map(|r| r.quoted_text.as_str()).unwrap_or_default())?;
    // JIDs never contain commas, so a comma-separated list is unambiguous. An
    // empty list preserves the previous no-mention behavior.
    let mentioned_jids = CString::new(mentioned_jids.join(","))?;
    let response = unsafe {
        C_SendText(
            handle,
            chat_jid.as_ptr(),
            text.as_ptr(),
            reply_id.as_ptr(),
            reply_participant.as_ptr(),
            reply_text.as_ptr(),
            mentioned_jids.as_ptr(),
        )
    };
    take_c_string(response)
}

pub fn request_history(
    handle: ClientHandle,
    chat_jid: &str,
    oldest_msg_id: &str,
    oldest_from_me: bool,
    oldest_timestamp_unix: i64,
    count: usize,
) -> anyhow::Result<String> {
    let chat_jid = CString::new(chat_jid)?;
    let oldest_msg_id = CString::new(oldest_msg_id)?;
    let count = count.min(i32::MAX as usize) as i32;
    let response = unsafe {
        C_RequestHistory(
            handle,
            chat_jid.as_ptr(),
            oldest_msg_id.as_ptr(),
            u8::from(oldest_from_me),
            oldest_timestamp_unix,
            count,
        )
    };
    take_c_string(response)
}

#[allow(clippy::too_many_arguments)]
pub fn send_media(
    handle: ClientHandle,
    chat_jid: &str,
    path: &str,
    mime_type: &str,
    file_name: &str,
    caption: &str,
    content_type: &str,
    reply: Option<&ReplyTarget>,
) -> anyhow::Result<String> {
    let chat_jid = CString::new(chat_jid)?;
    let path = CString::new(path)?;
    let mime_type = CString::new(mime_type)?;
    let file_name = CString::new(file_name)?;
    let caption = CString::new(caption)?;
    let content_type = CString::new(content_type)?;
    let reply_id = CString::new(reply.map(|r| r.id.as_str()).unwrap_or_default())?;
    let reply_participant =
        CString::new(reply.map(|r| r.participant.as_str()).unwrap_or_default())?;
    let reply_text = CString::new(reply.map(|r| r.quoted_text.as_str()).unwrap_or_default())?;
    let response = unsafe {
        C_SendMedia(
            handle,
            chat_jid.as_ptr(),
            path.as_ptr(),
            mime_type.as_ptr(),
            file_name.as_ptr(),
            caption.as_ptr(),
            content_type.as_ptr(),
            reply_id.as_ptr(),
            reply_participant.as_ptr(),
            reply_text.as_ptr(),
        )
    };
    take_c_string(response)
}

pub fn send_reaction(
    handle: ClientHandle,
    chat_jid: &str,
    sender_jid: &str,
    message_id: &str,
    emoji: &str,
) -> anyhow::Result<String> {
    let chat_jid = CString::new(chat_jid)?;
    let sender_jid = CString::new(sender_jid)?;
    let message_id = CString::new(message_id)?;
    let emoji = CString::new(emoji)?;
    let response = unsafe {
        C_SendReaction(
            handle,
            chat_jid.as_ptr(),
            sender_jid.as_ptr(),
            message_id.as_ptr(),
            emoji.as_ptr(),
        )
    };
    take_c_string(response)
}

pub fn send_poll_vote(
    handle: ClientHandle,
    chat_jid: &str,
    sender_jid: &str,
    message_id: &str,
    selected_options: &[String],
) -> anyhow::Result<String> {
    let chat_jid = CString::new(chat_jid)?;
    let sender_jid = CString::new(sender_jid)?;
    let message_id = CString::new(message_id)?;
    let selected_options_json = CString::new(serde_json::to_string(selected_options)?)?;
    let response = unsafe {
        C_SendPollVote(
            handle,
            chat_jid.as_ptr(),
            sender_jid.as_ptr(),
            message_id.as_ptr(),
            selected_options_json.as_ptr(),
        )
    };
    take_c_string(response)
}

pub fn search_contacts(handle: ClientHandle, query: &str, limit: usize) -> anyhow::Result<String> {
    let query = CString::new(query)?;
    let limit = limit.min(i32::MAX as usize) as i32;
    let response = unsafe { C_SearchContacts(handle, query.as_ptr(), limit) };
    take_c_string(response)
}

/// Lists the participants of a WhatsApp group, including each member's admin
/// authority, as a JSON `group_members` bridge event.
pub fn group_members(handle: ClientHandle, chat_jid: &str) -> anyhow::Result<String> {
    let chat_jid = CString::new(chat_jid)?;
    let response = unsafe { C_GroupMembers(handle, chat_jid.as_ptr()) };
    take_c_string(response)
}

/// Sends read receipts for the given messages. `messages_json` is a JSON
/// array of `{"id": ..., "sender_jid": ...}` objects describing inbound
/// messages of the chat that should be acknowledged.
pub fn mark_read(
    handle: ClientHandle,
    chat_jid: &str,
    messages_json: &str,
) -> anyhow::Result<String> {
    let chat_jid = CString::new(chat_jid)?;
    let messages_json = CString::new(messages_json)?;
    let response = unsafe { C_MarkRead(handle, chat_jid.as_ptr(), messages_json.as_ptr()) };
    take_c_string(response)
}

fn take_c_string(response: *mut c_char) -> anyhow::Result<String> {
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
