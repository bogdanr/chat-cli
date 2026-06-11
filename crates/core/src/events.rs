use crate::types::*;
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkActivityDirection {
    Rx,
    Tx,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkActivityKind {
    Auth,
    Connect,
    History,
    Send,
    Reaction,
    Receipt,
    Media,
    Realtime,
    Sync,
    Other,
}

#[derive(Clone, Debug)]
pub enum ProviderEvent {
    Message {
        message: Message,
        is_historical: bool,
    },
    MessageEdited {
        message: Message,
    },
    MessageDeleted {
        chat_id: ChatId,
        message_id: MessageId,
    },
    ReactionChanged {
        chat_id: ChatId,
        message_id: MessageId,
        emoji: Arc<str>,
        added: bool,
        sender: PlatformId,
    },
    Receipt {
        chat_id: ChatId,
        message_ids: Vec<MessageId>,
        kind: ReceiptKind,
        sender: PlatformId,
    },
    ChatUpdated(Chat),
    /// The chat was marked read (locally acknowledged or read on another
    /// client of the same account). Carries no activity metadata so it can
    /// never regress sidebar ordering; consumers should only clear unread
    /// state.
    ChatMarkedRead {
        chat_id: ChatId,
    },
    ChatMerged {
        from_chat_id: ChatId,
        to_chat_id: ChatId,
        chat: Chat,
    },
    AuthRequired(AuthChallenge),
    AuthSucceeded,
    SyncProgress(u8),
    SyncComplete,
    AccountNotice {
        title: Arc<str>,
        body: Arc<str>,
        severity: AccountNoticeSeverity,
    },
    Disconnected(Option<Arc<str>>),
    Reconnecting,
    Typing {
        chat_id: ChatId,
        sender: PlatformId,
        is_typing: bool,
    },
    NetworkActivity {
        direction: NetworkActivityDirection,
        kind: NetworkActivityKind,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AccountNoticeSeverity {
    /// Informational notice. Surfaced in-app and follows the user's
    /// notification mode preference for any desktop delivery.
    #[default]
    Info,
    /// Critical account condition that undermines the app's core purpose
    /// (for example, realtime messaging is unavailable). Always raises a
    /// system notification so the user is aware even when away from the TUI.
    SystemAlert,
}

#[derive(Clone, Debug)]
pub enum AuthChallenge {
    QrCode(Arc<str>),
    PairingCode(Arc<str>),
    OAuthUrl(Arc<str>),
    Waiting,
}

#[derive(Clone, Debug)]
pub struct EventBus {
    tx: broadcast::Sender<ProviderEvent>,
}

impl EventBus {
    pub const DEFAULT_CAPACITY: usize = 512;

    pub fn new() -> Self {
        Self::with_capacity(Self::DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn send(&self, event: ProviderEvent) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ProviderEvent> {
        self.tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
