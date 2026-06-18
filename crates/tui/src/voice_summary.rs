//! Voice-summary dispatch to the external `fono` voice assistant.
//!
//! When the user enables the "Voice summaries" setting, every *delivered*
//! message notification also dispatches a structured payload to the `fono`
//! CLI (`fono summarize --json`), which summarizes the message with its
//! configured LLM and speaks one or two sentences about who wants what.
//! Raw message bodies are never spoken; fono's summarize prompt forbids
//! quoting long content or logs.
//!
//! The dispatch is fire-and-forget from the TUI's perspective: the process
//! runs on a background tokio task and reports an outcome through an
//! unbounded channel that the app drains on ticks, purely for status and
//! performance instrumentation. Summaries inherit every notification
//! eligibility rule because payloads are only built on the notification
//! queue path (mode, scope, mute, and pause all apply), and the app
//! re-checks the delivery-time gates plus attended-chat cancellation
//! before dispatching.
//!
//! Messages the user sent themselves are never summarized, even when
//! `notify_self_messages` lets them raise a visual notification.
//! Consecutive messages from the same sender in the same chat are grouped:
//! each new message folds into the pending payload and pushes the dispatch
//! deadline out by [`GROUP_WINDOW`], so a burst speaks once, after it
//! settles.

use std::process::Stdio;
use std::time::{Duration, Instant};

use chat_core::{Chat, ChatId, ChatKind, Content, Message, MessageId, ProviderId};
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// Upper bound on the message text shipped to fono. Fono truncates input
/// itself before prompting, so this only bounds the local pipe write.
const MESSAGE_TEXT_CAP: usize = 30_000;

/// How long a single summarize-and-speak run may take end to end. Local
/// models plus TTS playback can be slow, so this is generous; the process is
/// killed when the budget is exhausted so stale summaries are never spoken
/// minutes later.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(180);

/// Environment variable that overrides the `fono` binary path.
pub const FONO_BIN_ENV: &str = "CHAT_CLI_FONO_BIN";

/// Debounce window for grouping messages from the same sender in the same
/// chat into a single voice summary. Each new message in the burst extends
/// the deadline by this much, so the summary is spoken once the sender has
/// been quiet for the whole window.
pub const GROUP_WINDOW: Duration = Duration::from_secs(15);

/// Structured payload matching the input schema of `fono summarize --json`
/// (and the `fono.summarize` MCP tool). All fields are optional on the fono
/// side except `message_text`; empty strings are omitted from the rendered
/// prompt there.
#[derive(Clone, Debug, Serialize)]
pub struct VoiceSummaryPayload {
    pub source_app: String,
    pub source_kind: String,
    pub account: String,
    pub chat_name: String,
    pub chat_kind: String,
    pub sender_name: String,
    pub message_text: String,
    pub attachments: Vec<VoiceSummaryAttachment>,
    pub instructions: String,
}

impl VoiceSummaryPayload {
    /// Fold a newer message from the same sender into this payload so a
    /// burst is summarized once. Texts are concatenated with blank-line
    /// separators (still capped at [`MESSAGE_TEXT_CAP`]), attachment
    /// metadata accumulates, and the instructions tell fono to treat the
    /// payload as one grouped update of `total_messages` messages.
    pub fn merge_grouped(&mut self, newer: VoiceSummaryPayload, total_messages: usize) {
        if !newer.message_text.trim().is_empty() {
            if self.message_text.trim().is_empty() {
                self.message_text = newer.message_text;
            } else {
                self.message_text.push_str("\n\n");
                self.message_text.push_str(&newer.message_text);
            }
            if self.message_text.chars().count() > MESSAGE_TEXT_CAP {
                self.message_text = self.message_text.chars().take(MESSAGE_TEXT_CAP).collect();
            }
        }
        self.attachments.extend(newer.attachments);
        let thread_note =
            if self.instructions.contains("thread") || newer.instructions.contains("thread") {
                " Some of the messages are replies inside an existing thread."
            } else {
                ""
            };
        self.instructions = format!(
            "The message text contains {total_messages} consecutive messages from the same \
             sender, separated by blank lines; summarize them together as one \
             update.{thread_note}"
        );
    }
}

/// Attachment metadata only — fono describes attachments by kind and name,
/// it never receives or reads file contents in this version.
#[derive(Clone, Debug, Serialize)]
pub struct VoiceSummaryAttachment {
    pub kind: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: Option<u64>,
}

