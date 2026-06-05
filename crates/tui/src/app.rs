use crate::{
    event::AppEvent,
    theme::Theme,
    widgets::{chat_list, message_list},
};
use anyhow::{Result, anyhow};
use arboard::Clipboard;
use chat_core::{
    Account, AuthChallenge, Chat, Content, Media, Message, MessageId, PlatformData, Provider,
    ProviderEvent, ProviderId, Reaction, Sender,
};
use chrono::Utc;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CrosstermEvent, KeyCode, KeyEvent,
        KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use ratatui_textarea::{Input as TextAreaInput, Key as TextAreaKey, TextArea};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use storage::Store;
use tokio::sync::broadcast;

const IDLE_POLL_TIMEOUT: Duration = Duration::from_millis(250);
const HISTORY_LIMIT: usize = 50;
const MOUSE_SCROLL_STEP: usize = 3;
const MESSAGE_SCROLL_STEP: usize = 3;
const IMAGE_VIEWER_MAX_WIDTH: u16 = 96;
const REACTION_OPTIONS: [&str; 6] = ["👍", "❤️", "😂", "🎉", "😮", "🙏"];
const LOCAL_REACTION_SENDER: &str = "me";
const REACTION_OPTION_CELL_WIDTH: u16 = 6;
const NOTIFICATION_TICKS: u8 = 16;

pub type ProviderBox = Box<dyn Provider>;

