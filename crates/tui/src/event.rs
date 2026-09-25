use chat_core::{MessageId, ProviderEvent, ProviderId};
use crossterm::event::{KeyEvent, MouseEvent};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(u16, u16),
    /// Bracketed paste (also how terminals deliver dropped files).
    Paste(String),
    Tick,
    Provider(ProviderId, Box<ProviderEvent>),
    MediaReady(MessageId, PathBuf),
}
