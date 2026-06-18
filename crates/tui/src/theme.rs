use ratatui::style::{Color, Modifier, Style};
use storage::UiThemePreset;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    pub background: Color,
    pub foreground: Color,
    pub muted: Color,
    /// De-emphasized foreground for low-priority (read/unimportant) text. Sits
    /// between `foreground` and `muted` so unread items clearly stand out.
    pub subtle: Color,
    pub accent: Color,
    pub incoming: Color,
    pub outgoing: Color,
    pub unread: Color,
    pub warning: Color,
    pub error: Color,
    pub overlay: Color,
    /// Border color for unfocused panes and overlays.
    pub border: Color,
    /// Foreground used for the currently selected row.
    pub selection: Color,
    /// Background fill for the currently selected row.
    pub selection_bg: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::from_preset(UiThemePreset::default())
    }
}

impl Theme {
    pub fn from_preset(preset: UiThemePreset) -> Self {
        match preset {
            // Modern "mocha" dark palette. Background stays `Reset` so users
            // who run a transparent or themed terminal keep their backdrop.
            UiThemePreset::DefaultDark => Self {
                background: Color::Reset,
                foreground: Color::Rgb(205, 214, 244),
                muted: Color::Rgb(108, 112, 134),
                subtle: Color::Rgb(130, 136, 160),
                accent: Color::Rgb(180, 190, 254),
                incoming: Color::Rgb(166, 227, 161),
                outgoing: Color::Rgb(137, 180, 250),
                unread: Color::Rgb(249, 226, 175),
                warning: Color::Rgb(250, 179, 135),
                error: Color::Rgb(243, 139, 168),
                overlay: Color::Rgb(137, 180, 250),
                border: Color::Rgb(69, 71, 90),
                selection: Color::Rgb(205, 214, 244),
                selection_bg: Color::Rgb(49, 50, 68),
            },
            // Soft "latte" light palette tuned for readable contrast on paper.
            UiThemePreset::Light => Self {
                background: Color::Rgb(239, 241, 245),
                foreground: Color::Rgb(76, 79, 105),
                muted: Color::Rgb(140, 143, 161),
                subtle: Color::Rgb(122, 126, 146),
                accent: Color::Rgb(30, 102, 245),
                incoming: Color::Rgb(64, 160, 43),
                outgoing: Color::Rgb(32, 159, 181),
                unread: Color::Rgb(223, 142, 29),
                warning: Color::Rgb(254, 100, 11),
                error: Color::Rgb(210, 15, 57),
                overlay: Color::Rgb(30, 102, 245),
                border: Color::Rgb(188, 192, 204),
                selection: Color::Rgb(76, 79, 105),
                selection_bg: Color::Rgb(220, 224, 232),
            },
            // Maximum legibility: pure black/white with saturated accents.
            UiThemePreset::HighContrast => Self {
                background: Color::Rgb(0, 0, 0),
                foreground: Color::Rgb(255, 255, 255),
                muted: Color::Rgb(176, 176, 176),
                subtle: Color::Rgb(160, 160, 160),
                accent: Color::Rgb(255, 224, 0),
                incoming: Color::Rgb(0, 255, 128),
                outgoing: Color::Rgb(0, 200, 255),
                unread: Color::Rgb(255, 224, 0),
                warning: Color::Rgb(255, 160, 0),
                error: Color::Rgb(255, 80, 80),
                overlay: Color::Rgb(255, 224, 0),
                border: Color::Rgb(208, 208, 208),
                selection: Color::Rgb(0, 0, 0),
                selection_bg: Color::Rgb(255, 224, 0),
            },
            // WhatsApp dark brand colors (teal/green on near-black slate).
            UiThemePreset::WhatsApp => Self {
                background: Color::Rgb(11, 20, 26),
                foreground: Color::Rgb(233, 237, 239),
                muted: Color::Rgb(134, 150, 160),
                subtle: Color::Rgb(120, 136, 146),
                accent: Color::Rgb(0, 168, 132),
                incoming: Color::Rgb(102, 224, 177),
                outgoing: Color::Rgb(37, 211, 102),
                unread: Color::Rgb(37, 211, 102),
                warning: Color::Rgb(255, 193, 7),
                error: Color::Rgb(240, 90, 90),
                overlay: Color::Rgb(0, 168, 132),
                border: Color::Rgb(38, 52, 60),
                selection: Color::Rgb(233, 237, 239),
                selection_bg: Color::Rgb(32, 44, 51),
            },
            // Slack "aubergine" brand colors over a dark workspace background.
            UiThemePreset::Slack => Self {
                background: Color::Rgb(26, 29, 33),
                foreground: Color::Rgb(209, 210, 211),
                muted: Color::Rgb(149, 152, 154),
                subtle: Color::Rgb(130, 132, 136),
                accent: Color::Rgb(54, 197, 240),
                incoming: Color::Rgb(46, 182, 125),
                outgoing: Color::Rgb(188, 131, 200),
                unread: Color::Rgb(236, 178, 46),
                warning: Color::Rgb(255, 140, 40),
                error: Color::Rgb(224, 30, 90),
                overlay: Color::Rgb(188, 131, 200),
                border: Color::Rgb(60, 63, 67),
                selection: Color::Rgb(209, 210, 211),
                selection_bg: Color::Rgb(74, 21, 75),
            },
        }
    }
    pub fn focus_border(self, focused: bool) -> Style {
        if focused {
            Style::default().fg(self.accent)
        } else {
            Style::default().fg(self.border)
        }
    }

    /// Style applied to the currently selected row in lists.
    pub fn selection(self) -> Style {
        Style::default().fg(self.selection).bg(self.selection_bg)
    }

    pub fn pane_title(self) -> Style {
        Style::default()
            .fg(self.foreground)
            .add_modifier(Modifier::BOLD)
    }

    /// Pane title style. The focused pane uses the accent color in bold so it
    /// clearly stands out; unfocused panes use the dimmer `subtle` tone with no
    /// bold, so their titles stay readable without competing for attention.
    pub fn pane_title_for(self, focused: bool) -> Style {
        if focused {
            Style::default()
                .fg(self.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.subtle)
        }
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
        Style::default().fg(self.foreground)
    }

    pub fn status_key(self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    pub fn error(self) -> Style {
        Style::default().fg(self.error)
    }

    pub fn overlay_border(self) -> Style {
        Style::default().fg(self.overlay)
    }

    pub fn help_overlay_border(self) -> Style {
        Style::default().fg(self.border)
    }
}