pub async fn run(store: Arc<Store>, providers: Vec<ProviderBox>) -> Result<()> {
    let mut app = App::new(store, providers).await?;
    let mut terminal = init_terminal()?;
    let result = run_app_loop(&mut terminal, &mut app).await;
    restore_terminal(&mut terminal)?;
    result
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FocusPane {
    #[default]
    ChatList,
    Messages,
    Compose,
    Details,
}

impl FocusPane {
    fn label(self) -> &'static str {
        match self {
            Self::ChatList => "Chats",
            Self::Messages => "Messages",
            Self::Compose => "Compose",
            Self::Details => "Details",
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::ChatList => Self::ChatList,
            Self::Messages => Self::ChatList,
            Self::Compose => Self::Messages,
            Self::Details => Self::Compose,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::ChatList => Self::Messages,
            Self::Messages => Self::Compose,
            Self::Compose => Self::Details,
            Self::Details => Self::Details,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PaneAreas {
    chat_list: Rect,
    messages: Rect,
    compose: Rect,
    details: Rect,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum LayoutMode {
    Compact,
    Medium,
    #[default]
    Wide,
}

impl LayoutMode {
    fn label(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Medium => "medium",
            Self::Wide => "wide",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct AppLayout {
    mode: LayoutMode,
    chat_list: Rect,
    messages: Rect,
    compose: Rect,
    details: Rect,
    status: Rect,
}

impl AppLayout {
    fn for_area(area: Rect, compose_height: u16) -> Self {
        if area.is_empty() {
            return Self::default();
        }

        let [body, status] = *Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area)
        else {
            return Self::default();
        };

        match layout_mode(area) {
            LayoutMode::Wide => Self::wide(body, status, compose_height),
            LayoutMode::Medium => Self::medium(body, status, compose_height),
            LayoutMode::Compact => Self::compact(body, status, compose_height),
        }
    }

    fn wide(body: Rect, status: Rect, compose_height: u16) -> Self {
        let [chat_list, center, details] = *Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(30),
                Constraint::Percentage(45),
                Constraint::Percentage(25),
            ])
            .split(body)
        else {
            return Self::default();
        };
        let [messages, compose] = message_compose_split(center, compose_height);

        Self {
            mode: LayoutMode::Wide,
            chat_list,
            messages,
            compose,
            details,
            status,
        }
    }

    fn medium(body: Rect, status: Rect, compose_height: u16) -> Self {
        let [chat_list, center] = *Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(body)
        else {
            return Self::default();
        };
        let [messages, compose] = message_compose_split(center, compose_height);

        Self {
            mode: LayoutMode::Medium,
            chat_list,
            messages,
            compose,
            details: Rect::default(),
            status,
        }
    }

    fn compact(body: Rect, status: Rect, compose_height: u16) -> Self {
        let [messages, compose] = message_compose_split(body, compose_height);

        Self {
            mode: LayoutMode::Compact,
            chat_list: body,
            messages,
            compose,
            details: Rect::default(),
            status,
        }
    }
}

fn layout_mode(area: Rect) -> LayoutMode {
    if area.width < 72 {
        LayoutMode::Compact
    } else if area.width < 100 {
        LayoutMode::Medium
    } else {
        LayoutMode::Wide
    }
}

fn message_compose_split(area: Rect, compose_height: u16) -> [Rect; 2] {
    let compose_height = compose_height.min(area.height);
    let [messages, compose] = *Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(compose_height)])
        .split(area)
    else {
        return [Rect::default(), Rect::default()];
    };
    [messages, compose]
}

#[derive(Clone, Debug)]
struct ImageViewer {
    path: PathBuf,
    title: String,
    caption: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActionMenuItem {
    Reply,
    ViewThread,
    React,
    CopyText,
    OpenImage,
    Cancel,
}

impl ActionMenuItem {
    const ALL: [Self; 6] = [
        Self::Reply,
        Self::ViewThread,
        Self::React,
        Self::CopyText,
        Self::OpenImage,
        Self::Cancel,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Reply => "Reply",
            Self::ViewThread => "View thread",
            Self::React => "React",
            Self::CopyText => "Copy text",
            Self::OpenImage => "Open image",
            Self::Cancel => "Cancel",
        }
    }
}

#[derive(Clone, Debug)]
struct ActionMenu {
    message_id: MessageId,
    selected: usize,
}

#[derive(Clone, Debug)]
struct ReactionPicker {
    message_id: MessageId,
    selected: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AccountConnection {
    Connecting,
    Syncing(u8),
    Online,
    NeedsAuth,
    Reconnecting,
    Offline,
}

impl AccountConnection {
    fn label(&self) -> String {
        match self {
            Self::Connecting => "connecting".to_owned(),
            Self::Syncing(progress) => format!("syncing {progress}%"),
            Self::Online => "online".to_owned(),
            Self::NeedsAuth => "auth needed".to_owned(),
            Self::Reconnecting => "reconnecting".to_owned(),
            Self::Offline => "offline".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AccountStatus {
    display_name: String,
    connection: AccountConnection,
    detail: Option<String>,
}

impl AccountStatus {
    fn new(account: &Account, connection: AccountConnection) -> Self {
        Self {
            display_name: account.display_name.to_string(),
            connection,
            detail: None,
        }
    }

    fn summary(&self) -> String {
        let mut summary = format!("{} {}", self.display_name, self.connection.label());
        if let Some(detail) = &self.detail
            && !detail.is_empty()
        {
            summary.push_str(": ");
            summary.push_str(detail);
        }
        summary
    }
}

#[derive(Clone, Debug)]
struct AccountSwitcher {
    selected: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AccountOption {
    provider_id: Option<ProviderId>,
    label: String,
    summary: String,
    chat_count: usize,
}

#[derive(Clone, Debug)]
struct NotificationOverlay {
    message_id: MessageId,
    chat_name: String,
    sender_name: String,
    preview: String,
    ticks_remaining: u8,
}

impl NotificationOverlay {
    fn new(chat: &Chat, message: &Message) -> Self {
        Self {
            message_id: message.id.clone(),
            chat_name: chat.name.to_string(),
            sender_name: message.sender.display_name.to_string(),
            preview: notification_preview(message),
            ticks_remaining: NOTIFICATION_TICKS,
        }
    }
}

#[derive(Debug)]
pub struct AppState {
    chats: Vec<Chat>,
    visible_chat_indices: Vec<usize>,
    messages: Vec<Message>,
    selected_chat: usize,
    filter: String,
    filter_mode: bool,
    compose: TextArea<'static>,
    compose_text: String,
    compose_cursor: usize,
    focus: FocusPane,
    layout_mode: LayoutMode,
    pane_areas: PaneAreas,
    media_hits: Vec<message_list::MediaHit>,
    message_hits: Vec<message_list::MessageHit>,
    selected_message_id: Option<MessageId>,
    action_menu: Option<ActionMenu>,
    reaction_picker: Option<ReactionPicker>,
    account_switcher: Option<AccountSwitcher>,
    active_account: Option<ProviderId>,
    notification: Option<NotificationOverlay>,
    account_statuses: HashMap<ProviderId, AccountStatus>,
    reply_to: Option<MessageId>,
    thread_root: Option<MessageId>,
    image_viewer: Option<ImageViewer>,
    message_scroll: usize,
    frame_area: Rect,
    should_quit: bool,
    status: String,
}

impl Default for AppState {
    fn default() -> Self {
        let mut state = Self {
            chats: Vec::new(),
            visible_chat_indices: Vec::new(),
            messages: Vec::new(),
            selected_chat: 0,
            filter: String::new(),
            filter_mode: false,
            compose: new_compose_textarea(),
            compose_text: String::new(),
            compose_cursor: 0,
            focus: FocusPane::ChatList,
            layout_mode: LayoutMode::Wide,
            pane_areas: PaneAreas::default(),
            media_hits: Vec::new(),
            message_hits: Vec::new(),
            selected_message_id: None,
            action_menu: None,
            reaction_picker: None,
            account_switcher: None,
            active_account: None,
            notification: None,
            account_statuses: HashMap::new(),
            reply_to: None,
            thread_root: None,
            image_viewer: None,
            message_scroll: 0,
            frame_area: Rect::default(),
            should_quit: false,
            status: String::new(),
        };
        state.sync_compose_cache();
        state
    }
}

impl AppState {
    fn sync_compose_cache(&mut self) {
        self.compose_text = self.compose.lines().join("\n");
        self.compose_cursor = textarea_byte_cursor(&self.compose);
    }

    pub fn chats(&self) -> &[Chat] {
        &self.chats
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn visible_chat_indices(&self) -> &[usize] {
        &self.visible_chat_indices
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn filter_mode(&self) -> bool {
        self.filter_mode
    }

    pub fn focus(&self) -> FocusPane {
        self.focus
    }

    pub fn compose_text(&self) -> &str {
        &self.compose_text
    }

    pub fn compose_cursor(&self) -> usize {
        self.compose_cursor
    }

    pub fn selected_chat_index(&self) -> usize {
        self.selected_chat
    }

    pub fn message_scroll(&self) -> usize {
        self.message_scroll
    }

    pub fn image_viewer_open(&self) -> bool {
        self.image_viewer.is_some()
    }

    pub fn media_hit_count(&self) -> usize {
        self.media_hits.len()
    }

    pub fn selected_message_id(&self) -> Option<&MessageId> {
        self.selected_message_id.as_ref()
    }

    pub fn action_menu_open(&self) -> bool {
        self.action_menu.is_some()
    }

    pub fn reaction_picker_open(&self) -> bool {
        self.reaction_picker.is_some()
    }

    pub fn account_switcher_open(&self) -> bool {
        self.account_switcher.is_some()
    }

    pub fn active_account(&self) -> Option<&ProviderId> {
        self.active_account.as_ref()
    }

    pub fn account_switcher_selected(&self) -> Option<usize> {
        self.account_switcher
            .as_ref()
            .map(|switcher| switcher.selected)
    }

    pub fn notification_visible(&self) -> bool {
        self.notification.is_some()
    }

    pub fn notification_preview(&self) -> Option<&str> {
        self.notification
            .as_ref()
            .map(|notification| notification.preview.as_str())
    }

    pub fn account_status_summary(&self) -> String {
        account_status_summary(&self.account_statuses)
    }

    pub fn reply_to(&self) -> Option<&MessageId> {
        self.reply_to.as_ref()
    }

    pub fn thread_root(&self) -> Option<&MessageId> {
        self.thread_root.as_ref()
    }

    pub fn selected_chat(&self) -> Option<&Chat> {
        if self.visible_chat_indices.contains(&self.selected_chat) {
            self.chats.get(self.selected_chat)
        } else {
            None
        }
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn provider_for_selected_chat(&self) -> Option<&ProviderId> {
        self.selected_chat().map(|chat| &chat.account)
    }
}

pub struct App {
    providers: Vec<ProviderBox>,
    provider_receivers: Vec<(ProviderId, broadcast::Receiver<ProviderEvent>)>,
    store: Arc<Store>,
    state: AppState,
    media_preview_cache: message_list::MediaPreviewCache,
    theme: Theme,
}

impl App {
    pub async fn new(store: Arc<Store>, providers: Vec<ProviderBox>) -> Result<Self> {
        let provider_receivers = providers
            .iter()
            .map(|provider| (provider.id().clone(), provider.events()))
            .collect();
        let mut app = Self {
            providers,
            provider_receivers,
            store,
            state: AppState::default(),
            media_preview_cache: message_list::MediaPreviewCache::default(),
            theme: Theme::default(),
        };
        app.bootstrap().await?;
        Ok(app)
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    pub async fn handle_event(&mut self, event: AppEvent) -> Result<()> {
        match event {
            AppEvent::Key(key) => {
                self.dismiss_notification();
                if self.handle_key(key).await? {
                    self.reload_selected_messages().await?;
                    self.scroll_messages_to_bottom();
                }
            }
            AppEvent::Mouse(mouse) => {
                self.dismiss_notification();
                if self.handle_mouse(mouse).await? {
                    self.reload_selected_messages().await?;
                    self.scroll_messages_to_bottom();
                    if self.state.focus == FocusPane::Messages {
                        self.mark_selected_chat_read().await?;
                    }
                }
            }
            AppEvent::Resize(width, height) => {
                self.dismiss_notification();
                self.state.status = format!("terminal resized to {width}x{height}");
            }
            AppEvent::Tick => self.handle_tick(),
            AppEvent::Provider(provider_id, event) => {
                self.handle_provider_event(provider_id, *event).await?;
            }
            AppEvent::MediaReady(message_id, path) => {
                self.state.status = format!("media ready for {message_id}: {}", path.display());
            }
        }
        Ok(())
    }

    pub async fn drain_provider_events(&mut self) -> Result<bool> {
        let mut events = Vec::new();
        for (provider_id, receiver) in &mut self.provider_receivers {
            loop {
                match receiver.try_recv() {
                    Ok(event) => {
                        events.push(AppEvent::Provider(provider_id.clone(), Box::new(event)))
                    }
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                    Err(broadcast::error::TryRecvError::Closed) => break,
                }
            }
        }

        let had_events = !events.is_empty();
        for event in events {
            self.handle_event(event).await?;
        }
        Ok(had_events)
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        self.state.frame_area = frame.area();
        let layout = AppLayout::for_area(frame.area(), self.compose_height(frame.area()));
        self.state.layout_mode = layout.mode;
        self.state.pane_areas = self.visible_pane_areas(layout);

        match layout.mode {
            LayoutMode::Compact => self.draw_compact(frame, layout),
            LayoutMode::Medium => {
                self.draw_chat_list(frame, layout.chat_list);
                self.draw_messages(frame, layout.messages);
                self.draw_compose(frame, layout.compose);
            }
            LayoutMode::Wide => {
                self.draw_chat_list(frame, layout.chat_list);
                self.draw_messages(frame, layout.messages);
                self.draw_compose(frame, layout.compose);
                self.draw_details(frame, layout.details);
            }
        }
        self.draw_status_bar(frame, layout.status);
        self.draw_notification_overlay(frame, frame.area());
        self.draw_account_switcher(frame, frame.area());
        self.draw_image_viewer(frame, frame.area());
        self.draw_action_menu(frame, frame.area());
        self.draw_reaction_picker(frame, frame.area());
    }

    fn visible_pane_areas(&self, layout: AppLayout) -> PaneAreas {
        match layout.mode {
            LayoutMode::Compact => match self.state.focus {
                FocusPane::ChatList => PaneAreas {
                    chat_list: layout.chat_list,
                    ..PaneAreas::default()
                },
                FocusPane::Messages | FocusPane::Compose => PaneAreas {
                    messages: layout.messages,
                    compose: layout.compose,
                    ..PaneAreas::default()
                },
                FocusPane::Details => PaneAreas {
                    details: layout.chat_list,
                    ..PaneAreas::default()
                },
            },
            LayoutMode::Medium => PaneAreas {
                chat_list: layout.chat_list,
                messages: layout.messages,
                compose: layout.compose,
                details: Rect::default(),
            },
            LayoutMode::Wide => PaneAreas {
                chat_list: layout.chat_list,
                messages: layout.messages,
                compose: layout.compose,
                details: layout.details,
            },
        }
    }

    fn draw_compact(&mut self, frame: &mut Frame<'_>, layout: AppLayout) {
        match self.state.focus {
            FocusPane::ChatList => self.draw_chat_list(frame, layout.chat_list),
            FocusPane::Messages | FocusPane::Compose => {
                self.draw_messages(frame, layout.messages);
                self.draw_compose(frame, layout.compose);
            }
            FocusPane::Details => self.draw_details(frame, layout.chat_list),
        }
    }

    fn draw_chat_list(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        if area.is_empty() {
            return;
        }

        let avatar_rows = self.chat_avatar_rows();
        chat_list::render_chat_list(
            frame,
            area,
            chat_list::ChatListProps {
                chats: &self.state.chats,
                visible_chat_indices: &self.state.visible_chat_indices,
                selected_chat_index: self.state.selected_chat,
                filter: &self.state.filter,
                filter_mode: self.state.filter_mode,
                account_filter: &self.account_filter_label(),
                focused: self.state.focus == FocusPane::ChatList,
                avatar_rows: &avatar_rows,
                theme: self.theme,
            },
        );
        self.draw_vertical_scrollbar(
            frame,
            area,
            chat_list::content_height(&self.state.chats, &self.state.visible_chat_indices),
            chat_list::scroll_position(
                &self.state.chats,
                &self.state.visible_chat_indices,
                self.state.selected_chat,
                area,
            ),
        );
    }

    fn chat_avatar_rows(&mut self) -> HashMap<usize, chat_list::AvatarRows> {
        let mut rows = HashMap::new();
        let rendered_chat_indices = chat_list::rendered_chat_indices(
            &self.state.chats,
            &self.state.visible_chat_indices,
            self.state.selected_chat,
            self.state.pane_areas.chat_list,
        );
        for chat_index in rendered_chat_indices {
            let Some(path) = self
                .state
                .chats
                .get(chat_index)
                .and_then(|chat| chat.avatar.as_deref())
                .filter(|path| path.exists())
                .map(Path::to_path_buf)
            else {
                continue;
            };

            if let Ok(avatar) = message_list::cached_image_preview_rows(
                &path,
                &mut self.media_preview_cache,
                chat_list::CHAT_AVATAR_WIDTH,
                chat_list::CHAT_AVATAR_ROWS,
            ) {
                rows.insert(chat_index, avatar);
            }
        }
        rows
    }

    fn draw_messages(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        if area.is_empty() {
            return;
        }

        let title = self
            .state
            .selected_chat()
            .map(|chat| format!("Messages - {}", chat.name))
            .unwrap_or_else(|| "Messages".to_owned());
        let total_lines = if self.state.messages.is_empty() {
            0
        } else {
            self.message_line_count()
        };
        let lines = if self.state.messages.is_empty() {
            self.state.media_hits.clear();
            vec![Line::from(Span::styled(
                "No messages loaded. Start with --mock to seed sample chats.",
                self.theme.muted(),
            ))]
        } else {
            let render = message_list::build_message_lines(
                &self.state.messages,
                area.width.saturating_sub(2),
                self.state.message_scroll,
                self.state.selected_message_id.as_deref(),
                &mut self.media_preview_cache,
                self.theme,
            );
            self.state.media_hits = render.media_hits;
            self.state.message_hits = render.message_hits;
            render.lines
        };
        message_list::render_message_list(
            frame,
            area,
            message_list::MessageListProps {
                title: &title,
                lines,
                total_lines,
                scroll: self.state.message_scroll,
                focused: self.state.focus == FocusPane::Messages,
                theme: self.theme,
            },
        );
    }

    fn compose_height(&self, area: Rect) -> u16 {
        if area.height < 8 {
            return area.height.min(3);
        }

        let content_lines = self.state.compose.lines().len() as u16;
        let reply_extra = u16::from(self.state.reply_to.is_some());
        let wrapped_extra = self
            .state
            .compose
            .lines()
            .iter()
            .map(|line| {
                let width = area.width.saturating_div(2).max(24) as usize;
                line.chars().count().saturating_div(width)
            })
            .sum::<usize>() as u16;
        (content_lines + wrapped_extra + reply_extra + 2).clamp(3, 7)
    }

    fn draw_compose(&self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        if area.is_empty() {
            return;
        }

        let title = self
            .state
            .selected_chat()
            .map(|chat| format!("Compose - {}", chat.name))
            .unwrap_or_else(|| "Compose".to_owned());
        let is_focused = self.state.focus == FocusPane::Compose;
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(self.theme.focus_border(is_focused));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let editor_area = if let Some(reply_to) = &self.state.reply_to {
            let preview = self
                .message_by_id(reply_to)
                .map(reply_preview)
                .unwrap_or_else(|| format!("Replying to {reply_to}"));
            let [reply_area, editor_area] = *Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(0)])
                .split(inner)
            else {
                return;
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Replying · ", self.theme.status_key()),
                    Span::styled(preview, self.theme.muted()),
                ])),
                reply_area,
            );
            editor_area
        } else {
            inner
        };

        let mut compose = self.state.compose.clone();
        compose.remove_block();
        compose.set_style(Style::default().fg(self.theme.foreground));
        compose.set_cursor_line_style(if is_focused {
            Style::default().bg(Color::Black)
        } else {
            Style::default()
        });
        compose.set_cursor_style(if is_focused {
            Style::default().fg(Color::Black).bg(self.theme.accent)
        } else {
            Style::default()
        });
        compose.set_placeholder_text("Type a message...");
        compose.set_placeholder_style(self.theme.muted());
        frame.render_widget(&compose, editor_area);
    }

    fn draw_details(&self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        if area.is_empty() {
            return;
        }

        if let Some(thread_root) = &self.state.thread_root {
            self.draw_thread_details(frame, area, thread_root);
            return;
        }

        let selected_chat = self
            .state
            .selected_chat()
            .map(|chat| chat.name.as_ref())
            .unwrap_or("None");
        let filter = if self.state.filter.is_empty() {
            "none".to_owned()
        } else {
            self.state.filter.clone()
        };
        let mode = if self.state.filter_mode {
            "filtering chats"
        } else {
            "normal"
        };
        let details = vec![
            Line::from(format!("Selected: {selected_chat}")),
            Line::from(format!(
                "Focus: {} · Mode: {mode}",
                self.state.focus.label()
            )),
            Line::from(format!("Layout: {}", self.state.layout_mode.label())),
            Line::from(format!("Account: {}", self.state.account_status_summary())),
            Line::from(format!("Account filter: {}", self.account_filter_label())),
            Line::from(format!("Filter: {filter}")),
            Line::from(format!(
                "Chats: {}/{} · Messages: {}",
                self.state.visible_chat_indices.len(),
                self.state.chats.len(),
                self.state.messages.len()
            )),
            Line::from(format!(
                "Message scroll: line {}",
                self.state.message_scroll + 1
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Discoverable controls",
                self.theme.pane_title(),
            )),
            Line::from("  Arrows: move/edit text"),
            Line::from("  Click/tap: focus/open"),
            Line::from("  Scroll/trackpad: browse"),
            Line::from("  Click compose: type"),
            Line::from("  Enter: send"),
            Line::from("  Shift/Alt+Enter: newline"),
            Line::from("  Ctrl+J: newline fallback"),
            Line::from("  Esc: back/close/clear"),
            Line::from("  Ctrl+A: account filter"),
            Line::from("  Ctrl+F: text filter"),
            Line::from("  PageUp/PageDown: faster"),
            Line::from("  Home/End: edges"),
            Line::from("  Ctrl+Q: quit"),
            Line::from(""),
            Line::from(format!("Status: {}", self.state.status)),
        ];
        let paragraph = Paragraph::new(details).block(
            Block::default()
                .title("Details")
                .borders(Borders::ALL)
                .border_style(
                    self.theme
                        .focus_border(self.state.focus == FocusPane::Details),
                ),
        );
        frame.render_widget(paragraph, area);
    }

    fn draw_thread_details(
        &self,
        frame: &mut Frame<'_>,
        area: ratatui::layout::Rect,
        thread_root: &MessageId,
    ) {
        let root = self.message_by_id(thread_root);
        let replies = self.thread_replies(thread_root);
        let mut lines = Vec::new();

        lines.push(Line::from(Span::styled("Thread", self.theme.pane_title())));
        lines.push(Line::from(Span::styled(
            "Esc closes thread · Reply starts from message actions",
            self.theme.muted(),
        )));
        lines.push(Line::from(""));

        if let Some(root) = root {
            lines.push(Line::from(Span::styled(
                "Original",
                self.theme.status_key(),
            )));
            lines.extend(thread_message_lines(root, self.theme));
        } else {
            lines.push(Line::from(Span::styled(
                format!("Original message {} is not loaded", short_id(thread_root)),
                self.theme.muted(),
            )));
        }

        lines.push(Line::from(""));
        let reply_title = if replies.len() == 1 {
            "1 reply".to_owned()
        } else {
            format!("{} replies", replies.len())
        };
        lines.push(Line::from(Span::styled(
            reply_title,
            self.theme.status_key(),
        )));
        if replies.is_empty() {
            lines.push(Line::from(Span::styled(
                "No replies yet. Choose Reply from message actions to start one.",
                self.theme.muted(),
            )));
        } else {
            for reply in replies {
                lines.extend(thread_message_lines(reply, self.theme));
            }
        }

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Thread")
                    .borders(Borders::ALL)
                    .border_style(
                        self.theme
                            .focus_border(self.state.focus == FocusPane::Details),
                    ),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    fn draw_status_bar(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.is_empty() {
            return;
        }

        let hints = self.status_hints();
        let mut spans = Vec::new();
        spans.push(Span::styled(
            format!(" {} ", self.state.focus.label()),
            self.theme.status_key(),
        ));
        for hint in hints {
            spans.push(Span::styled(" · ", self.theme.muted()));
            spans.push(hint);
        }
        if !self.state.status.is_empty() {
            spans.push(Span::styled(" · ", self.theme.muted()));
            spans.push(Span::styled(
                format!("{} ", self.state.status),
                self.theme.muted(),
            ));
        }

        let paragraph = Paragraph::new(Line::from(spans)).style(self.theme.status_bar());
        frame.render_widget(paragraph, area);
    }

    fn status_hints(&self) -> Vec<Span<'static>> {
        let hint = |value: &'static str| Span::styled(value, self.theme.status_bar());

        if self.state.image_viewer.is_some() {
            return vec![hint("Click anywhere or press Esc to close")];
        }

        if self.state.account_switcher.is_some() {
            return vec![
                hint("Choose account"),
                hint("Arrow keys move"),
                hint("Enter applies"),
                hint("Esc cancels"),
            ];
        }

        if self.state.action_menu.is_some() {
            return vec![
                hint("Choose an action"),
                hint("Arrow keys move"),
                hint("Enter selects"),
                hint("Esc cancels"),
            ];
        }

        if self.state.reaction_picker.is_some() {
            return vec![
                hint("Choose reaction"),
                hint("Arrow keys move"),
                hint("Enter toggles"),
                hint("Esc cancels"),
            ];
        }

        if self.state.thread_root.is_some() {
            return vec![
                hint("Thread open"),
                hint("Esc closes"),
                hint("Reply from actions"),
            ];
        }

        if self.state.filter_mode {
            return vec![
                hint("Type to filter"),
                hint("Backspace deletes"),
                hint("Enter or Esc finishes"),
            ];
        }

        match self.state.focus {
            FocusPane::ChatList => vec![
                hint("↑↓ choose chat"),
                hint("Enter or click opens"),
                hint("Ctrl+A account filter"),
                hint("Ctrl+F text filter"),
            ],
            FocusPane::Messages => {
                if self.state.selected_message_id.is_some() {
                    vec![
                        hint("↑↓ selects messages"),
                        hint("Enter opens actions"),
                        hint("Esc clears selection"),
                    ]
                } else {
                    vec![
                        hint("Click messages to select"),
                        hint("Scroll to browse"),
                        hint("Type to reply"),
                    ]
                }
            }
            FocusPane::Compose => vec![
                hint("Enter sends"),
                hint("Ctrl+J adds a new line"),
                hint("Esc returns to messages"),
            ],
            FocusPane::Details => vec![hint("← returns"), hint("Ctrl+Q quits")],
        }
    }

    fn draw_vertical_scrollbar(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        content_len: usize,
        position: usize,
    ) {
        let viewport = inner_area(area).height as usize;
        if area.width < 3 || area.height < 3 || content_len <= viewport.max(1) {
            return;
        }

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .thumb_style(Style::default().fg(self.theme.muted))
            .track_style(Style::default().fg(Color::Black))
            .begin_symbol(None)
            .end_symbol(None);
        let mut state = ScrollbarState::new(content_len)
            .position(position)
            .viewport_content_length(viewport);
        frame.render_stateful_widget(scrollbar, area, &mut state);
    }

    fn draw_action_menu(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(menu) = &self.state.action_menu else {
            return;
        };

        let modal = self.action_menu_rect(area, &menu.message_id);
        if modal.is_empty() {
            return;
        }

        let mut lines = vec![Line::from(Span::styled(
            "Message actions",
            self.theme.pane_title(),
        ))];
        for (index, item) in ActionMenuItem::ALL.iter().enumerate() {
            let selected = index == menu.selected;
            let prefix = if selected { "› " } else { "  " };
            let style = if selected {
                self.theme.status_key()
            } else {
                self.theme.status_bar()
            };
            lines.push(Line::from(Span::styled(
                format!("{prefix}{}", item.label()),
                style,
            )));
        }
        lines.push(Line::from(Span::styled(
            "Enter selects · Esc cancels",
            self.theme.muted(),
        )));

        let paragraph = Paragraph::new(lines).block(
            Block::default()
                .title("Actions")
                .borders(Borders::ALL)
                .border_style(self.theme.overlay_border()),
        );
        frame.render_widget(Clear, modal);
        frame.render_widget(paragraph, modal);
    }

    fn draw_reaction_picker(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(picker) = &self.state.reaction_picker else {
            return;
        };

        let modal = self.reaction_picker_rect(area, &picker.message_id);
        if modal.is_empty() {
            return;
        }

        let reacted_by_me = self
            .message_by_id(&picker.message_id)
            .map(|message| {
                REACTION_OPTIONS
                    .iter()
                    .map(|emoji| message_reacted_by_sender(message, emoji, LOCAL_REACTION_SENDER))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![false; REACTION_OPTIONS.len()]);

        let mut spans = vec![Span::raw(" ")];
        for (index, emoji) in REACTION_OPTIONS.iter().enumerate() {
            let under_cursor = index == picker.selected;
            let already_selected = reacted_by_me.get(index).copied().unwrap_or_default();
            let label = if under_cursor {
                format!("›{emoji}‹")
            } else {
                format!(" {emoji} ")
            };
            let style = match (under_cursor, already_selected) {
                (true, true) => Style::default()
                    .fg(Color::Black)
                    .bg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
                (true, false) => self.theme.status_key(),
                (false, true) => Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
                (false, false) => self.theme.status_bar(),
            };
            spans.push(Span::styled(label, style));
            if index + 1 < REACTION_OPTIONS.len() {
                spans.push(Span::raw(" "));
            }
        }

        let paragraph = Paragraph::new(vec![
            Line::from(Span::styled("Choose reaction", self.theme.pane_title())),
            Line::from(spans),
            Line::from(Span::styled(
                "Enter toggles selected reaction",
                self.theme.muted(),
            )),
        ])
        .block(
            Block::default()
                .title("React")
                .borders(Borders::ALL)
                .border_style(self.theme.overlay_border()),
        );
        frame.render_widget(Clear, modal);
        frame.render_widget(paragraph, modal);
    }

    fn draw_notification_overlay(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(notification) = &self.state.notification else {
            return;
        };
        if area.width < 24 || area.height < 6 {
            return;
        }

        let width = area.width.saturating_sub(4).clamp(24, 48);
        let height = 5;
        let x = area.x + area.width.saturating_sub(width.saturating_add(1));
        let y = area.y.saturating_add(1);
        let popup = Rect::new(x, y, width, height);
        let text_width = popup.width.saturating_sub(4) as usize;
        let title = truncate_chars(&notification.chat_name, text_width);
        let sender = truncate_chars(&notification.sender_name, text_width);
        let preview = truncate_chars(&notification.preview, text_width);

        let paragraph = Paragraph::new(vec![
            Line::from(Span::styled("New message", self.theme.status_key())),
            Line::from(vec![
                Span::styled(sender, self.theme.incoming()),
                Span::styled(format!(" · {title}"), self.theme.muted()),
            ]),
            Line::from(Span::raw(preview)),
        ])
        .block(
            Block::default()
                .title("Notification")
                .borders(Borders::ALL)
                .border_style(self.theme.overlay_border()),
        )
        .wrap(Wrap { trim: true });
        frame.render_widget(Clear, popup);
        frame.render_widget(paragraph, popup);
    }

    fn draw_account_switcher(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(switcher) = &self.state.account_switcher else {
            return;
        };
        if area.width < 32 || area.height < 8 {
            return;
        }

        let options = self.account_options();
        let width = area.width.saturating_sub(4).clamp(32, 64);
        let height = (options.len() as u16).saturating_add(4).clamp(6, 14);
        let modal = centered_fixed_rect(area, width, height);
        let mut lines = vec![
            Line::from(Span::styled("Filter by account", self.theme.pane_title())),
            Line::from(Span::styled("Filters chats only", self.theme.muted())),
        ];
        for (index, option) in options.iter().enumerate() {
            let selected = index == switcher.selected;
            let marker = if selected { "›" } else { " " };
            let active = match (&self.state.active_account, &option.provider_id) {
                (None, None) => " •",
                (Some(active), Some(provider_id)) if active == provider_id => " •",
                _ => "  ",
            };
            let style = if selected {
                self.theme.status_key()
            } else {
                self.theme.status_bar()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker} "), style),
                Span::styled(truncate_chars(&option.label, 18), style),
                Span::styled(format!(" ({})", option.chat_count), self.theme.muted()),
                Span::styled(active, self.theme.status_key()),
                Span::styled(" - ", self.theme.muted()),
                Span::styled(truncate_chars(&option.summary, 24), self.theme.muted()),
            ]));
        }
        lines.push(Line::from(Span::styled(
            "Enter applies · Esc cancels",
            self.theme.muted(),
        )));

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Account filter")
                    .borders(Borders::ALL)
                    .border_style(self.theme.overlay_border()),
            )
            .wrap(Wrap { trim: true });
        frame.render_widget(Clear, modal);
        frame.render_widget(paragraph, modal);
    }

    fn draw_image_viewer(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(viewer) = &self.state.image_viewer else {
            return;
        };

        let modal = centered_rect(area, 84, 84);
        if modal.width < 12 || modal.height < 8 {
            return;
        }

        let preview_area = inner_area(modal);
        let preview_width = preview_area.width.clamp(1, IMAGE_VIEWER_MAX_WIDTH);
        let preview_rows = preview_area.height.saturating_sub(4).max(1);
        let preview = message_list::cached_image_preview_rows(
            &viewer.path,
            &mut self.media_preview_cache,
            preview_width,
            preview_rows,
        );
        let mut lines = vec![Line::from(Span::styled(
            truncate_chars(&viewer.path.display().to_string(), preview_width as usize),
            Style::default().fg(Color::DarkGray),
        ))];

        match preview {
            Ok(preview_rows) => lines.extend(preview_rows.into_iter().map(Line::from)),
            Err(error) => {
                lines.extend(
                    message_list::fallback_preview_rows(
                        preview_width,
                        preview_rows,
                        Color::DarkGray,
                        "image decode failed",
                    )
                    .into_iter()
                    .map(Line::from),
                );
                lines.push(Line::from(Span::styled(
                    error,
                    Style::default().fg(Color::Red),
                )));
            }
        }

        if let Some(caption) = &viewer.caption {
            lines.push(Line::from(format!("Caption: {caption}")));
        }
        lines.push(Line::from(Span::styled(
            "Click anywhere or press Esc to close",
            Style::default().fg(Color::DarkGray),
        )));

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!("Image Preview - {}", viewer.title))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(Clear, modal);
        frame.render_widget(paragraph, modal);
    }

    async fn bootstrap(&mut self) -> Result<()> {
        for provider in &self.providers {
            let account = provider.account_info();
            self.state.account_statuses.insert(
                account.id.clone(),
                AccountStatus::new(&account, AccountConnection::Connecting),
            );
            self.store.upsert_account(&account, "{}").await?;
            provider.connect().await?;
            if let Some(status) = self.state.account_statuses.get_mut(&account.id) {
                status.connection = AccountConnection::Syncing(0);
                status.detail = None;
            }

            for chat in provider.chats().await? {
                self.store.upsert_chat(&chat).await?;
                for message in provider.history(&chat.id, None, HISTORY_LIMIT).await? {
                    self.store.upsert_message(&message).await?;
                }
            }
            if let Some(status) = self.state.account_statuses.get_mut(&account.id) {
                status.connection = AccountConnection::Online;
                status.detail = None;
            }
        }

        self.reload_chats().await?;
        self.reload_selected_messages().await?;
        self.state.status = if self.state.chats.is_empty() {
            "ready - no chats loaded".to_owned()
        } else {
            "ready".to_owned()
        };
        Ok(())
    }

    async fn handle_provider_event(
        &mut self,
        provider_id: ProviderId,
        event: ProviderEvent,
    ) -> Result<()> {
        match event {
            ProviderEvent::Message {
                message,
                is_historical,
            } => {
                let selected_chat_id = self.state.selected_chat().map(|chat| chat.id.clone());
                let should_reload = selected_chat_id.as_ref() == Some(&message.chat_id);
                self.store.upsert_message(&message).await?;
                if should_reload {
                    self.reload_selected_messages().await?;
                }
                self.reload_chats().await?;
                self.maybe_show_notification(&message, selected_chat_id.as_ref(), is_historical);
                if !self
                    .state
                    .notification
                    .as_ref()
                    .is_some_and(|notification| notification.message_id == message.id)
                {
                    self.state.status = format!("message event from {provider_id}");
                }
            }
            ProviderEvent::MessageEdited { message } => {
                let selected_chat_id = self.state.selected_chat().map(|chat| chat.id.clone());
                let should_reload = selected_chat_id.as_ref() == Some(&message.chat_id);
                self.store.upsert_message(&message).await?;
                if should_reload {
                    self.reload_selected_messages().await?;
                }
                self.reload_chats().await?;
                self.state.status = format!("message edited from {provider_id}");
            }
            ProviderEvent::ChatUpdated(chat) => {
                self.store.upsert_chat(&chat).await?;
                self.reload_chats().await?;
                self.state.status = format!("chat updated from {provider_id}");
            }
            ProviderEvent::AuthRequired(challenge) => {
                self.set_account_status(
                    &provider_id,
                    AccountConnection::NeedsAuth,
                    Some(auth_challenge_label(&challenge).to_owned()),
                );
                self.state.status = format!("authentication required for {provider_id}");
            }
            ProviderEvent::AuthSucceeded => {
                self.set_account_status(&provider_id, AccountConnection::Online, None);
                self.state.status = format!("authenticated {provider_id}");
            }
            ProviderEvent::SyncProgress(progress) => {
                self.set_account_status(&provider_id, AccountConnection::Syncing(progress), None);
                self.state.status = format!("sync {provider_id}: {progress}%");
            }
            ProviderEvent::SyncComplete => {
                self.set_account_status(&provider_id, AccountConnection::Online, None);
                self.state.status = format!("sync complete for {provider_id}");
            }
            ProviderEvent::Disconnected(reason) => {
                let detail = reason.as_deref().map(str::to_owned);
                self.set_account_status(&provider_id, AccountConnection::Offline, detail.clone());
                self.state.status = detail
                    .map(|reason| format!("{provider_id} disconnected: {reason}"))
                    .unwrap_or_else(|| format!("{provider_id} disconnected"));
            }
            ProviderEvent::Reconnecting => {
                self.set_account_status(&provider_id, AccountConnection::Reconnecting, None);
                self.state.status = format!("reconnecting {provider_id}");
            }
            ProviderEvent::ReactionChanged {
                chat_id,
                message_id,
                emoji,
                added,
                sender,
            } => {
                let mut updated = None;
                if let Some(message) = self.message_by_id_mut(&message_id) {
                    if added {
                        add_reaction(message, emoji.as_ref(), sender);
                    } else {
                        remove_reaction(message, emoji.as_ref(), &sender);
                    }
                    updated = Some(message.clone());
                }
                if let Some(message) = updated {
                    self.store.upsert_message(&message).await?;
                } else if self
                    .state
                    .selected_chat()
                    .is_some_and(|chat| chat.id == chat_id)
                {
                    self.reload_selected_messages().await?;
                }
                self.state.status = format!("reaction updated from {provider_id}");
            }
            ProviderEvent::MessageDeleted { .. }
            | ProviderEvent::Receipt { .. }
            | ProviderEvent::Typing { .. } => {
                self.state.status = format!("event received from {provider_id}");
            }
        }
        Ok(())
    }

    async fn handle_action_menu_key(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(menu) = &mut self.state.action_menu else {
            return Ok(false);
        };

        match key.code {
            KeyCode::Esc => {
                self.state.action_menu = None;
                self.state.status = "message actions closed".to_owned();
            }
            KeyCode::Up => {
                menu.selected = menu.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                menu.selected = menu
                    .selected
                    .saturating_add(1)
                    .min(ActionMenuItem::ALL.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                let message_id = menu.message_id.clone();
                let item = ActionMenuItem::ALL[menu.selected];
                self.state.action_menu = None;
                self.perform_action_menu_item(message_id, item).await?;
            }
            _ => {}
        }
        Ok(false)
    }

    async fn handle_reaction_picker_key(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(picker) = &mut self.state.reaction_picker else {
            return Ok(false);
        };

        match key.code {
            KeyCode::Esc => {
                self.state.reaction_picker = None;
                self.state.status = "reaction cancelled".to_owned();
            }
            KeyCode::Left | KeyCode::Up => {
                picker.selected = picker.selected.saturating_sub(1);
            }
            KeyCode::Right | KeyCode::Down => {
                picker.selected = picker
                    .selected
                    .saturating_add(1)
                    .min(REACTION_OPTIONS.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                let message_id = picker.message_id.clone();
                let emoji = REACTION_OPTIONS[picker.selected];
                self.state.reaction_picker = None;
                self.apply_reaction(message_id, emoji).await?;
            }
            _ => {}
        }
        Ok(false)
    }

    async fn perform_action_menu_item(
        &mut self,
        message_id: MessageId,
        item: ActionMenuItem,
    ) -> Result<()> {
        match item {
            ActionMenuItem::Reply => self.start_reply(message_id),
            ActionMenuItem::ViewThread => self.open_thread(message_id),
            ActionMenuItem::React => {
                let selected = self
                    .message_by_id(&message_id)
                    .and_then(local_reaction_option)
                    .unwrap_or_default();
                self.state.reaction_picker = Some(ReactionPicker {
                    message_id,
                    selected,
                });
                self.state.status = "choose a reaction".to_owned();
            }
            ActionMenuItem::CopyText => self.copy_message_text(&message_id),
            ActionMenuItem::OpenImage => {
                if !self.open_message_image(&message_id) {
                    self.state.status = "selected message has no image preview".to_owned();
                }
            }
            ActionMenuItem::Cancel => {
                self.state.status = "message actions cancelled".to_owned();
            }
        }
        Ok(())
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if is_ctrl_char(key, 'q') {
            self.state.should_quit = true;
            return Ok(false);
        }

        if self.state.account_switcher.is_some() {
            return self.handle_account_switcher_key(key);
        }

        if self.state.action_menu.is_some() {
            return self.handle_action_menu_key(key).await;
        }

        if self.state.reaction_picker.is_some() {
            return self.handle_reaction_picker_key(key).await;
        }

        if self.state.filter_mode {
            return Ok(self.handle_filter_key(key));
        }

        if is_ctrl_char(key, 'a') {
            self.open_account_switcher();
            return Ok(false);
        }

        if is_ctrl_char(key, 'f') {
            return Ok(self.enter_filter_mode());
        }

        if self.state.focus == FocusPane::Compose {
            return self.handle_compose_key(key).await;
        }

        if self.state.focus == FocusPane::Messages
            && let KeyCode::Char(value) = key.code
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.state.focus = FocusPane::Compose;
            self.apply_compose_edit_input(textarea_input(TextAreaKey::Char(value), key.modifiers));
            self.state.status = "typing message".to_owned();
            return Ok(false);
        }

        let selection_changed = match self.state.focus {
            FocusPane::Messages => self.handle_message_key(key).await?,
            _ => match key.code {
                KeyCode::Esc => self.handle_escape(),
                KeyCode::Left => {
                    self.focus_previous_pane();
                    false
                }
                KeyCode::Right => {
                    self.focus_next_pane();
                    false
                }
                KeyCode::Enter => {
                    let changed = self.activate_selected_chat();
                    self.mark_selected_chat_read().await?;
                    changed
                }
                KeyCode::Down => self.select_next_chat(),
                KeyCode::Up => self.select_previous_chat(),
                KeyCode::Home => self.select_first_chat(),
                KeyCode::End => self.select_last_chat(),
                KeyCode::PageDown => self.page_down_chats(),
                KeyCode::PageUp => self.page_up_chats(),
                _ => false,
            },
        };
        Ok(selection_changed)
    }

    fn handle_account_switcher_key(&mut self, key: KeyEvent) -> Result<bool> {
        let options_len = self.account_options().len();
        let Some(switcher) = &mut self.state.account_switcher else {
            return Ok(false);
        };
        match key.code {
            KeyCode::Esc => {
                self.state.account_switcher = None;
                self.state.status = "account filter closed".to_owned();
                Ok(false)
            }
            KeyCode::Enter => {
                let selected = switcher.selected;
                Ok(self.apply_account_switcher_selection(selected))
            }
            KeyCode::Down => {
                switcher.selected = switcher
                    .selected
                    .saturating_add(1)
                    .min(options_len.saturating_sub(1));
                self.state.status = "choose account filter".to_owned();
                Ok(false)
            }
            KeyCode::Up => {
                switcher.selected = switcher.selected.saturating_sub(1);
                self.state.status = "choose account filter".to_owned();
                Ok(false)
            }
            KeyCode::Home => {
                switcher.selected = 0;
                self.state.status = "choose account filter".to_owned();
                Ok(false)
            }
            KeyCode::End => {
                switcher.selected = options_len.saturating_sub(1);
                self.state.status = "choose account filter".to_owned();
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    async fn handle_message_key(&mut self, key: KeyEvent) -> Result<bool> {
        let changed = match key.code {
            KeyCode::Esc => {
                if self.state.selected_message_id.take().is_some() {
                    self.state.action_menu = None;
                    self.state.reaction_picker = None;
                    self.state.status = "message selection cleared".to_owned();
                    false
                } else {
                    self.handle_escape()
                }
            }
            KeyCode::Left => {
                self.focus_previous_pane();
                false
            }
            KeyCode::Right => {
                self.focus_next_pane();
                false
            }
            KeyCode::Enter => {
                if self.state.selected_message_id.is_some() {
                    self.open_action_menu();
                }
                false
            }
            KeyCode::Down => {
                self.select_next_message();
                false
            }
            KeyCode::Up => {
                self.select_previous_message();
                false
            }
            KeyCode::Home => {
                self.select_first_message();
                false
            }
            KeyCode::End => {
                self.select_last_message();
                false
            }
            KeyCode::PageDown => {
                self.scroll_messages_down(MESSAGE_SCROLL_STEP * 2);
                false
            }
            KeyCode::PageUp => {
                self.scroll_messages_up(MESSAGE_SCROLL_STEP * 2);
                false
            }
            _ => false,
        };
        Ok(changed)
    }

    async fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<bool> {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.state.image_viewer.is_some()
        {
            self.close_image_viewer();
            return Ok(false);
        }

        if self.state.action_menu.is_some()
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            if self.handle_action_menu_click(mouse).await? {
                return Ok(false);
            }
            self.state.action_menu = None;
            self.state.status = "message actions closed".to_owned();
            return Ok(false);
        }

        if self.state.reaction_picker.is_some()
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            if self.handle_reaction_picker_click(mouse).await? {
                return Ok(false);
            }
            self.state.reaction_picker = None;
            self.state.status = "reaction picker closed".to_owned();
            return Ok(false);
        }

        if self.state.account_switcher.is_some()
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            if self.handle_account_switcher_click(mouse) {
                return Ok(true);
            }
            self.state.account_switcher = None;
            self.state.status = "account switcher closed".to_owned();
            return Ok(false);
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.state.focus == FocusPane::ChatList
            && self.status_bar_contains(mouse.column, mouse.row)
        {
            self.open_account_switcher();
            return Ok(false);
        }

        let Some(pane) = self.pane_at(mouse.column, mouse.row) else {
            return Ok(false);
        };

        if self.state.focus != pane {
            self.state.focus = pane;
        }

        let changed = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.handle_left_click(pane, mouse),
            MouseEventKind::ScrollDown => self.handle_scroll_down(pane),
            MouseEventKind::ScrollUp => self.handle_scroll_up(pane),
            MouseEventKind::ScrollLeft => {
                self.focus_previous_pane();
                false
            }
            MouseEventKind::ScrollRight => {
                self.focus_next_pane();
                false
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Moved => false,
        };
        Ok(changed)
    }

    async fn handle_action_menu_click(&mut self, mouse: MouseEvent) -> Result<bool> {
        let Some(menu) = self.state.action_menu.clone() else {
            return Ok(false);
        };
        let Some(index) = self.action_menu_item_at(mouse.column, mouse.row, &menu) else {
            return Ok(false);
        };

        let item = ActionMenuItem::ALL[index];
        self.state.action_menu = None;
        self.perform_action_menu_item(menu.message_id, item).await?;
        Ok(true)
    }

    async fn handle_reaction_picker_click(&mut self, mouse: MouseEvent) -> Result<bool> {
        let Some(picker) = self.state.reaction_picker.clone() else {
            return Ok(false);
        };
        let Some(index) = self.reaction_picker_option_at(mouse.column, mouse.row, &picker) else {
            return Ok(false);
        };

        let emoji = REACTION_OPTIONS[index];
        self.state.reaction_picker = None;
        self.apply_reaction(picker.message_id, emoji).await?;
        Ok(true)
    }

    fn handle_account_switcher_click(&mut self, mouse: MouseEvent) -> bool {
        let Some(index) = self.account_switcher_option_at(mouse.column, mouse.row) else {
            return false;
        };
        self.apply_account_switcher_selection(index)
    }

    fn handle_left_click(&mut self, pane: FocusPane, mouse: MouseEvent) -> bool {
        match pane {
            FocusPane::ChatList => {
                if let Some(chat_index) = chat_list::chat_at(
                    &self.state.chats,
                    &self.state.visible_chat_indices,
                    self.state.selected_chat,
                    self.state.pane_areas.chat_list,
                    mouse.column,
                    mouse.row,
                ) {
                    return self.activate_chat_index(chat_index);
                }
                self.state.status = "chat list focused".to_owned();
                false
            }
            FocusPane::Messages => {
                if self.open_media_at(mouse.column, mouse.row) {
                    return false;
                }
                if self.select_message_at(mouse.column, mouse.row) {
                    self.open_action_menu();
                    return false;
                }
                self.state.status = "messages focused".to_owned();
                false
            }
            FocusPane::Compose => {
                self.state.status = "compose focused".to_owned();
                false
            }
            FocusPane::Details => {
                self.state.status = "details focused".to_owned();
                false
            }
        }
    }

    fn handle_scroll_down(&mut self, pane: FocusPane) -> bool {
        match pane {
            FocusPane::ChatList => self.move_chat_selection(MOUSE_SCROLL_STEP as isize),
            FocusPane::Messages => {
                self.scroll_messages_down(MESSAGE_SCROLL_STEP);
                false
            }
            FocusPane::Compose | FocusPane::Details => false,
        }
    }

    fn handle_scroll_up(&mut self, pane: FocusPane) -> bool {
        match pane {
            FocusPane::ChatList => self.move_chat_selection(-(MOUSE_SCROLL_STEP as isize)),
            FocusPane::Messages => {
                self.scroll_messages_up(MESSAGE_SCROLL_STEP);
                false
            }
            FocusPane::Compose | FocusPane::Details => false,
        }
    }

    fn pane_at(&self, column: u16, row: u16) -> Option<FocusPane> {
        if rect_contains(self.state.pane_areas.chat_list, column, row) {
            Some(FocusPane::ChatList)
        } else if rect_contains(self.state.pane_areas.messages, column, row) {
            Some(FocusPane::Messages)
        } else if rect_contains(self.state.pane_areas.compose, column, row) {
            Some(FocusPane::Compose)
        } else if rect_contains(self.state.pane_areas.details, column, row) {
            Some(FocusPane::Details)
        } else {
            None
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Enter | KeyCode::Esc => {
                self.state.filter_mode = false;
                self.state.status = self.filter_status();
                false
            }
            KeyCode::Backspace => {
                self.state.filter.pop();
                let selection_changed = self.apply_filter();
                self.state.status = self.filter_status();
                selection_changed
            }
            KeyCode::Char(value)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.state.filter.push(value);
                let selection_changed = self.apply_filter();
                self.state.status = self.filter_status();
                selection_changed
            }
            _ => false,
        }
    }

    async fn handle_compose_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc => {
                self.state.focus = FocusPane::Messages;
                self.state.status = "compose closed".to_owned();
            }
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.insert_compose_newline();
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_compose_newline();
            }
            KeyCode::Enter => self.send_composed_message().await?,
            KeyCode::Backspace => {
                self.apply_compose_edit_input(textarea_input(
                    TextAreaKey::Backspace,
                    key.modifiers,
                ));
            }
            KeyCode::Delete => {
                self.apply_compose_edit_input(textarea_input(TextAreaKey::Delete, key.modifiers));
            }
            KeyCode::Left => {
                if self.compose_cursor_at_start() {
                    self.state.focus = FocusPane::Messages;
                    self.state.status = "focused Messages".to_owned();
                } else {
                    self.apply_compose_navigation_input(textarea_input(
                        TextAreaKey::Left,
                        key.modifiers,
                    ));
                }
            }
            KeyCode::Right => {
                if self.compose_cursor_at_end() {
                    self.state.focus = FocusPane::Details;
                    self.state.status = "focused Details".to_owned();
                } else {
                    self.apply_compose_navigation_input(textarea_input(
                        TextAreaKey::Right,
                        key.modifiers,
                    ));
                }
            }
            KeyCode::Up => {
                if self.compose_cursor_on_first_line() {
                    self.state.focus = FocusPane::Messages;
                    self.state.status = "focused Messages".to_owned();
                } else {
                    self.apply_compose_navigation_input(textarea_input(
                        TextAreaKey::Up,
                        key.modifiers,
                    ));
                }
            }
            KeyCode::Down => {
                if self.compose_cursor_on_last_line() {
                    self.state.status = "compose focused".to_owned();
                } else {
                    self.apply_compose_navigation_input(textarea_input(
                        TextAreaKey::Down,
                        key.modifiers,
                    ));
                }
            }
            KeyCode::Home => {
                self.apply_compose_navigation_input(textarea_input(
                    TextAreaKey::Home,
                    key.modifiers,
                ));
            }
            KeyCode::End => {
                self.apply_compose_navigation_input(textarea_input(
                    TextAreaKey::End,
                    key.modifiers,
                ));
            }
            KeyCode::Tab => {
                self.apply_compose_edit_input(textarea_input(TextAreaKey::Tab, key.modifiers));
            }
            KeyCode::Char(value)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.apply_compose_edit_input(textarea_input(
                    TextAreaKey::Char(value),
                    key.modifiers,
                ));
            }
            _ => {}
        }
        Ok(false)
    }

    fn apply_compose_edit_input(&mut self, input: TextAreaInput) -> bool {
        let modified = self.state.compose.input_without_shortcuts(input);
        self.state.sync_compose_cache();
        modified
    }

    fn insert_compose_newline(&mut self) {
        self.apply_compose_edit_input(textarea_input(TextAreaKey::Enter, KeyModifiers::NONE));
        self.state.status = "inserted newline".to_owned();
    }

    fn apply_compose_navigation_input(&mut self, input: TextAreaInput) -> bool {
        let modified = self.state.compose.input(input);
        self.state.sync_compose_cache();
        modified
    }

    fn compose_cursor_at_start(&self) -> bool {
        self.state.compose.cursor() == (0, 0)
    }

    fn compose_cursor_at_end(&self) -> bool {
        let (row, column) = self.state.compose.cursor();
        let lines = self.state.compose.lines();
        let Some(line) = lines.get(row) else {
            return true;
        };

        row + 1 >= lines.len() && column >= line.chars().count()
    }

    fn compose_cursor_on_first_line(&self) -> bool {
        self.state.compose.cursor().0 == 0
    }

    fn compose_cursor_on_last_line(&self) -> bool {
        let (row, _) = self.state.compose.cursor();
        row + 1 >= self.state.compose.lines().len()
    }

    async fn send_composed_message(&mut self) -> Result<()> {
        let text = self.state.compose_text.trim_end().to_owned();
        if text.trim().is_empty() {
            self.state.status = "type a message before sending".to_owned();
            return Ok(());
        }

        let Some(chat) = self.state.selected_chat().cloned() else {
            self.state.status = "select a chat before sending".to_owned();
            return Ok(());
        };
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.id().as_ref() == chat.account.as_ref())
            .ok_or_else(|| anyhow!("no provider registered for {}", chat.account))?;
        let account = provider.account_info();
        let content = Content::Text(Arc::from(text.as_str()));
        let reply_to = self.state.reply_to.clone();
        let message_id = provider
            .send(&chat.id, content.clone(), reply_to.as_ref())
            .await?;
        let timestamp = Utc::now();
        let message = Message {
            id: message_id,
            chat_id: chat.id.clone(),
            account: chat.account.clone(),
            sender: Sender {
                platform_id: Arc::from("me"),
                display_name: account.display_name,
                avatar: account.avatar,
            },
            timestamp,
            edited_at: None,
            content,
            reply_to: reply_to.clone(),
            thread_id: reply_to,
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me: true,
            platform_data: PlatformData::default(),
        };

        self.store.upsert_message(&message).await?;
        self.update_chat_after_send(&chat, timestamp, &text).await?;
        self.state.compose = new_compose_textarea();
        self.state.reply_to = None;
        self.state.sync_compose_cache();
        self.reload_chats().await?;
        self.reload_selected_messages().await?;
        self.scroll_messages_to_bottom();
        self.state.status = format!("sent message to {}", chat.name);
        Ok(())
    }

    async fn update_chat_after_send(
        &mut self,
        chat: &Chat,
        timestamp: chat_core::Timestamp,
        preview: &str,
    ) -> Result<()> {
        let updated_chat = self
            .state
            .chats
            .iter_mut()
            .find(|candidate| candidate.id == chat.id && candidate.account == chat.account)
            .map(|candidate| {
                candidate.last_message_at = Some(timestamp);
                candidate.last_message_preview = Some(Arc::from(preview));
                candidate.clone()
            });

        if let Some(updated_chat) = updated_chat {
            self.store.upsert_chat(&updated_chat).await?;
        }
        Ok(())
    }

    fn scroll_messages_to_bottom(&mut self) {
        self.state.message_scroll = self.max_message_scroll();
    }

    fn enter_filter_mode(&mut self) -> bool {
        self.state.focus = FocusPane::ChatList;
        self.state.filter_mode = true;
        self.state.status = "type to filter chats; Enter or Esc closes filtering".to_owned();
        false
    }

    fn handle_escape(&mut self) -> bool {
        if self.state.image_viewer.is_some() {
            self.close_image_viewer();
            return false;
        }

        if self.state.reaction_picker.is_some() {
            self.state.reaction_picker = None;
            self.state.status = "reaction cancelled".to_owned();
            return false;
        }

        if self.state.action_menu.is_some() {
            self.state.action_menu = None;
            self.state.status = "message actions closed".to_owned();
            return false;
        }

        if self.state.account_switcher.is_some() {
            self.state.account_switcher = None;
            self.state.status = "account switcher closed".to_owned();
            return false;
        }

        if self.state.thread_root.take().is_some() {
            self.state.status = "thread closed".to_owned();
            return false;
        }

        if self.state.reply_to.take().is_some() {
            self.state.status = "reply cancelled".to_owned();
            return false;
        }

        if self.state.selected_message_id.take().is_some() {
            self.state.status = "message selection cleared".to_owned();
            return false;
        }

        if self.state.focus != FocusPane::ChatList {
            self.state.focus = FocusPane::ChatList;
            self.state.status = "back to chat list".to_owned();
            return false;
        }

        self.clear_filter()
    }

    fn focus_previous_pane(&mut self) {
        let next_focus = self.state.focus.previous();
        if next_focus != self.state.focus {
            self.state.focus = next_focus;
            self.state.status = format!("focused {}", self.state.focus.label());
        }
    }

    fn focus_next_pane(&mut self) {
        let next_focus = self.state.focus.next();
        if next_focus != self.state.focus {
            self.state.focus = next_focus;
            self.state.status = format!("focused {}", self.state.focus.label());
        }
    }

    fn status_bar_contains(&self, column: u16, row: u16) -> bool {
        let area = self.state.frame_area;
        area.width > 0
            && area.height > 0
            && column >= area.x
            && column < area.x.saturating_add(area.width)
            && row == area.y.saturating_add(area.height.saturating_sub(1))
    }

    fn activate_selected_chat(&mut self) -> bool {
        let had_unread = self.selected_chat_has_unread();
        if let Some(chat_name) = self.state.selected_chat().map(|chat| chat.name.to_string()) {
            self.state.focus = FocusPane::Messages;
            self.state.status = format!("opened {chat_name}");
        }
        had_unread
    }

    fn activate_chat_index(&mut self, chat_index: usize) -> bool {
        let changed = chat_index != self.state.selected_chat;
        self.state.selected_chat = chat_index;
        self.state.focus = FocusPane::Messages;
        self.state.message_scroll = 0;
        if changed {
            self.state.selected_message_id = None;
            self.state.action_menu = None;
            self.state.reaction_picker = None;
            self.state.account_switcher = None;
            self.state.reply_to = None;
            self.state.thread_root = None;
        }
        self.state.image_viewer = None;

        if let Some(chat_name) = self.state.selected_chat().map(|chat| chat.name.to_string()) {
            self.state.status = format!("opened {chat_name}");
        }

        changed
            || self
                .state
                .chats
                .get(chat_index)
                .is_some_and(|chat| chat.unread_count > 0)
    }

    fn clear_filter(&mut self) -> bool {
        if self.state.filter.is_empty() {
            return false;
        }

        self.state.filter_mode = false;
        self.state.filter.clear();
        let selection_changed = self.apply_filter();
        self.state.status = "chat filter cleared".to_owned();
        selection_changed
    }

    fn select_next_chat(&mut self) -> bool {
        self.move_chat_selection(1)
    }

    fn select_previous_chat(&mut self) -> bool {
        self.move_chat_selection(-1)
    }

    fn select_first_chat(&mut self) -> bool {
        self.select_visible_position(0)
    }

    fn select_last_chat(&mut self) -> bool {
        let Some(last_position) = self.state.visible_chat_indices.len().checked_sub(1) else {
            return false;
        };
        self.select_visible_position(last_position)
    }

    fn page_down_chats(&mut self) -> bool {
        self.move_chat_selection(5)
    }

    fn page_up_chats(&mut self) -> bool {
        self.move_chat_selection(-5)
    }

    fn move_chat_selection(&mut self, offset: isize) -> bool {
        if self.state.visible_chat_indices.is_empty() {
            return false;
        }

        let current_position = self.selected_visible_position().unwrap_or(0);
        let last_position = self.state.visible_chat_indices.len() - 1;
        let next_position = if offset.is_negative() {
            current_position.saturating_sub(offset.unsigned_abs())
        } else {
            current_position
                .saturating_add(offset as usize)
                .min(last_position)
        };

        self.select_visible_position(next_position)
    }

    fn selected_visible_position(&self) -> Option<usize> {
        chat_list::selected_visible_position(
            &self.state.visible_chat_indices,
            self.state.selected_chat,
        )
    }

    fn select_visible_position(&mut self, position: usize) -> bool {
        let Some(&chat_index) = self.state.visible_chat_indices.get(position) else {
            return false;
        };
        let changed = chat_index != self.state.selected_chat;
        self.state.selected_chat = chat_index;
        if changed {
            self.state.message_scroll = 0;
            self.state.selected_message_id = None;
            self.state.action_menu = None;
            self.state.reaction_picker = None;
            self.state.account_switcher = None;
            self.state.reply_to = None;
            self.state.thread_root = None;
            self.state.image_viewer = None;
        }
        changed
            || self
                .state
                .chats
                .get(chat_index)
                .is_some_and(|chat| chat.unread_count > 0)
    }

    fn selected_chat_has_unread(&self) -> bool {
        self.state
            .selected_chat()
            .is_some_and(|chat| chat.unread_count > 0)
    }

    async fn mark_selected_chat_read(&mut self) -> Result<()> {
        let Some(chat) = self.state.selected_chat().cloned() else {
            return Ok(());
        };
        if chat.unread_count == 0 {
            return Ok(());
        }

        if let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.id().as_ref() == chat.account.as_ref())
            && let Some(message) = self.state.messages.last()
        {
            provider.mark_read(&chat.id, &message.id).await?;
        }

        if let Some(current) = self
            .state
            .chats
            .iter_mut()
            .find(|candidate| candidate.id == chat.id && candidate.account == chat.account)
        {
            current.unread_count = 0;
            let updated = current.clone();
            self.store.upsert_chat(&updated).await?;
        }
        Ok(())
    }

    fn scroll_messages_down(&mut self, amount: usize) {
        let max_scroll = self.max_message_scroll();
        self.state.message_scroll = self
            .state
            .message_scroll
            .saturating_add(amount)
            .min(max_scroll);
        self.state.status = if self.state.message_scroll == max_scroll {
            "showing latest messages".to_owned()
        } else {
            format!("showing message line {}", self.state.message_scroll + 1)
        };
    }

    fn scroll_messages_up(&mut self, amount: usize) {
        self.state.message_scroll = self.state.message_scroll.saturating_sub(amount);
        self.state.status = format!("showing message line {}", self.state.message_scroll + 1);
    }

    fn open_media_at(&mut self, column: u16, row: u16) -> bool {
        let content_area = inner_area(self.state.pane_areas.messages);
        if !rect_contains(content_area, column, row) {
            return false;
        }

        let clicked_line = self
            .state
            .message_scroll
            .saturating_add(row.saturating_sub(content_area.y) as usize);
        let Some(hit) = self
            .state
            .media_hits
            .iter()
            .find(|hit| {
                clicked_line >= hit.start_line
                    && clicked_line <= hit.end_line
                    && clicked_column_in_hit(content_area, column, hit.start_col, hit.end_col)
            })
            .cloned()
        else {
            return false;
        };

        self.state.action_menu = None;
        self.state.reaction_picker = None;
        self.state.status = format!("viewing image {}", hit.path.display());
        self.state.image_viewer = Some(ImageViewer {
            path: hit.path,
            title: hit.title,
            caption: hit.caption,
        });
        true
    }

    fn close_image_viewer(&mut self) {
        self.state.image_viewer = None;
        self.state.status = "image preview closed".to_owned();
    }

    fn select_message_at(&mut self, column: u16, row: u16) -> bool {
        let content_area = inner_area(self.state.pane_areas.messages);
        if !rect_contains(content_area, column, row) {
            return false;
        }

        let clicked_line = self
            .state
            .message_scroll
            .saturating_add(row.saturating_sub(content_area.y) as usize);
        let Some(hit) = self
            .state
            .message_hits
            .iter()
            .find(|hit| {
                hit.line_hits.iter().any(|line_hit| {
                    line_hit.line == clicked_line
                        && clicked_column_in_hit(
                            content_area,
                            column,
                            line_hit.start_col,
                            line_hit.end_col,
                        )
                })
            })
            .cloned()
        else {
            return false;
        };

        self.state.selected_message_id = Some(hit.message_id.clone());
        self.state.reaction_picker = None;
        self.state.status = format!("selected message {}", short_id(&hit.message_id));
        self.ensure_selected_message_visible();
        true
    }

    fn selected_message_index(&self) -> Option<usize> {
        let selected = self.state.selected_message_id.as_ref()?;
        self.state
            .messages
            .iter()
            .position(|message| message.id == *selected)
    }

    fn message_by_id(&self, message_id: &MessageId) -> Option<&Message> {
        self.state
            .messages
            .iter()
            .find(|message| message.id == *message_id)
    }

    fn message_by_id_mut(&mut self, message_id: &MessageId) -> Option<&mut Message> {
        self.state
            .messages
            .iter_mut()
            .find(|message| message.id == *message_id)
    }

    fn thread_replies(&self, thread_root: &MessageId) -> Vec<&Message> {
        self.state
            .messages
            .iter()
            .filter(|message| {
                message.reply_to.as_ref() == Some(thread_root)
                    || message.thread_id.as_ref() == Some(thread_root)
            })
            .filter(|message| message.id != *thread_root)
            .collect()
    }

    fn thread_reply_count(&self, thread_root: &MessageId) -> usize {
        self.thread_replies(thread_root).len()
    }

    fn select_first_message(&mut self) {
        let Some(message) = self.state.messages.first() else {
            self.state.status = "no messages to select".to_owned();
            return;
        };
        self.state.selected_message_id = Some(message.id.clone());
        self.state.status = "selected first message".to_owned();
        self.ensure_selected_message_visible();
    }

    fn select_last_message(&mut self) {
        let Some(message) = self.state.messages.last() else {
            self.state.status = "no messages to select".to_owned();
            return;
        };
        self.state.selected_message_id = Some(message.id.clone());
        self.state.status = "selected latest message".to_owned();
        self.ensure_selected_message_visible();
    }

    fn select_next_message(&mut self) {
        if self.state.messages.is_empty() {
            self.state.status = "no messages to select".to_owned();
            return;
        }

        let next = self
            .selected_message_index()
            .map(|index| index.saturating_add(1).min(self.state.messages.len() - 1))
            .unwrap_or(0);
        self.state.selected_message_id = Some(self.state.messages[next].id.clone());
        self.state.status = "selected next message".to_owned();
        self.ensure_selected_message_visible();
    }

    fn select_previous_message(&mut self) {
        if self.state.messages.is_empty() {
            self.state.status = "no messages to select".to_owned();
            return;
        }

        let previous = self
            .selected_message_index()
            .map(|index| index.saturating_sub(1))
            .unwrap_or_else(|| self.state.messages.len().saturating_sub(1));
        self.state.selected_message_id = Some(self.state.messages[previous].id.clone());
        self.state.status = "selected previous message".to_owned();
        self.ensure_selected_message_visible();
    }

    fn ensure_selected_message_visible(&mut self) {
        let Some(selected) = self.state.selected_message_id.as_deref() else {
            return;
        };
        let Some(hit) = self
            .state
            .message_hits
            .iter()
            .find(|hit| hit.message_id.as_ref() == selected)
        else {
            return;
        };
        let viewport_rows = inner_area(self.state.pane_areas.messages).height as usize;
        if viewport_rows == 0 {
            return;
        }
        if hit.start_line < self.state.message_scroll {
            self.state.message_scroll = hit.start_line;
        } else if hit.end_line >= self.state.message_scroll.saturating_add(viewport_rows) {
            self.state.message_scroll =
                hit.end_line.saturating_add(1).saturating_sub(viewport_rows);
        }
        self.clamp_message_scroll();
    }

    fn open_action_menu(&mut self) {
        if let Some(message_id) = self.state.selected_message_id.clone() {
            self.state.reaction_picker = None;
            self.state.action_menu = Some(ActionMenu {
                message_id,
                selected: 0,
            });
            self.state.status = "message actions opened".to_owned();
        } else {
            self.state.status = "select a message first".to_owned();
        }
    }

    fn start_reply(&mut self, message_id: MessageId) {
        let preview = self
            .message_by_id(&message_id)
            .map(reply_preview)
            .unwrap_or_else(|| format!("message {}", short_id(&message_id)));
        self.state.reply_to = Some(message_id);
        self.state.focus = FocusPane::Compose;
        self.state.status = format!("replying to {preview}");
    }

    fn open_thread(&mut self, message_id: MessageId) {
        let reply_count = self.thread_reply_count(&message_id);
        self.state.thread_root = Some(message_id);
        self.state.focus = FocusPane::Details;
        self.state.status = if reply_count == 0 {
            "thread opened; no replies yet".to_owned()
        } else if reply_count == 1 {
            "thread opened with 1 reply".to_owned()
        } else {
            format!("thread opened with {reply_count} replies")
        };
    }

    async fn apply_reaction(&mut self, message_id: MessageId, emoji: &str) -> Result<()> {
        let Some(chat) = self.state.selected_chat().cloned() else {
            self.state.status = "select a chat before reacting".to_owned();
            return Ok(());
        };
        let had_reaction = self.message_by_id(&message_id).is_some_and(|message| {
            message_reacted_by_sender(message, emoji, LOCAL_REACTION_SENDER)
        });

        if let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.id().as_ref() == chat.account.as_ref())
        {
            provider.react(&chat.id, &message_id, emoji).await?;
        }

        let Some(message) = self.message_by_id_mut(&message_id) else {
            self.state.status = "selected message was not found".to_owned();
            return Ok(());
        };
        if had_reaction {
            remove_reaction(message, emoji, &Arc::from(LOCAL_REACTION_SENDER));
        } else {
            add_reaction(message, emoji, Arc::from(LOCAL_REACTION_SENDER));
        }
        let updated = message.clone();
        self.store.upsert_message(&updated).await?;
        self.state.status = if had_reaction {
            format!("removed reaction {emoji}")
        } else {
            format!("reacted with {emoji}")
        };
        Ok(())
    }

    fn copy_message_text(&mut self, message_id: &MessageId) {
        let Some(message) = self.message_by_id(message_id) else {
            self.state.status = "selected message was not found".to_owned();
            return;
        };
        let text = content_copy_text(&message.content);
        if text.trim().is_empty() {
            self.state.status = "selected message has no copyable text".to_owned();
            return;
        }

        match Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text.clone())) {
            Ok(()) => self.state.status = "copied message text".to_owned(),
            Err(error) => {
                self.state.status = format!("clipboard unavailable; text: {text} ({error})");
            }
        }
    }

    fn open_message_image(&mut self, message_id: &MessageId) -> bool {
        let Some(message) = self.message_by_id(message_id) else {
            return false;
        };
        let Some((path, title, caption)) = message_image_preview(&message.content) else {
            return false;
        };

        self.state.image_viewer = Some(ImageViewer {
            path: path.clone(),
            title,
            caption,
        });
        self.state.status = format!("viewing image {}", path.display());
        true
    }

    fn set_account_status(
        &mut self,
        provider_id: &ProviderId,
        connection: AccountConnection,
        detail: Option<String>,
    ) {
        let status = self
            .state
            .account_statuses
            .entry(provider_id.clone())
            .or_insert_with(|| AccountStatus {
                display_name: provider_id.to_string(),
                connection: AccountConnection::Connecting,
                detail: None,
            });
        status.connection = connection;
        status.detail = detail;
    }

    fn maybe_show_notification(
        &mut self,
        message: &Message,
        selected_chat_id: Option<&chat_core::ChatId>,
        is_historical: bool,
    ) {
        if is_historical || message.is_from_me || selected_chat_id == Some(&message.chat_id) {
            return;
        }

        let Some(chat) = self
            .state
            .chats
            .iter()
            .find(|chat| chat.id == message.chat_id && chat.account == message.account)
            .or_else(|| {
                self.state
                    .chats
                    .iter()
                    .find(|chat| chat.id == message.chat_id)
            })
        else {
            return;
        };
        if chat.muted {
            return;
        }

        let notification = NotificationOverlay::new(chat, message);
        let chat_name = chat.name.to_string();
        self.state.status = format!("new message in {chat_name}");
        self.state.notification = Some(notification);
    }

    fn dismiss_notification(&mut self) {
        self.state.notification = None;
    }

    fn handle_tick(&mut self) {
        let expired = if let Some(notification) = &mut self.state.notification {
            notification.ticks_remaining = notification.ticks_remaining.saturating_sub(1);
            notification.ticks_remaining == 0
        } else {
            false
        };
        if expired {
            self.state.notification = None;
        }
    }

    fn open_account_switcher(&mut self) {
        let selected = self
            .account_options()
            .iter()
            .position(|option| option.provider_id == self.state.active_account)
            .unwrap_or_default();
        self.state.action_menu = None;
        self.state.reaction_picker = None;
        self.state.account_switcher = Some(AccountSwitcher { selected });
        self.state.status = "choose account".to_owned();
    }

    fn apply_account_switcher_selection(&mut self, selected: usize) -> bool {
        let Some(option) = self.account_options().get(selected).cloned() else {
            return false;
        };
        self.state.active_account = option.provider_id;
        self.state.account_switcher = None;
        let changed = self.apply_filter();
        self.state.selected_message_id = None;
        self.state.action_menu = None;
        self.state.reaction_picker = None;
        self.state.reply_to = None;
        self.state.thread_root = None;
        self.state.image_viewer = None;
        self.state.message_scroll = 0;
        self.state.status = format!("filtered to {}", option.label);
        changed
    }

    fn account_options(&self) -> Vec<AccountOption> {
        let mut account_ids = self
            .state
            .account_statuses
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for chat in &self.state.chats {
            if !account_ids.iter().any(|id| id == &chat.account) {
                account_ids.push(chat.account.clone());
            }
        }
        account_ids.sort_by(|left, right| {
            let left_name = self
                .state
                .account_statuses
                .get(left)
                .map(|status| status.display_name.as_str())
                .unwrap_or(left.as_ref());
            let right_name = self
                .state
                .account_statuses
                .get(right)
                .map(|status| status.display_name.as_str())
                .unwrap_or(right.as_ref());
            left_name.cmp(right_name).then_with(|| left.cmp(right))
        });

        let mut options = vec![AccountOption {
            provider_id: None,
            label: "All accounts".to_owned(),
            summary: self.state.account_status_summary(),
            chat_count: self.state.chats.len(),
        }];
        options.extend(account_ids.into_iter().map(|provider_id| {
            let status = self.state.account_statuses.get(&provider_id);
            let label = status
                .map(|status| status.display_name.clone())
                .unwrap_or_else(|| provider_id.to_string());
            let summary = status
                .map(AccountStatus::summary)
                .unwrap_or_else(|| "unknown".to_owned());
            let chat_count = self
                .state
                .chats
                .iter()
                .filter(|chat| chat.account == provider_id)
                .count();
            AccountOption {
                provider_id: Some(provider_id),
                label,
                summary,
                chat_count,
            }
        }));
        options
    }

    fn account_filter_label(&self) -> String {
        self.state
            .active_account
            .as_ref()
            .and_then(|provider_id| self.state.account_statuses.get(provider_id))
            .map(|status| status.display_name.clone())
            .unwrap_or_else(|| {
                self.state
                    .active_account
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "All accounts".to_owned())
            })
    }

    fn account_switcher_option_at(&self, column: u16, row: u16) -> Option<usize> {
        let options = self.account_options();
        let width = self.state.frame_area.width.saturating_sub(4).clamp(32, 64);
        let height = (options.len() as u16).saturating_add(4).clamp(6, 14);
        let modal = centered_fixed_rect(self.state.frame_area, width, height);
        if !rect_contains(modal, column, row) {
            return None;
        }
        let first_option_row = modal.y.saturating_add(3);
        let index = row.checked_sub(first_option_row)? as usize;
        (index < options.len()).then_some(index)
    }

    fn action_menu_rect(&self, area: Rect, message_id: &MessageId) -> Rect {
        self.anchored_message_popup_rect(area, message_id, 34, ActionMenuItem::ALL.len() as u16 + 4)
    }

    fn reaction_picker_rect(&self, area: Rect, message_id: &MessageId) -> Rect {
        let width = REACTION_OPTIONS
            .len()
            .saturating_mul(REACTION_OPTION_CELL_WIDTH as usize)
            .saturating_add(4) as u16;
        self.anchored_message_popup_rect(area, message_id, width, 5)
    }

    fn anchored_message_popup_rect(
        &self,
        area: Rect,
        message_id: &MessageId,
        width: u16,
        height: u16,
    ) -> Rect {
        let fallback = centered_fixed_rect(area, width, height);
        let width = width.max(1).min(area.width);
        let height = height.max(1).min(area.height);
        let content_area = inner_area(self.state.pane_areas.messages);
        let Some(hit) = self
            .state
            .message_hits
            .iter()
            .find(|hit| hit.message_id.as_ref() == message_id.as_ref())
        else {
            return fallback;
        };

        let viewport_start = self.state.message_scroll;
        let viewport_end = viewport_start.saturating_add(content_area.height as usize);
        if hit.end_line < viewport_start || hit.start_line >= viewport_end {
            return fallback;
        }

        let visible_start = hit.start_line.saturating_sub(viewport_start);
        let visible_end = hit.end_line.saturating_sub(viewport_start);
        let anchor_top = content_area.y.saturating_add(visible_start as u16);
        let anchor_bottom = content_area.y.saturating_add(visible_end as u16);
        let area_bottom = area.y.saturating_add(area.height);
        let below = anchor_bottom.saturating_add(1);
        let above = anchor_top.saturating_sub(height);
        let y = if below.saturating_add(height) <= area_bottom {
            below
        } else if anchor_top >= area.y.saturating_add(height) {
            above
        } else {
            fallback.y
        };

        let content_right = content_area.x.saturating_add(content_area.width);
        let preferred_x = if self
            .message_by_id(message_id)
            .is_some_and(|message| message.is_from_me)
        {
            content_right.saturating_sub(width.saturating_add(1))
        } else {
            content_area.x.saturating_add(1)
        };
        let max_x = area.x.saturating_add(area.width.saturating_sub(width));
        let x = preferred_x.clamp(area.x, max_x);

        Rect::new(x, y.min(area_bottom.saturating_sub(height)), width, height)
    }

    fn action_menu_item_at(&self, column: u16, row: u16, menu: &ActionMenu) -> Option<usize> {
        let modal = self.action_menu_rect(self.state.frame_area, &menu.message_id);
        if !rect_contains(modal, column, row) {
            return None;
        }
        let first_item_row = modal.y.saturating_add(2);
        let index = row.checked_sub(first_item_row)? as usize;
        (index < ActionMenuItem::ALL.len()).then_some(index)
    }

    fn reaction_picker_option_at(
        &self,
        column: u16,
        row: u16,
        picker: &ReactionPicker,
    ) -> Option<usize> {
        let modal = self.reaction_picker_rect(self.state.frame_area, &picker.message_id);
        if row != modal.y.saturating_add(2) || !rect_contains(modal, column, row) {
            return None;
        }
        let first_option_column = modal.x.saturating_add(2);
        let relative_column = column.checked_sub(first_option_column)?;
        let option = (relative_column / REACTION_OPTION_CELL_WIDTH) as usize;
        if relative_column % REACTION_OPTION_CELL_WIDTH >= REACTION_OPTION_CELL_WIDTH - 1 {
            return None;
        }
        (option < REACTION_OPTIONS.len()).then_some(option)
    }

    fn message_line_count(&self) -> usize {
        message_list::message_line_count(&self.state.messages, self.message_content_width())
    }

    fn message_content_width(&self) -> u16 {
        self.state
            .pane_areas
            .messages
            .width
            .saturating_sub(2)
            .max(1)
    }

    fn max_message_scroll(&self) -> usize {
        let viewport_rows = inner_area(self.state.pane_areas.messages).height as usize;
        let total_lines = self.message_line_count();
        bounded_message_scroll(total_lines, viewport_rows)
    }

    fn apply_filter(&mut self) -> bool {
        let previous_selected_chat = self
            .state
            .selected_chat()
            .map(|chat| (chat.id.clone(), chat.account.clone()));
        self.state.visible_chat_indices =
            chat_list::filter_chat_indices(&self.state.chats, &self.state.filter)
                .into_iter()
                .filter(|index| {
                    self.state.active_account.as_ref().is_none_or(|active| {
                        self.state
                            .chats
                            .get(*index)
                            .is_some_and(|chat| chat.account == *active)
                    })
                })
                .collect();

        if !self
            .state
            .visible_chat_indices
            .contains(&self.state.selected_chat)
        {
            self.state.selected_chat = self
                .state
                .visible_chat_indices
                .first()
                .copied()
                .unwrap_or_default();
        }

        let selected_chat = self
            .state
            .selected_chat()
            .map(|chat| (chat.id.clone(), chat.account.clone()));
        previous_selected_chat != selected_chat
    }

    fn clamp_message_scroll(&mut self) {
        self.state.message_scroll = self.state.message_scroll.min(self.max_message_scroll());
    }

    fn filter_status(&self) -> String {
        if self.state.filter.is_empty() {
            "filter cleared".to_owned()
        } else {
            format!(
                "filter {}: {} chats",
                self.state.filter,
                self.state.visible_chat_indices.len()
            )
        }
    }

    async fn reload_chats(&mut self) -> Result<()> {
        let selected_chat = self
            .state
            .selected_chat()
            .map(|chat| (chat.id.clone(), chat.account.clone()));
        self.state.chats = self.store.get_all_chats().await?;

        if let Some((selected_chat_id, selected_account)) = selected_chat
            && let Some(index) =
                self.state.chats.iter().position(|chat| {
                    chat.id == selected_chat_id && chat.account == selected_account
                })
        {
            self.state.selected_chat = index;
        }

        if self.state.selected_chat >= self.state.chats.len() {
            self.state.selected_chat = self.state.chats.len().saturating_sub(1);
        }
        self.apply_filter();
        Ok(())
    }

    async fn reload_selected_messages(&mut self) -> Result<()> {
        if let Some(chat) = self.state.selected_chat() {
            self.state.messages = self
                .store
                .get_messages(&chat.id, None, HISTORY_LIMIT)
                .await?;
        } else {
            self.state.messages.clear();
        }
        self.clamp_message_scroll();
        Ok(())
    }
}

