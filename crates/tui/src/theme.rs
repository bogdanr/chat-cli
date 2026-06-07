use ratatui::style::{Color, Modifier, Style};
use storage::UiThemePreset;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    pub background: Color,
    pub foreground: Color,
    pub muted: Color,
    pub accent: Color,
    pub incoming: Color,
    pub outgoing: Color,
    pub unread: Color,
    pub warning: Color,
    pub error: Color,
    pub overlay: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::from_preset(UiThemePreset::default())
    }
}

impl Theme {
    pub fn from_preset(preset: UiThemePreset) -> Self {
        match preset {
            UiThemePreset::DefaultDark => Self {
                background: Color::Reset,
                foreground: Color::White,
                muted: Color::DarkGray,
                accent: Color::Cyan,
                incoming: Color::Green,
                outgoing: Color::LightCyan,
                unread: Color::Yellow,
                warning: Color::Yellow,
                error: Color::Red,
                overlay: Color::LightMagenta,
            },
            UiThemePreset::Light => Self {
                background: Color::White,
                foreground: Color::Black,
                muted: Color::Gray,
                accent: Color::Blue,
                incoming: Color::Green,
                outgoing: Color::Blue,
                unread: Color::Magenta,
                warning: Color::Yellow,
                error: Color::Red,
                overlay: Color::Blue,
            },
            UiThemePreset::HighContrast => Self {
                background: Color::Black,
                foreground: Color::White,
                muted: Color::Gray,
                accent: Color::LightYellow,
                incoming: Color::LightGreen,
                outgoing: Color::LightCyan,
                unread: Color::LightYellow,
                warning: Color::LightYellow,
                error: Color::LightRed,
                overlay: Color::LightYellow,
            },
            UiThemePreset::WhatsApp => Self {
                background: Color::Black,
                foreground: Color::White,
                muted: Color::DarkGray,
                accent: Color::Green,
                incoming: Color::LightGreen,
                outgoing: Color::LightCyan,
                unread: Color::LightGreen,
                warning: Color::Yellow,
                error: Color::Red,
                overlay: Color::Green,
            },
            UiThemePreset::Slack => Self {
                background: Color::Black,
                foreground: Color::White,
                muted: Color::Gray,
                accent: Color::Magenta,
                incoming: Color::LightMagenta,
                outgoing: Color::LightBlue,
                unread: Color::LightMagenta,
                warning: Color::Yellow,
                error: Color::LightRed,
                overlay: Color::Magenta,
            },
        }
    }
    pub fn focus_border(self, focused: bool) -> Style {
        if focused {
            Style::default().fg(self.accent)
        } else {
            Style::default().fg(self.muted)
        }
    }

    pub fn pane_title(self) -> Style {
        Style::default()
            .fg(self.foreground)
            .add_modifier(Modifier::BOLD)
    }

    pub fn muted(self) -> Style {
        Style::default().fg(self.muted)
    }

    pub fn unread(self) -> Style {
        Style::default()
            .fg(self.unread)
            .add_modifier(Modifier::BOLD)
    }

    pub fn outgoing(self) -> Style {
        Style::default()
            .fg(self.outgoing)
            .add_modifier(Modifier::BOLD)
    }

    pub fn incoming(self) -> Style {
        Style::default()
            .fg(self.incoming)
            .add_modifier(Modifier::BOLD)
    }

    pub fn status_bar(self) -> Style {
        Style::default().fg(self.foreground).bg(Color::Black)
    }

    pub fn status_key(self) -> Style {
        Style::default()
            .fg(self.accent)
            .bg(Color::Black)
            .add_modifier(Modifier::BOLD)
    }

    pub fn error(self) -> Style {
        Style::default().fg(self.error)
    }

    pub fn overlay_border(self) -> Style {
        Style::default().fg(self.overlay)
    }

    pub fn help_overlay_border(self) -> Style {
        Style::default().fg(self.muted)
    }
}
