use crate::types::*;
use std::sync::Arc;
use tokio::sync::broadcast;

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
    AuthRequired(AuthChallenge),
    AuthSucceeded,
    SyncProgress(u8),
    SyncComplete,
    Disconnected(Option<Arc<str>>),
    Reconnecting,
    Typing {
        chat_id: ChatId,
        sender: PlatformId,
        is_typing: bool,
    },
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