async fn run_app_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut needs_draw = true;

    while !app.state.should_quit() {
        if needs_draw {
            terminal.draw(|frame| app.draw(frame))?;
            needs_draw = false;
        }

        if app.drain_provider_events().await? {
            needs_draw = true;
            continue;
        }

        if event::poll(IDLE_POLL_TIMEOUT)? {
            match event::read()? {
                CrosstermEvent::Key(key) => app.handle_event(AppEvent::Key(key)).await?,
                CrosstermEvent::Mouse(mouse) => app.handle_event(AppEvent::Mouse(mouse)).await?,
                CrosstermEvent::Resize(width, height) => {
                    app.handle_event(AppEvent::Resize(width, height)).await?;
                }
                _ => {}
            }
            needs_draw = true;
        } else {
            let notification_visible = app.state.notification_visible();
            app.handle_event(AppEvent::Tick).await?;
            if notification_visible != app.state.notification_visible() {
                needs_draw = true;
            }
        }
    }

    terminal.draw(|frame| app.draw(frame))?;
    Ok(())
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
        )
    );
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn new_compose_textarea() -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_placeholder_text("Type a message...");
    textarea.set_cursor_line_style(Style::default());
    textarea
}

fn bounded_message_scroll(total_lines: usize, viewport_rows: usize) -> usize {
    let viewport_rows = viewport_rows.max(1);
    if total_lines <= viewport_rows {
        return 0;
    }

    let keep_visible_rows = viewport_rows.min(total_lines).max(1);
    total_lines.saturating_sub(keep_visible_rows)
}

