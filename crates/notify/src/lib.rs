use anyhow::{Context, Result};
use notify_rust::{Notification, Timeout};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageNotification {
    pub chat_name: String,
    pub sender_name: String,
    pub preview: Option<String>,
}

impl MessageNotification {
    pub fn title(&self) -> String {
        if self.sender_name.trim().is_empty() {
            self.chat_name.clone()
        } else {
            format!("{} · {}", self.sender_name, self.chat_name)
        }
    }

    pub fn body(&self) -> &str {
        self.preview.as_deref().unwrap_or("New message")
    }
}

#[derive(Clone, Debug, Default)]
pub struct DesktopNotifier;

impl DesktopNotifier {
    pub fn new() -> Self {
        Self
    }

    pub fn send_message(&self, notification: &MessageNotification) -> Result<()> {
        Notification::new()
            .appname("chat-cli")
            .summary(&notification.title())
            .body(notification.body())
            .timeout(Timeout::Milliseconds(6_000))
            .show()
            .context("showing desktop notification")?;
        Ok(())
    }
}
