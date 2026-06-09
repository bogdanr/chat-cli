use anyhow::{Context, Result, anyhow};
use notify_rust::{Notification, Timeout};
use std::process::Command;
use std::sync::{Arc, Mutex};

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
pub struct DesktopNotifier {
    /// When set, system notifications are captured in-memory instead of being
    /// dispatched to the OS. Used by tests to assert delivery without requiring
    /// a desktop notification daemon.
    capture: Option<Arc<Mutex<Vec<MessageNotification>>>>,
}

impl DesktopNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a notifier that records every notification it is asked to send in
    /// the returned buffer instead of dispatching to the OS.
    pub fn with_capture() -> (Self, Arc<Mutex<Vec<MessageNotification>>>) {
        let capture = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                capture: Some(Arc::clone(&capture)),
            },
            capture,
        )
    }

    pub fn send_message(&self, notification: &MessageNotification) -> Result<()> {
        if let Some(capture) = &self.capture {
            capture
                .lock()
                .expect("notification capture mutex poisoned")
                .push(notification.clone());
            return Ok(());
        }

        match send_via_dbus(notification) {
            Ok(()) => Ok(()),
            Err(dbus_error) => {
                // notify_rust reaches the session bus through zbus, which only
                // honours DBUS_SESSION_BUS_ADDRESS / $XDG_RUNTIME_DIR/bus and
                // does NOT perform D-Bus X11 autolaunch. In minimal/root X
                // sessions the bus is published only via the X root-window
                // autolaunch property, so zbus fails even though a notification
                // daemon is reachable. The `notify-send` CLI does perform X11
                // autolaunch, so fall back to it before reporting failure.
                send_via_notify_send(notification).map_err(|cli_error| {
                    dbus_error.context(format!("notify-send fallback also failed: {cli_error:#}"))
                })
            }
        }
    }
}

fn send_via_dbus(notification: &MessageNotification) -> Result<()> {
    Notification::new()
        .appname("chat-cli")
        .summary(&notification.title())
        .body(notification.body())
        .timeout(Timeout::Milliseconds(6_000))
        .show()
        .context("showing desktop notification via D-Bus")?;
    Ok(())
}

/// Builds the argument vector passed to the `notify-send` CLI. Options precede
/// the positional summary/body so the daemon receives the expected fields.
fn notify_send_args(notification: &MessageNotification) -> Vec<String> {
    vec![
        "--app-name=chat-cli".to_string(),
        "--expire-time=6000".to_string(),
        notification.title(),
        notification.body().to_string(),
    ]
}

fn send_via_notify_send(notification: &MessageNotification) -> Result<()> {
    let status = Command::new("notify-send")
        .args(notify_send_args(notification))
        .status()
        .context("spawning notify-send")?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("notify-send exited with status {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_send_args_carry_title_and_body_after_options() {
        let notification = MessageNotification {
            chat_name: "Slack".to_string(),
            sender_name: String::new(),
            preview: Some("Realtime unavailable".to_string()),
        };

        let args = notify_send_args(&notification);

        assert_eq!(
            args,
            vec![
                "--app-name=chat-cli".to_string(),
                "--expire-time=6000".to_string(),
                "Slack".to_string(),
                "Realtime unavailable".to_string(),
            ]
        );
    }

    #[test]
    fn notify_send_body_falls_back_to_default_when_preview_missing() {
        let notification = MessageNotification {
            chat_name: "Slack".to_string(),
            sender_name: "Alice".to_string(),
            preview: None,
        };

        let args = notify_send_args(&notification);

        assert_eq!(args.last().map(String::as_str), Some("New message"));
        assert_eq!(args[2], "Alice · Slack");
    }
}