fn textarea_byte_cursor(textarea: &TextArea<'_>) -> usize {
    let (row, column) = textarea.cursor();
    let lines = textarea.lines();
    let mut cursor = lines
        .iter()
        .take(row)
        .map(|line| line.len() + 1)
        .sum::<usize>();

    if let Some(line) = lines.get(row) {
        cursor += line
            .char_indices()
            .nth(column)
            .map(|(index, _)| index)
            .unwrap_or(line.len());
    }

    cursor
}

fn textarea_input(key: TextAreaKey, modifiers: KeyModifiers) -> TextAreaInput {
    TextAreaInput {
        key,
        ctrl: modifiers.contains(KeyModifiers::CONTROL),
        alt: modifiers.contains(KeyModifiers::ALT),
        shift: modifiers.contains(KeyModifiers::SHIFT),
    }
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn clicked_column_in_hit(content_area: Rect, column: u16, start_col: u16, end_col: u16) -> bool {
    let clicked_col = column.saturating_sub(content_area.x);
    clicked_col >= start_col && clicked_col < end_col
}

fn inner_area(area: Rect) -> Rect {
    if area.width < 2 || area.height < 2 {
        return Rect::new(area.x, area.y, 0, 0);
    }

    Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2)
}

fn centered_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let width = area
        .width
        .saturating_mul(width_percent)
        .saturating_div(100)
        .max(1)
        .min(area.width);
    let height = area
        .height
        .saturating_mul(height_percent)
        .saturating_div(100)
        .max(1)
        .min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn centered_fixed_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.max(1).min(area.width);
    let height = height.max(1).min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn is_ctrl_char(key: KeyEvent, expected: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(value) if value.eq_ignore_ascii_case(&expected))
}