/// Completion report for one dispatched summary, drained by the app on
/// ticks for status/instrumentation. `result` carries the failure detail
/// when the process could not be spawned, timed out, or exited non-zero.
#[derive(Debug)]
pub struct VoiceSummaryOutcome {
    pub account: ProviderId,
    pub chat_id: ChatId,
    pub message_id: MessageId,
    pub result: Result<(), String>,
    pub elapsed: Duration,
}

/// The argv used to dispatch summaries. The binary defaults to `fono` on
/// `PATH` and can be overridden with [`FONO_BIN_ENV`]; tests override the
/// whole argv on the app instead of mutating the process environment.
pub fn default_command() -> Vec<String> {
    let bin = std::env::var(FONO_BIN_ENV)
        .ok()
        .filter(|bin| !bin.trim().is_empty())
        .unwrap_or_else(|| "fono".to_owned());
    vec![bin, "summarize".to_owned(), "--json".to_owned()]
}

/// Build the payload for an incoming message. `message_text` is the full
/// extracted text (not the 80-char notification preview) so fono's model can
/// understand intent; fono never speaks it verbatim.
pub fn payload_for_message(
    chat: &Chat,
    account_label: &str,
    message: &Message,
    message_text: String,
    is_thread_reply: bool,
) -> VoiceSummaryPayload {
    let mut text = message_text;
    if text.chars().count() > MESSAGE_TEXT_CAP {
        text = text.chars().take(MESSAGE_TEXT_CAP).collect();
    }
    VoiceSummaryPayload {
        source_app: "chat-cli".to_owned(),
        source_kind: "incoming_message".to_owned(),
        account: account_label.to_owned(),
        chat_name: chat.name.to_string(),
        chat_kind: chat_kind_label(chat.kind).to_owned(),
        sender_name: message.sender.display_name.to_string(),
        message_text: text,
        attachments: attachments_for_content(&message.content),
        instructions: if is_thread_reply {
            "The message is a reply inside an existing thread.".to_owned()
        } else {
            String::new()
        },
    }
}

fn chat_kind_label(kind: ChatKind) -> &'static str {
    match kind {
        ChatKind::Direct => "direct",
        ChatKind::Group => "group",
        ChatKind::PublicChannel => "channel",
        ChatKind::PrivateChannel => "private_channel",
        ChatKind::GroupDirectMessage => "group_dm",
    }
}

fn attachments_for_content(content: &Content) -> Vec<VoiceSummaryAttachment> {
    let (kind, media) = match content {
        Content::Image(media) => ("image", media),
        Content::Video(media) => ("video", media),
        Content::Audio(media) => ("audio", media),
        Content::File(media) => ("file", media),
        Content::Sticker(media) => ("sticker", media),
        Content::Text(_)
        | Content::LinkPreview(_)
        | Content::Cards(_)
        | Content::Poll(_)
        | Content::Deleted
        | Content::Unsupported(_) => return Vec::new(),
    };
    vec![VoiceSummaryAttachment {
        kind: kind.to_owned(),
        filename: media.file_name.to_string(),
        mime_type: media.mime_type.to_string(),
        size_bytes: media.size_bytes,
    }]
}

/// Spawn the summarize process on a background task. Never blocks the event
/// loop: serialization happens here (bounded by [`MESSAGE_TEXT_CAP`]), the
/// process I/O and wait happen on the spawned task, and the outcome is
/// reported through `tx` for the app's tick drain.
pub fn dispatch(
    command: &[String],
    payload: &VoiceSummaryPayload,
    tx: mpsc::UnboundedSender<VoiceSummaryOutcome>,
    account: ProviderId,
    chat_id: ChatId,
    message_id: MessageId,
) {
    let Some((program, args)) = command.split_first() else {
        let _ = tx.send(VoiceSummaryOutcome {
            account,
            chat_id,
            message_id,
            result: Err("voice summary command is empty".to_owned()),
            elapsed: Duration::ZERO,
        });
        return;
    };
    let json = match serde_json::to_vec(payload) {
        Ok(json) => json,
        Err(error) => {
            let _ = tx.send(VoiceSummaryOutcome {
                account,
                chat_id,
                message_id,
                result: Err(format!(
                    "voice summary payload serialization failed: {error}"
                )),
                elapsed: Duration::ZERO,
            });
            return;
        }
    };

    let program = program.clone();
    let args: Vec<String> = args.to_vec();
    tokio::spawn(async move {
        let started = Instant::now();
        let result = run_summary_process(&program, &args, &json).await;
        let _ = tx.send(VoiceSummaryOutcome {
            account,
            chat_id,
            message_id,
            result,
            elapsed: started.elapsed(),
        });
    });
}