fn short_id(id: &str) -> String {
    id.rsplit(':')
        .next()
        .unwrap_or(id)
        .chars()
        .take(10)
        .collect()
}

fn account_status_summary(statuses: &HashMap<ProviderId, AccountStatus>) -> String {
    if statuses.is_empty() {
        return "none".to_owned();
    }

    let mut statuses = statuses.iter().collect::<Vec<_>>();
    statuses.sort_by(|(left_id, left), (right_id, right)| {
        left.display_name
            .cmp(&right.display_name)
            .then_with(|| left_id.cmp(right_id))
    });
    statuses
        .into_iter()
        .map(|(_, status)| status.summary())
        .collect::<Vec<_>>()
        .join(" · ")
}

fn auth_challenge_label(challenge: &AuthChallenge) -> &'static str {
    match challenge {
        AuthChallenge::QrCode(_) => "scan QR code",
        AuthChallenge::PairingCode(_) => "enter pairing code",
        AuthChallenge::OAuthUrl(_) => "open OAuth URL",
        AuthChallenge::Waiting => "waiting",
    }
}

fn reply_preview(message: &Message) -> String {
    let text = content_copy_text(&message.content);
    let preview = if text.trim().is_empty() {
        "attachment".to_owned()
    } else {
        truncate_chars(text.trim(), 42)
    };
    format!("{}: {preview}", message.sender.display_name)
}

fn notification_preview(message: &Message) -> String {
    let text = content_copy_text(&message.content);
    let preview = if text.trim().is_empty() {
        "attachment".to_owned()
    } else {
        text.trim().to_owned()
    };
    truncate_chars(&preview, 80)
}

fn thread_message_lines(message: &Message, theme: Theme) -> Vec<Line<'static>> {
    let sender_style = message_list::sender_style(theme, message.is_from_me);
    let mut lines = vec![Line::from(vec![
        Span::styled(message.sender.display_name.to_string(), sender_style),
        Span::styled(
            format!(" · {}", message.timestamp.format("%H:%M")),
            theme.muted(),
        ),
    ])];
    for line in content_copy_text(&message.content).lines() {
        let text = if line.trim().is_empty() {
            "attachment"
        } else {
            line
        };
        lines.push(Line::from(Span::raw(format!("  {text}"))));
    }
    if !message.reactions.is_empty() {
        let reactions = message
            .reactions
            .iter()
            .map(|reaction| format!("{} {}", reaction.emoji, reaction.senders.len()))
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(Line::from(Span::styled(
            format!("  {reactions}"),
            theme.muted(),
        )));
    }
    lines.push(Line::from(""));
    lines
}

fn content_copy_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.to_string(),
        Content::Image(media)
        | Content::Video(media)
        | Content::Audio(media)
        | Content::File(media)
        | Content::Sticker(media) => media
            .caption
            .as_deref()
            .map(str::to_owned)
            .unwrap_or_else(|| media.file_name.to_string()),
        Content::LinkPreview(link) => {
            let title = link.title.as_deref().unwrap_or("Link");
            let description = link
                .description
                .as_deref()
                .map(|description| format!(" — {description}"))
                .unwrap_or_default();
            format!("{title}: {}{description}", link.url)
        }
        Content::Deleted => String::new(),
        Content::Unsupported(kind) => format!("Unsupported message: {kind}"),
    }
}

fn message_image_preview(content: &Content) -> Option<(PathBuf, String, Option<String>)> {
    match content {
        Content::Image(media) | Content::Sticker(media) => media_image_preview(media),
        Content::LinkPreview(link) => link.image.as_ref().and_then(media_image_preview),
        Content::Video(_)
        | Content::Audio(_)
        | Content::File(_)
        | Content::Text(_)
        | Content::Deleted
        | Content::Unsupported(_) => None,
    }
}

fn media_image_preview(media: &Media) -> Option<(PathBuf, String, Option<String>)> {
    let path = media
        .local_path
        .as_ref()
        .filter(|path| path.exists())
        .or_else(|| media.thumbnail.as_ref().filter(|path| path.exists()))?
        .clone();
    Some((
        path,
        media.file_name.to_string(),
        media.caption.as_deref().map(str::to_owned),
    ))
}

fn message_reacted_by_sender(message: &Message, emoji: &str, sender: &str) -> bool {
    message.reactions.iter().any(|reaction| {
        reaction.emoji.as_ref() == emoji
            && reaction
                .senders
                .iter()
                .any(|candidate| candidate.as_ref() == sender)
    })
}

fn local_reaction_option(message: &Message) -> Option<usize> {
    REACTION_OPTIONS
        .iter()
        .position(|emoji| message_reacted_by_sender(message, emoji, LOCAL_REACTION_SENDER))
}

fn add_reaction(message: &mut Message, emoji: &str, sender: Arc<str>) {
    if let Some(reaction) = message
        .reactions
        .iter_mut()
        .find(|reaction| reaction.emoji.as_ref() == emoji)
    {
        if !reaction
            .senders
            .iter()
            .any(|candidate| candidate == &sender)
        {
            reaction.senders.push(sender);
        }
    } else {
        message.reactions.push(Reaction {
            emoji: Arc::from(emoji),
            senders: vec![sender],
        });
    }
}