async fn run_summary_process(program: &str, args: &[String], json: &[u8]) -> Result<(), String> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("could not start {program}: {error}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(json)
            .await
            .map_err(|error| format!("could not write payload to {program}: {error}"))?;
        drop(stdin);
    }

    let output = tokio::time::timeout(SUMMARY_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| format!("{program} timed out after {}s", SUMMARY_TIMEOUT.as_secs()))?
        .map_err(|error| format!("could not wait for {program}: {error}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.lines().last().unwrap_or("").trim().to_owned();
        Err(format!(
            "{program} exited with {}{}",
            output.status,
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use chat_core::{ChatMembership, Media, Platform, PlatformData, Sender, Timestamp};

    use super::*;

    fn test_chat(kind: ChatKind) -> Chat {
        Chat {
            id: Arc::from("C-1"),
            account: Arc::from("slack:acme"),
            platform: Platform::Slack,
            name: Arc::from("Backend Alerts"),
            avatar: None,
            is_group: true,
            kind,
            membership: ChatMembership::Joined,
            is_shared: false,
            unread_count: 0,
            muted: false,
            pinned: false,
            last_message_at: None,
            last_message_preview: None,
            thread_id: None,
        }
    }

    fn test_message(content: Content) -> Message {
        Message {
            id: Arc::from("m-1"),
            chat_id: Arc::from("C-1"),
            account: Arc::from("slack:acme"),
            sender: Sender {
                platform_id: Arc::from("U-mihai"),
                display_name: Arc::from("Mihai"),
                avatar: None,
            },
            timestamp: Timestamp::from_timestamp(1_710_000_000, 0).unwrap(),
            edited_at: None,
            content,
            reply_to: None,
            thread_id: None,
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me: false,
            mentions_me: false,
            platform_data: PlatformData::default(),
        }
    }

    #[test]
    fn payload_serializes_with_fono_schema_field_names() {
        let chat = test_chat(ChatKind::PublicChannel);
        let message = test_message(Content::Text(Arc::from("deploy failed")));
        let payload = payload_for_message(
            &chat,
            "Engineering Slack",
            &message,
            "deploy failed".to_owned(),
            false,
        );
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["source_app"], "chat-cli");
        assert_eq!(json["source_kind"], "incoming_message");
        assert_eq!(json["account"], "Engineering Slack");
        assert_eq!(json["chat_name"], "Backend Alerts");
        assert_eq!(json["chat_kind"], "channel");
        assert_eq!(json["sender_name"], "Mihai");
        assert_eq!(json["message_text"], "deploy failed");
        assert_eq!(json["instructions"], "");
        assert!(json["attachments"].as_array().unwrap().is_empty());
    }

    #[test]
    fn media_message_carries_attachment_metadata_only() {
        let chat = test_chat(ChatKind::Direct);
        let media = Media {
            id: Arc::from("f-1"),
            file_name: Arc::from("screenshot.png"),
            mime_type: Arc::from("image/png"),
            size_bytes: Some(2048),
            caption: Some(Arc::from("look at this")),
            local_path: Some(PathBuf::from("/tmp/screenshot.png")),
            thumbnail: None,
        };
        let message = test_message(Content::Image(media));
        let payload = payload_for_message(
            &chat,
            "Engineering Slack",
            &message,
            "look at this".to_owned(),
            false,
        );
        assert_eq!(payload.chat_kind, "direct");
        assert_eq!(payload.attachments.len(), 1);
        let attachment = &payload.attachments[0];
        assert_eq!(attachment.kind, "image");
        assert_eq!(attachment.filename, "screenshot.png");
        assert_eq!(attachment.mime_type, "image/png");
        assert_eq!(attachment.size_bytes, Some(2048));
        // The payload never includes paths or file contents.
        let json = serde_json::to_string(&payload).unwrap();
        assert!(!json.contains("/tmp/screenshot.png"));
    }

    #[test]
    fn thread_replies_note_the_thread_in_instructions() {
        let chat = test_chat(ChatKind::PublicChannel);
        let message = test_message(Content::Text(Arc::from("shipping it")));
        let payload = payload_for_message(&chat, "Acme", &message, "shipping it".to_owned(), true);
        assert!(payload.instructions.contains("thread"));
    }

    #[test]
    fn merge_grouped_folds_texts_and_counts_messages() {
        let chat = test_chat(ChatKind::Direct);
        let first = test_message(Content::Text(Arc::from("are you around?")));
        let second = test_message(Content::Text(Arc::from("need a review on the auth PR")));
        let mut payload =
            payload_for_message(&chat, "Acme", &first, "are you around?".to_owned(), false);
        let newer = payload_for_message(
            &chat,
            "Acme",
            &second,
            "need a review on the auth PR".to_owned(),
            false,
        );
        payload.merge_grouped(newer, 2);
        assert_eq!(
            payload.message_text,
            "are you around?\n\nneed a review on the auth PR"
        );
        assert!(payload.instructions.contains('2'));
        assert!(payload.instructions.contains("consecutive messages"));
        assert!(!payload.instructions.contains("thread"));
    }

    #[test]
    fn merge_grouped_preserves_thread_note_and_attachments() {
        let chat = test_chat(ChatKind::PublicChannel);
        let first = test_message(Content::Text(Arc::from("shipping it")));
        let mut payload =
            payload_for_message(&chat, "Acme", &first, "shipping it".to_owned(), true);
        let media = Media {
            id: Arc::from("f-1"),
            file_name: Arc::from("screenshot.png"),
            mime_type: Arc::from("image/png"),
            size_bytes: Some(2048),
            caption: None,
            local_path: None,
            thumbnail: None,
        };
        let second = test_message(Content::Image(media));
        let newer = payload_for_message(&chat, "Acme", &second, String::new(), false);
        payload.merge_grouped(newer, 2);
        // The empty caption must not introduce a separator.
        assert_eq!(payload.message_text, "shipping it");
        assert_eq!(payload.attachments.len(), 1);
        assert!(payload.instructions.contains("thread"));
    }

    #[test]
    fn merge_grouped_caps_combined_text() {
        let chat = test_chat(ChatKind::Direct);
        let message = test_message(Content::Text(Arc::from("x")));
        let mut payload = payload_for_message(
            &chat,
            "Acme",
            &message,
            "y".repeat(MESSAGE_TEXT_CAP - 10),
            false,
        );
        let newer = payload_for_message(&chat, "Acme", &message, "z".repeat(100), false);
        payload.merge_grouped(newer, 2);
        assert_eq!(payload.message_text.chars().count(), MESSAGE_TEXT_CAP);
    }

    #[test]
    fn oversized_message_text_is_capped() {
        let chat = test_chat(ChatKind::Direct);
        let message = test_message(Content::Text(Arc::from("x")));
        let huge = "y".repeat(MESSAGE_TEXT_CAP + 5_000);
        let payload = payload_for_message(&chat, "Acme", &message, huge, false);
        assert_eq!(payload.message_text.chars().count(), MESSAGE_TEXT_CAP);
    }

    #[test]
    fn default_command_targets_fono_summarize_json() {
        // Do not mutate the process environment here: other tests run in
        // parallel. The default path only depends on the env var being unset
        // or set; assert the stable argv tail instead.
        let command = default_command();
        assert_eq!(command.len(), 3);
        assert_eq!(command[1], "summarize");
        assert_eq!(command[2], "--json");
    }

    #[tokio::test]
    async fn dispatch_reports_ok_for_succeeding_command() {
        let chat = test_chat(ChatKind::Direct);
        let message = test_message(Content::Text(Arc::from("hello")));
        let payload = payload_for_message(&chat, "Acme", &message, "hello".to_owned(), false);
        let (tx, mut rx) = mpsc::unbounded_channel();
        // `cat` consumes stdin and exits 0 — a stand-in for a healthy fono.
        dispatch(
            &["cat".to_owned()],
            &payload,
            tx,
            Arc::from("slack:acme"),
            Arc::from("C-1"),
            Arc::from("m-1"),
        );
        let outcome = rx.recv().await.expect("outcome arrives");
        assert!(outcome.result.is_ok(), "got: {:?}", outcome.result);
        assert_eq!(outcome.message_id.as_ref(), "m-1");
    }

    #[tokio::test]
    async fn dispatch_reports_error_for_missing_binary() {
        let chat = test_chat(ChatKind::Direct);
        let message = test_message(Content::Text(Arc::from("hello")));
        let payload = payload_for_message(&chat, "Acme", &message, "hello".to_owned(), false);
        let (tx, mut rx) = mpsc::unbounded_channel();
        dispatch(
            &["/nonexistent/fono-test-binary".to_owned()],
            &payload,
            tx,
            Arc::from("slack:acme"),
            Arc::from("C-1"),
            Arc::from("m-1"),
        );
        let outcome = rx.recv().await.expect("outcome arrives");
        let error = outcome.result.expect_err("missing binary must fail");
        assert!(error.contains("could not start"), "got: {error}");
    }
}