fn remove_reaction(message: &mut Message, emoji: &str, sender: &Arc<str>) {
    if let Some(reaction) = message
        .reactions
        .iter_mut()
        .find(|reaction| reaction.emoji.as_ref() == emoji)
    {
        reaction.senders.retain(|candidate| candidate != sender);
    }
    message
        .reactions
        .retain(|reaction| !reaction.senders.is_empty());
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::{EventBus, MockProvider, Platform};
    use crossterm::event::{KeyEventKind, KeyEventState, MouseEventKind};
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    #[tokio::test]
    async fn app_bootstraps_mock_provider_into_chat_list() -> Result<()> {
        let app = test_app().await?;

        assert_eq!(app.state().chats().len(), 10);
        assert_eq!(
            app.state().selected_chat().map(|chat| chat.name.as_ref()),
            Some("Family Weekend 🏡")
        );
        assert_eq!(app.state().messages().len(), 2);
        assert_eq!(
            app.state().visible_chat_indices(),
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
        );
        assert_eq!(app.state().filter(), "");
        assert_eq!(app.state().focus(), FocusPane::ChatList);

        Ok(())
    }

    #[tokio::test]
    async fn app_handles_navigation_and_quit_keys() -> Result<()> {
        let mut app = test_app().await?;

        app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().selected_chat_index(), 1);
        assert_eq!(app.state().messages().len(), 3);

        app.handle_event(AppEvent::Key(key(KeyCode::Up, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().selected_chat_index(), 0);
        assert_eq!(app.state().messages().len(), 2);

        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        assert!(app.state().should_quit());

        Ok(())
    }

    #[tokio::test]
    async fn app_marks_unread_chat_read_when_opened() -> Result<()> {
        let mut app = test_app().await?;
        assert_eq!(app.state().selected_chat().unwrap().unread_count, 4);

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        let chat = app.state().selected_chat().unwrap();
        assert_eq!(chat.unread_count, 0);
        let stored = app.store.get_all_chats().await?;
        let stored_chat = stored
            .iter()
            .find(|candidate| candidate.id == chat.id && candidate.account == chat.account)
            .unwrap();
        assert_eq!(stored_chat.unread_count, 0);

        Ok(())
    }

    #[tokio::test]
    async fn app_account_switcher_filters_chats_by_account_and_returns_to_all() -> Result<()> {
        let mock = MockProvider::new();
        let second = StaticTestProvider::from_mock(
            "mock:secondary",
            "Secondary Account",
            &mock,
            "mock:secondary:",
        )?;
        let mut app = test_app_with_providers(vec![Box::new(mock), Box::new(second)]).await?;
        let mut terminal = Terminal::new(TestBackend::new(120, 32))?;
        terminal.draw(|frame| app.draw(frame))?;

        assert_eq!(app.state().chats().len(), 20);
        assert_eq!(app.state().visible_chat_indices().len(), 20);
        assert_eq!(app.state().active_account(), None);

        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        assert!(app.state().account_switcher_open());
        assert_eq!(app.state().account_switcher_selected(), Some(0));

        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Filter by account"));
        assert!(content.contains("All accounts"));
        assert!(content.contains("Mock Account"));
        assert!(content.contains("Secondary Account"));

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            30,
            17,
        )))
        .await?;

        assert!(!app.state().account_switcher_open());
        assert_eq!(
            app.state().active_account().map(|id| id.as_ref()),
            Some("mock:secondary")
        );
        assert_eq!(app.state().visible_chat_indices().len(), 10);
        assert!(
            app.state()
                .selected_chat()
                .is_some_and(|chat| chat.account.as_ref() == "mock:secondary")
        );
        assert!(app.state().status().contains("Secondary Account"));

        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Account: Secondary Account"));

        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('f'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        for value in "zzzz-no-match".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        assert!(app.state().visible_chat_indices().is_empty());
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("No chats match 'zzzz-no-match'"));
        assert!(content.contains("Account: Secondary Account"));

        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        assert!(!app.state().filter_mode());
        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().visible_chat_indices().len(), 10);

        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        assert_eq!(app.state().account_switcher_selected(), Some(2));
        app.handle_event(AppEvent::Key(key(KeyCode::Up, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().account_switcher_selected(), Some(1));
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        assert!(!app.state().account_switcher_open());
        assert_eq!(
            app.state().active_account().map(|id| id.as_ref()),
            Some("mock:local")
        );
        assert_eq!(app.state().visible_chat_indices().len(), 10);
        assert!(
            app.state()
                .selected_chat()
                .is_some_and(|chat| chat.account.as_ref() == "mock:local")
        );
        assert!(app.state().status().contains("Mock Account"));

        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        assert_eq!(app.state().account_switcher_selected(), Some(1));
        app.handle_event(AppEvent::Key(key(KeyCode::Home, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        assert_eq!(app.state().active_account(), None);
        assert_eq!(app.state().visible_chat_indices().len(), 20);
        assert!(app.state().status().contains("All accounts"));

        Ok(())
    }

    #[tokio::test]
    async fn app_handles_filtering_and_filter_clear() -> Result<()> {
        let mut app = test_app().await?;

        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('f'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        assert!(app.state().filter_mode());

        for value in "media".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }

        assert_eq!(app.state().filter(), "media");
        assert_eq!(app.state().visible_chat_indices(), &[4]);
        assert_eq!(
            app.state().selected_chat().map(|chat| chat.name.as_ref()),
            Some("Media Samples")
        );
        assert_eq!(app.state().messages().len(), 3);

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert!(!app.state().filter_mode());

        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().filter(), "");
        assert_eq!(
            app.state().visible_chat_indices(),
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_handles_intuitive_pane_focus_and_standard_navigation() -> Result<()> {
        let mut app = test_app().await?;

        app.handle_event(AppEvent::Key(key(KeyCode::Right, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);

        app.handle_event(AppEvent::Key(key(KeyCode::Right, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Compose);

        app.handle_event(AppEvent::Key(key(KeyCode::Right, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Details);

        app.handle_event(AppEvent::Key(key(KeyCode::Left, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Compose);

        app.handle_event(AppEvent::Key(key(KeyCode::Left, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);

        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::ChatList);

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);
        assert!(app.state().status().contains("opened Family Weekend"));

        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::End, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().selected_chat_index(), 9);
        assert_eq!(app.state().messages().len(), 1);

        app.handle_event(AppEvent::Key(key(KeyCode::Home, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().selected_chat_index(), 0);
        assert_eq!(app.state().messages().len(), 2);

        app.handle_event(AppEvent::Key(key(KeyCode::PageDown, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().selected_chat_index(), 5);

        app.handle_event(AppEvent::Key(key(KeyCode::PageUp, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().selected_chat_index(), 0);

        Ok(())
    }

    #[tokio::test]
    async fn app_handles_compose_input_editing_and_send_flow() -> Result<()> {
        let mut app = test_app().await?;
        let chat_id = app.state().selected_chat().unwrap().id.clone();
        let initial_count = app.state().messages().len();

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);

        for value in "Hello compose".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        assert_eq!(app.state().focus(), FocusPane::Compose);
        assert_eq!(app.state().compose_text(), "Hello compose");
        assert_eq!(app.state().compose_cursor(), "Hello compose".len());

        app.handle_event(AppEvent::Key(key(KeyCode::Left, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Left, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Char('!'), KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().compose_text(), "Hello compo!se");

        app.handle_event(AppEvent::Key(key(KeyCode::End, KeyModifiers::NONE)))
            .await?;
        for value in " ✨".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(KeyCode::Backspace, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Char('✅'), KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().compose_text(), "Hello compo!se ✅");

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().compose_text(), "");
        assert_eq!(app.state().compose_cursor(), 0);
        assert_eq!(app.state().messages().len(), initial_count + 1);
        assert!(
            app.state()
                .status()
                .contains("sent message to Family Weekend")
        );

        let persisted = app
            .store
            .get_messages(&chat_id, None, HISTORY_LIMIT)
            .await?;
        assert!(persisted.iter().any(|message| {
            message.is_from_me
                && matches!(&message.content, Content::Text(text) if text.as_ref() == "Hello compo!se ✅")
        }));
        let provider_history = app.providers[0]
            .history(&chat_id, None, HISTORY_LIMIT)
            .await?;
        assert!(provider_history.iter().any(|message| {
            message.is_from_me
                && matches!(&message.content, Content::Text(text) if text.as_ref() == "Hello compo!se ✅")
        }));

        Ok(())
    }

    #[tokio::test]
    async fn app_handles_mouse_focused_compose_send_flow() -> Result<()> {
        let mut app = test_app().await?;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| app.draw(frame))?;

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            40,
            27,
        )))
        .await?;
        assert_eq!(app.state().focus(), FocusPane::Compose);

        for value in "Clicked compose".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        assert_eq!(app.state().compose_text(), "");
        assert!(app.state().messages().iter().any(|message| {
            message.is_from_me
                && matches!(&message.content, Content::Text(text) if text.as_ref() == "Clicked compose")
        }));
        assert!(app.state().status().contains("sent message"));

        Ok(())
    }

    #[tokio::test]
    async fn app_ignores_removed_vim_style_shortcuts() -> Result<()> {
        let mut app = test_app().await?;

        for code in [
            KeyCode::Char('/'),
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('g'),
            KeyCode::Char('G'),
            KeyCode::Char('n'),
            KeyCode::Char('N'),
        ] {
            app.handle_event(AppEvent::Key(key(code, KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )))
        .await?;

        assert_eq!(app.state().selected_chat_index(), 0);
        assert!(!app.state().filter_mode());
        assert_eq!(app.state().focus(), FocusPane::ChatList);

        Ok(())
    }

    #[tokio::test]
    async fn app_handles_mouse_clicks_and_touchpad_scrolling() -> Result<()> {
        let mut app = test_app().await?;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| app.draw(frame))?;

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            2,
            3,
        )))
        .await?;
        assert_eq!(app.state().selected_chat_index(), 1);
        assert_eq!(
            app.state().selected_chat().map(|chat| chat.name.as_ref()),
            Some("Alice Chen")
        );
        assert_eq!(app.state().focus(), FocusPane::Messages);
        assert_eq!(app.state().messages().len(), 3);

        app.handle_event(AppEvent::Mouse(mouse(MouseEventKind::ScrollDown, 2, 1)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::ChatList);
        assert_eq!(app.state().selected_chat_index(), 4);

        app.handle_event(AppEvent::Mouse(mouse(MouseEventKind::ScrollDown, 50, 2)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);
        assert_eq!(app.state().message_scroll(), app.max_message_scroll());

        app.handle_event(AppEvent::Mouse(mouse(MouseEventKind::ScrollUp, 2, 1)))
            .await?;
        assert_eq!(app.state().selected_chat_index(), 1);

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            100,
            2,
        )))
        .await?;
        assert_eq!(app.state().focus(), FocusPane::Details);

        Ok(())
    }

    #[tokio::test]
    async fn app_draws_visible_image_media_card() -> Result<()> {
        let mut app = test_app().await?;

        app.handle_event(AppEvent::Key(key(KeyCode::End, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Home, KeyModifiers::NONE)))
            .await?;
        for _ in 0..4 {
            app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
                .await?;
        }

        assert_eq!(
            app.state().selected_chat().map(|chat| chat.name.as_ref()),
            Some("Media Samples")
        );
        app.state.message_scroll = 0;

        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(content.contains("Photo"));
        assert!(!content.contains("actual image:"));
        assert!(content.contains("mock-screenshot.png"));
        assert!(content.contains("caption: Shared an image attachment"));
        assert!(!content.contains("Click to preview"));
        assert!(content.contains("▀"));
        assert!(content.contains("╭"));
        assert!(content.contains("╰"));
        assert!(terminal.backend().buffer().content().iter().any(|cell| {
            matches!(cell.fg, Color::Rgb(_, _, _)) || matches!(cell.bg, Color::Rgb(_, _, _))
        }));

        Ok(())
    }

    #[tokio::test]
    async fn app_handles_message_selection_actions_reply_and_reaction_clicks() -> Result<()> {
        let mut app = test_app().await?;
        let mut terminal = Terminal::new(TestBackend::new(140, 40))?;
        terminal.draw(|frame| app.draw(frame))?;

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .await?;
        let selected_message_id = app.state().selected_message_id().cloned().unwrap();
        assert_eq!(app.state().focus(), FocusPane::Messages);
        assert!(app.state().selected_message_id().is_some());

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert!(app.state().action_menu_open());

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().reply_to(), Some(&selected_message_id));
        assert_eq!(app.state().focus(), FocusPane::Compose);

        for value in "Reply from test".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().reply_to(), None);
        assert!(app.state().messages().iter().any(|message| {
            message.reply_to.as_ref() == Some(&selected_message_id)
                && matches!(&message.content, Content::Text(text) if text.as_ref() == "Reply from test")
        }));

        terminal.draw(|frame| app.draw(frame))?;
        app.handle_event(AppEvent::Key(key(KeyCode::Up, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert!(app.state().action_menu_open());

        let menu = app.state.action_menu.clone().unwrap();
        let menu_rect = app.action_menu_rect(app.state.frame_area, &menu.message_id);
        let react_row = menu_rect.y
            + 2
            + ActionMenuItem::ALL
                .iter()
                .position(|item| *item == ActionMenuItem::React)
                .unwrap() as u16;
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            menu_rect.x + 3,
            react_row,
        )))
        .await?;
        assert!(app.state().reaction_picker_open());

        terminal.draw(|frame| app.draw(frame))?;
        let picker = app.state.reaction_picker.clone().unwrap();
        let picker_rect = app.reaction_picker_rect(app.state.frame_area, &picker.message_id);
        let emoji_index = 3;
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            picker_rect.x + 2 + emoji_index as u16 * REACTION_OPTION_CELL_WIDTH,
            picker_rect.y + 2,
        )))
        .await?;
        assert!(!app.state().reaction_picker_open());
        let reacted = app.message_by_id(&picker.message_id).unwrap();
        assert!(
            reacted
                .reactions
                .iter()
                .any(|reaction| reaction.emoji.as_ref() == REACTION_OPTIONS[emoji_index])
        );

        let selected_message_id = app.state().selected_message_id().cloned().unwrap();
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        let mut menu = app.state.action_menu.clone().unwrap();
        menu.selected = ActionMenuItem::ALL
            .iter()
            .position(|item| *item == ActionMenuItem::React)
            .unwrap();
        app.state.action_menu = Some(menu);
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert!(app.state().reaction_picker_open());
        terminal.draw(|frame| app.draw(frame))?;
        let picker = app.state.reaction_picker.clone().unwrap();
        let picker_rect = app.reaction_picker_rect(app.state.frame_area, &picker.message_id);
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            picker_rect.x + 2 + emoji_index as u16 * REACTION_OPTION_CELL_WIDTH,
            picker_rect.y + 2,
        )))
        .await?;
        let reacted = app.message_by_id(&selected_message_id).unwrap();
        assert!(
            reacted
                .reactions
                .iter()
                .all(|reaction| reaction.emoji.as_ref() != REACTION_OPTIONS[emoji_index])
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_opens_thread_pane_and_shows_replies() -> Result<()> {
        let mut app = test_app().await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .await?;
        assert_eq!(
            app.state().selected_chat().map(|chat| chat.name.as_ref()),
            Some("#project-chat-cli")
        );
        let root_message_id = app
            .state()
            .messages()
            .iter()
            .find(|message| message.id.as_ref() == "mock:msg:team:2")
            .unwrap()
            .id
            .clone();
        let mut terminal = Terminal::new(TestBackend::new(140, 40))?;

        app.perform_action_menu_item(root_message_id.clone(), ActionMenuItem::ViewThread)
            .await?;
        assert_eq!(app.state().thread_root(), Some(&root_message_id));
        assert_eq!(app.state().focus(), FocusPane::Details);
        assert!(app.state().status().contains("2 replies"));

        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Thread"));
        assert!(content.contains("Original"));
        assert!(content.contains("2 replies"));
        assert!(content.contains("Threaded reply"));

        app.perform_action_menu_item(root_message_id.clone(), ActionMenuItem::Reply)
            .await?;
        for value in "Thread reply".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        app.perform_action_menu_item(root_message_id.clone(), ActionMenuItem::ViewThread)
            .await?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("3 replies"));
        assert!(content.contains("Thread reply"));

        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().thread_root(), None);
        assert_eq!(app.state().status(), "thread closed");

        Ok(())
    }

    #[tokio::test]
    async fn app_tracks_provider_connection_status_in_status_bar_and_details() -> Result<()> {
        let mut app = test_app().await?;
        let provider_id = app.state().provider_for_selected_chat().unwrap().clone();

        assert_eq!(app.state().account_status_summary(), "Mock Account online");

        app.handle_event(AppEvent::Provider(
            provider_id.clone(),
            Box::new(ProviderEvent::SyncProgress(42)),
        ))
        .await?;
        assert_eq!(
            app.state().account_status_summary(),
            "Mock Account syncing 42%"
        );

        app.handle_event(AppEvent::Provider(
            provider_id.clone(),
            Box::new(ProviderEvent::AuthRequired(AuthChallenge::QrCode(
                Arc::from("mock-qr"),
            ))),
        ))
        .await?;
        assert_eq!(
            app.state().account_status_summary(),
            "Mock Account auth needed: scan QR code"
        );

        app.handle_event(AppEvent::Provider(
            provider_id.clone(),
            Box::new(ProviderEvent::Disconnected(Some(Arc::from("network down")))),
        ))
        .await?;
        assert_eq!(
            app.state().account_status_summary(),
            "Mock Account offline: network down"
        );

        app.handle_event(AppEvent::Provider(
            provider_id.clone(),
            Box::new(ProviderEvent::Reconnecting),
        ))
        .await?;
        assert_eq!(
            app.state().account_status_summary(),
            "Mock Account reconnecting"
        );

        app.handle_event(AppEvent::Provider(
            provider_id,
            Box::new(ProviderEvent::SyncComplete),
        ))
        .await?;
        assert_eq!(app.state().account_status_summary(), "Mock Account online");

        let mut terminal = Terminal::new(TestBackend::new(120, 30))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(!content.contains("Accounts: Mock Account online"));
        assert!(!content.contains("click/Ctrl+A"));

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            4,
            29,
        )))
        .await?;
        assert!(app.state().account_switcher_open());
        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        assert!(!app.state().account_switcher_open());

        app.handle_event(AppEvent::Key(key(KeyCode::Right, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            4,
            29,
        )))
        .await?;
        assert!(!app.state().account_switcher_open());

        Ok(())
    }

    #[tokio::test]
    async fn app_shows_and_expires_notification_overlay_for_background_messages() -> Result<()> {
        let mut app = test_app().await?;
        let background_chat = app
            .state()
            .chats()
            .iter()
            .find(|chat| chat.name.as_ref() == "Alice Chen")
            .unwrap()
            .clone();
        let message = test_incoming_message(
            &background_chat,
            "mock:msg:notify:alice",
            "Alice",
            "Background notification preview from Alice",
        );

        app.handle_event(AppEvent::Provider(
            background_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: message.clone(),
                is_historical: false,
            }),
        ))
        .await?;

        assert!(app.state().notification_visible());
        assert_eq!(
            app.state().notification_preview(),
            Some("Background notification preview from Alice")
        );
        assert!(app.state().status().contains("new message in Alice Chen"));

        let mut terminal = Terminal::new(TestBackend::new(120, 30))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Notification"));
        assert!(content.contains("New message"));
        assert!(content.contains("Alice Chen"));
        assert!(content.contains("Background notification preview from Alice"));

        for _ in 0..NOTIFICATION_TICKS {
            app.handle_event(AppEvent::Tick).await?;
        }
        assert!(!app.state().notification_visible());

        Ok(())
    }

    #[tokio::test]
    async fn app_suppresses_and_dismisses_notification_overlay() -> Result<()> {
        let mut app = test_app().await?;
        let active_chat = app.state().selected_chat().unwrap().clone();
        let muted_chat = app
            .state()
            .chats()
            .iter()
            .find(|chat| chat.muted)
            .unwrap()
            .clone();
        let background_chat = app
            .state()
            .chats()
            .iter()
            .find(|chat| chat.name.as_ref() == "Alice Chen")
            .unwrap()
            .clone();

        app.handle_event(AppEvent::Provider(
            active_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: test_incoming_message(
                    &active_chat,
                    "mock:msg:notify:active",
                    "Maya",
                    "Active chat should stay quiet",
                ),
                is_historical: false,
            }),
        ))
        .await?;
        assert!(!app.state().notification_visible());

        app.handle_event(AppEvent::Provider(
            muted_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: test_incoming_message(
                    &muted_chat,
                    "mock:msg:notify:muted",
                    "Ops",
                    "Muted chat should stay quiet",
                ),
                is_historical: false,
            }),
        ))
        .await?;
        assert!(!app.state().notification_visible());

        app.handle_event(AppEvent::Provider(
            background_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: test_incoming_message(
                    &background_chat,
                    "mock:msg:notify:historical",
                    "Alice",
                    "Historical sync should stay quiet",
                ),
                is_historical: true,
            }),
        ))
        .await?;
        assert!(!app.state().notification_visible());

        app.handle_event(AppEvent::Provider(
            background_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: test_incoming_message(
                    &background_chat,
                    "mock:msg:notify:dismiss",
                    "Alice",
                    "Dismiss me with input",
                ),
                is_historical: false,
            }),
        ))
        .await?;
        assert!(app.state().notification_visible());

        app.handle_event(AppEvent::Key(key(KeyCode::Right, KeyModifiers::NONE)))
            .await?;
        assert!(!app.state().notification_visible());

        Ok(())
    }

    #[tokio::test]
    async fn app_mouse_click_opens_message_actions_without_opening_media() -> Result<()> {
        let mut app = test_app().await?;
        let mut terminal = Terminal::new(TestBackend::new(140, 40))?;
        terminal.draw(|frame| app.draw(frame))?;

        let hit = app.state.message_hits.first().cloned().unwrap();
        let line_hit = hit.line_hits.first().cloned().unwrap();
        let content_area = inner_area(app.state.pane_areas.messages);

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            content_area.x + content_area.width.saturating_sub(1),
            content_area.y + line_hit.line as u16,
        )))
        .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);
        assert_eq!(app.state().selected_message_id(), None);
        assert!(!app.state().action_menu_open());

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            content_area.x + line_hit.start_col + 1,
            content_area.y + line_hit.line as u16,
        )))
        .await?;

        assert_eq!(app.state().focus(), FocusPane::Messages);
        assert_eq!(app.state().selected_message_id(), Some(&hit.message_id));
        assert!(app.state().action_menu_open());
        assert_eq!(app.state().status(), "message actions opened");

        Ok(())
    }

    #[tokio::test]
    async fn app_copy_action_reports_clipboard_success_or_friendly_fallback() -> Result<()> {
        let mut app = test_app().await?;
        let message_id = app.state().messages().first().unwrap().id.clone();

        app.copy_message_text(&message_id);

        assert!(
            app.state().status() == "copied message text"
                || app
                    .state()
                    .status()
                    .starts_with("clipboard unavailable; text:")
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_clicks_image_thumbnail_to_open_and_close_viewer() -> Result<()> {
        let mut app = test_app().await?;
        for _ in 0..4 {
            app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
                .await?;
        }
        app.state.message_scroll = 0;

        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| app.draw(frame))?;

        assert!(app.state().media_hit_count() > 0);
        assert!(!app.state().image_viewer_open());
        let hit = app.state.media_hits.first().cloned().unwrap();
        let expected_title = format!("Image Preview - {}", hit.title);
        let content_area = inner_area(app.state.pane_areas.messages);
        let click_row = content_area
            .y
            .saturating_add(hit.start_line.saturating_sub(app.state.message_scroll) as u16);
        let click_column = content_area.x.saturating_add(4);

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            click_column,
            click_row,
        )))
        .await?;
        assert!(app.state().image_viewer_open());
        assert!(!app.state().action_menu_open());
        assert!(app.state().status().contains("viewing image"));

        terminal.draw(|frame| app.draw(frame))?;
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains(&expected_title));
        assert!(content.contains("Click anywhere or press Esc to close"));
        assert!(terminal.backend().buffer().content().iter().any(|cell| {
            matches!(cell.fg, Color::Rgb(_, _, _)) || matches!(cell.bg, Color::Rgb(_, _, _))
        }));

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            click_column,
            click_row,
        )))
        .await?;
        assert!(!app.state().image_viewer_open());
        assert_eq!(app.state().status(), "image preview closed");

        Ok(())
    }

    #[tokio::test]
    async fn app_draws_actual_avatar_images() -> Result<()> {
        let mut app = test_app().await?;
        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend)?;

        terminal.draw(|frame| app.draw(frame))?;
        let buffer = terminal.backend().buffer();

        assert!(
            (5..9).any(|x| rgb_cell_at(buffer, x, 1)) && (5..9).any(|x| rgb_cell_at(buffer, x, 2)),
            "chat list should render a square-ish real PNG avatar in the first visible chat row"
        );
        assert!(
            (43..45).any(|x| rgb_cell_at(buffer, x, 1)),
            "message header should render a compact square real PNG sender avatar"
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_draws_three_pane_shell() -> Result<()> {
        let mut app = test_app().await?;
        let backend = TestBackend::new(110, 24);
        let mut terminal = Terminal::new(backend)?;

        terminal.draw(|frame| app.draw(frame))?;
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(content.contains("Messages - Family Weekend"));
        assert!(content.contains("Details"));
        assert!(content.contains("Compose - Family Weekend"));
        assert!(content.contains("Arrows"));
        assert!(content.contains("Ctrl+F"));
        assert!(content.contains("Click/tap"));
        assert!(content.contains("Click compose"));
        assert!(!content.contains("Click image"));
        assert!(content.contains("Ctrl+J"));
        assert!(content.contains("Ctrl+A"));
        assert!(content.contains("Scroll/trackpad"));
        assert!(!content.contains("INSERT MODE"));
        assert!(!content.contains("normal mode"));
        assert!(!content.contains("type Esc"));
        assert!(!content.contains("j/k"));
        assert!(!content.contains("g/G"));
        assert!(!content.contains("n/N"));
        assert!(!content.contains("Ctrl+D/U"));

        Ok(())
    }

    #[tokio::test]
    async fn app_draws_responsive_layout_modes() -> Result<()> {
        let mut wide_app = test_app().await?;
        let mut wide_terminal = Terminal::new(TestBackend::new(120, 28))?;
        wide_terminal.draw(|frame| wide_app.draw(frame))?;
        let wide = buffer_text(wide_terminal.backend().buffer());
        assert_eq!(wide_app.state.layout_mode, LayoutMode::Wide);
        assert!(wide.contains("Chats"));
        assert!(wide.contains("Messages - Family Weekend"));
        assert!(wide.contains("Compose - Family Weekend"));
        assert!(wide.contains("Details"));

        let mut medium_app = test_app().await?;
        let mut medium_terminal = Terminal::new(TestBackend::new(88, 24))?;
        medium_terminal.draw(|frame| medium_app.draw(frame))?;
        let medium = buffer_text(medium_terminal.backend().buffer());
        assert_eq!(medium_app.state.layout_mode, LayoutMode::Medium);
        assert!(medium.contains("Chats"));
        assert!(medium.contains("Messages - Family Weekend"));
        assert!(medium.contains("Compose - Family Weekend"));
        assert!(!medium.contains("Details"));

        let mut compact_app = test_app().await?;
        let mut compact_terminal = Terminal::new(TestBackend::new(60, 22))?;
        compact_terminal.draw(|frame| compact_app.draw(frame))?;
        let compact_chat = buffer_text(compact_terminal.backend().buffer());
        assert_eq!(compact_app.state.layout_mode, LayoutMode::Compact);
        assert!(compact_chat.contains("Chats"));
        assert!(!compact_chat.contains("Messages - Family Weekend"));

        compact_app
            .handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        compact_terminal.draw(|frame| compact_app.draw(frame))?;
        let compact_messages = buffer_text(compact_terminal.backend().buffer());
        assert_eq!(compact_app.state.layout_mode, LayoutMode::Compact);
        assert!(compact_messages.contains("Messages - Family Weekend"));
        assert!(compact_messages.contains("Compose - Family Weekend"));
        assert!(!compact_messages.contains("Details"));

        Ok(())
    }

    #[tokio::test]
    async fn app_draws_status_bar_scrollbars_and_grouped_bubbles() -> Result<()> {
        let mut app = test_app().await?;
        let mut terminal = Terminal::new(TestBackend::new(120, 14))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());

        assert!(content.contains("Chats"));
        assert!(content.contains("↑↓"));
        assert!(content.contains("choose chat"));
        assert!(content.contains("╭─"));
        assert!(!content.contains("continued"));
        assert!(content.contains("╰─"));

        Ok(())
    }

    #[tokio::test]
    async fn app_compose_supports_multiline_textarea_input() -> Result<()> {
        let mut app = test_app().await?;

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        for value in "Line one".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::SHIFT)))
            .await?;
        for value in "Line two".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        for value in "Line three".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }

        assert_eq!(app.state().focus(), FocusPane::Compose);
        assert_eq!(app.state().compose_text(), "Line one\nLine two\nLine three");
        assert_eq!(
            app.state().compose_cursor(),
            "Line one\nLine two\nLine three".len()
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_clamps_message_scroll_at_bottom() -> Result<()> {
        let mut app = test_app().await?;
        let mut terminal = Terminal::new(TestBackend::new(120, 30))?;
        terminal.draw(|frame| app.draw(frame))?;

        for _ in 0..4 {
            app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
                .await?;
        }
        terminal.draw(|frame| app.draw(frame))?;

        let viewport_rows = inner_area(app.state.pane_areas.messages).height as usize;
        let total_lines = app.message_line_count();
        let expected_max = bounded_message_scroll(total_lines, viewport_rows);

        for _ in 0..50 {
            app.handle_event(AppEvent::Mouse(mouse(MouseEventKind::ScrollDown, 50, 2)))
                .await?;
        }

        assert_eq!(app.state().message_scroll(), expected_max);
        assert!(app.state().message_scroll() + viewport_rows >= total_lines);
        assert!(app.state().status().contains("latest"));

        Ok(())
    }

    #[tokio::test]
    async fn app_aligns_outgoing_bubbles_right_and_incoming_left() -> Result<()> {
        let mut app = test_app().await?;
        for _ in 0..4 {
            app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
                .await?;
        }

        let messages = app.state().messages().to_vec();
        let render = message_list::build_message_lines(
            &messages,
            80,
            0,
            None,
            &mut app.media_preview_cache,
            app.theme,
        );

        assert!(render.lines.iter().any(|line| {
            line.alignment == Some(ratatui::layout::Alignment::Right)
                && line_text(line).contains("Me")
        }));
        assert!(render.lines.iter().any(|line| {
            line.alignment == Some(ratatui::layout::Alignment::Left)
                && line_text(line).contains("Designer")
        }));
        assert!(render.lines.iter().any(|line| {
            line.alignment == Some(ratatui::layout::Alignment::Left)
                && line_text(line).contains("Photo")
        }));

        Ok(())
    }

    #[test]
    fn idle_poll_timeout_is_low_cpu_oriented() {
        assert!(IDLE_POLL_TIMEOUT >= std::time::Duration::from_millis(200));
    }

    async fn test_app() -> Result<App> {
        test_app_with_providers(vec![Box::new(MockProvider::new())]).await
    }

    async fn test_app_with_providers(providers: Vec<ProviderBox>) -> Result<App> {
        let store = Arc::new(Store::open_memory().await?);
        App::new(store, providers).await
    }

    #[derive(Clone, Debug)]
    struct StaticTestProvider {
        id: ProviderId,
        account: Account,
        chats: Vec<Chat>,
        messages: Vec<Message>,
        events: EventBus,
    }

    impl StaticTestProvider {
        fn from_mock(
            id: &str,
            display_name: &str,
            mock: &MockProvider,
            prefix: &str,
        ) -> Result<Self> {
            let id = Arc::<str>::from(id);
            let account = Account {
                id: id.clone(),
                platform: Platform::Unknown("test".to_owned()),
                display_name: Arc::from(display_name),
                avatar: None,
            };
            let chats = mock
                .seed_chats()
                .into_iter()
                .map(|mut chat| {
                    chat.id = Arc::from(format!("{prefix}{}", chat.id));
                    chat.account = id.clone();
                    chat.name = Arc::from(format!("{} / {display_name}", chat.name));
                    chat
                })
                .collect::<Vec<_>>();
            let messages = mock
                .seed_messages()
                .into_iter()
                .map(|mut message| {
                    message.id = Arc::from(format!("{prefix}{}", message.id));
                    message.chat_id = Arc::from(format!("{prefix}{}", message.chat_id));
                    message.account = id.clone();
                    message.reply_to = message
                        .reply_to
                        .as_ref()
                        .map(|reply_to| Arc::from(format!("{prefix}{reply_to}")) as MessageId);
                    message.thread_id = message
                        .thread_id
                        .as_ref()
                        .map(|thread_id| Arc::from(format!("{prefix}{thread_id}")));
                    message
                })
                .collect::<Vec<_>>();

            Ok(Self {
                id,
                account,
                chats,
                messages,
                events: EventBus::new(),
            })
        }
    }

    #[async_trait::async_trait]
    impl Provider for StaticTestProvider {
        fn id(&self) -> &ProviderId {
            &self.id
        }

        fn platform(&self) -> Platform {
            self.account.platform.clone()
        }

        fn account_info(&self) -> Account {
            self.account.clone()
        }

        async fn connect(&self) -> Result<()> {
            self.events.send(ProviderEvent::AuthSucceeded);
            self.events.send(ProviderEvent::SyncComplete);
            Ok(())
        }

        async fn disconnect(&self) -> Result<()> {
            self.events.send(ProviderEvent::Disconnected(None));
            Ok(())
        }

        fn is_connected(&self) -> bool {
            true
        }

        fn events(&self) -> broadcast::Receiver<ProviderEvent> {
            self.events.subscribe()
        }

        async fn chats(&self) -> Result<Vec<Chat>> {
            Ok(self.chats.clone())
        }

        async fn history(
            &self,
            chat_id: &Arc<str>,
            before: Option<chrono::DateTime<Utc>>,
            limit: usize,
        ) -> Result<Vec<Message>> {
            let mut messages = self
                .messages
                .iter()
                .filter(|message| message.chat_id == *chat_id)
                .filter(|message| before.is_none_or(|before| message.timestamp < before))
                .cloned()
                .collect::<Vec<_>>();
            messages.sort_by_key(|message| message.timestamp);
            let start = messages.len().saturating_sub(limit);
            Ok(messages.split_off(start))
        }

        async fn send(
            &self,
            _chat_id: &Arc<str>,
            _content: Content,
            _reply_to: Option<&Arc<str>>,
        ) -> Result<Arc<str>> {
            Ok(Arc::from("static:test:sent"))
        }

        async fn download_media(&self, media: &Media) -> Result<PathBuf> {
            Ok(media
                .local_path
                .clone()
                .unwrap_or_else(|| PathBuf::from(&*media.file_name)))
        }

        async fn mark_read(&self, _chat_id: &Arc<str>, _up_to: &Arc<str>) -> Result<()> {
            Ok(())
        }

        async fn react(
            &self,
            _chat_id: &Arc<str>,
            _message_id: &Arc<str>,
            _emoji: &str,
        ) -> Result<()> {
            Ok(())
        }

        async fn search(&self, query: &str, limit: usize) -> Result<Vec<Message>> {
            let query = query.to_lowercase();
            Ok(self
                .messages
                .iter()
                .filter(|message| {
                    content_copy_text(&message.content)
                        .to_lowercase()
                        .contains(&query)
                })
                .take(limit)
                .cloned()
                .collect())
        }

        async fn contact_info(&self, platform_id: &Arc<str>) -> Result<Option<Sender>> {
            Ok(Some(Sender {
                platform_id: platform_id.clone(),
                display_name: platform_id.clone(),
                avatar: None,
            }))
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn test_incoming_message(chat: &Chat, id: &str, sender: &str, text: &str) -> Message {
        Message {
            id: Arc::from(id),
            chat_id: chat.id.clone(),
            account: chat.account.clone(),
            sender: Sender {
                platform_id: Arc::from(format!("mock:user:{sender}")),
                display_name: Arc::from(sender),
                avatar: None,
            },
            timestamp: Utc::now(),
            edited_at: None,
            content: Content::Text(Arc::from(text)),
            reply_to: None,
            thread_id: None,
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me: false,
            platform_data: PlatformData::default(),
        }
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn rgb_cell_at(buffer: &ratatui::buffer::Buffer, x: u16, y: u16) -> bool {
        buffer.cell((x, y)).is_some_and(|cell| {
            cell.symbol() == "▀"
                && (matches!(cell.fg, Color::Rgb(_, _, _))
                    || matches!(cell.bg, Color::Rgb(_, _, _)))
        })
    }
}
