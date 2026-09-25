//! Compose attachments: native file pickers, drag-and-drop path parsing and
//! small presentation helpers shared by the compose tray and document cards.
//!
//! Everything here that touches the filesystem or spawns processes is meant to
//! run on background workers; the UI thread only calls the pure helpers
//! (command construction, output parsing, labels).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Environment override naming the picker backend (`kdialog`, `zenity`,
/// `yad`, `qarma`, `osascript`, `portal`) or `none` to disable native pickers.
pub const FILE_PICKER_ENV: &str = "CHAT_CLI_FILE_PICKER";

/// Upper bound for how long a picker dialog may stay open before the worker
/// gives up on it. Generous because the user may browse for a while.
pub const FILE_PICKER_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Maximum number of files accepted from one picker selection or drop.
pub const MAX_ATTACHMENTS_PER_PICK: usize = 10;

/// Maximum number of whitespace/newline separated tokens inspected when a
/// paste is checked for dropped file paths; larger pastes are plain text.
const MAX_DROPPED_TOKENS: usize = 32;

const MEDIA_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "bmp", "mp4", "mov", "m4v", "webm", "mkv",
    "3gp", "avi",
];
const STICKER_EXTENSIONS: &[&str] = &["webp", "png", "gif"];

/// What the user asked to attach, mirroring the native apps' attach sheet.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PickerMode {
    /// Photos and videos, sent as inline media with previews.
    Media,
    /// Any file, sent as a document (original bytes, no recompression).
    Document,
    /// A single sticker image.
    Sticker,
}

impl PickerMode {
    pub fn title(self) -> &'static str {
        match self {
            Self::Media => "Attach photos & videos",
            Self::Document => "Attach documents",
            Self::Sticker => "Attach a sticker",
        }
    }

    fn filter_label(self) -> &'static str {
        match self {
            Self::Media => "Photos & videos",
            Self::Document => "All files",
            Self::Sticker => "Stickers",
        }
    }

    fn extensions(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Media => Some(MEDIA_EXTENSIONS),
            Self::Document => None,
            Self::Sticker => Some(STICKER_EXTENSIONS),
        }
    }

    fn multiple(self) -> bool {
        !matches!(self, Self::Sticker)
    }

    fn glob_patterns(self) -> String {
        self.extensions()
            .map(|extensions| {
                extensions
                    .iter()
                    .flat_map(|ext| [format!("*.{ext}"), format!("*.{}", ext.to_uppercase())])
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_else(|| "*".to_owned())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickerBackend {
    KDialog,
    Zenity,
    Qarma,
    Yad,
    Osascript,
    /// `org.freedesktop.portal.FileChooser` over D-Bus: the desktop's own
    /// dialog, and the only option when no picker program is installed.
    Portal,
}

impl PickerBackend {
    pub fn program(self) -> &'static str {
        match self {
            Self::KDialog => "kdialog",
            Self::Zenity => "zenity",
            Self::Qarma => "qarma",
            Self::Yad => "yad",
            Self::Osascript => "osascript",
            Self::Portal => "xdg-desktop-portal",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "kdialog" => Some(Self::KDialog),
            "zenity" => Some(Self::Zenity),
            "qarma" => Some(Self::Qarma),
            "yad" => Some(Self::Yad),
            "osascript" => Some(Self::Osascript),
            "portal" | "xdg-desktop-portal" => Some(Self::Portal),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerError {
    /// The user closed the dialog without choosing anything.
    Cancelled,
    /// No usable picker program (or no graphical session) was found.
    Unavailable(String),
    /// The picker ran but failed.
    Failed(String),
}

impl std::fmt::Display for PickerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("cancelled"),
            Self::Unavailable(reason) | Self::Failed(reason) => formatter.write_str(reason),
        }
    }
}

/// Picker backends to try, in order, for this session. Reads the
/// environment and `PATH`, so call it from a worker thread. Installed
/// programs come first; the D-Bus portal is always the last resort because
/// its availability is only known by calling it.
pub fn picker_candidates() -> Result<Vec<PickerBackend>, PickerError> {
    if let Ok(name) = std::env::var(FILE_PICKER_ENV) {
        if name.trim().eq_ignore_ascii_case("none") {
            return Err(PickerError::Unavailable(format!(
                "native file picker disabled by {FILE_PICKER_ENV}"
            )));
        }
        return PickerBackend::from_name(&name)
            .map(|backend| vec![backend])
            .ok_or_else(|| {
                PickerError::Unavailable(format!("unknown {FILE_PICKER_ENV} value `{name}`"))
            });
    }

    if cfg!(target_os = "macos") {
        return Ok(vec![PickerBackend::Osascript]);
    }

    let graphical =
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    if !graphical {
        return Err(PickerError::Unavailable(
            "no graphical session for a file picker".to_owned(),
        ));
    }

    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let mut candidates: Vec<PickerBackend> = picker_preference(&desktop)
        .into_iter()
        .filter(|backend| program_on_path(backend.program()))
        .collect();
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
        candidates.push(PickerBackend::Portal);
    }
    if candidates.is_empty() {
        return Err(PickerError::Unavailable(
            "no file picker found (install zenity or kdialog)".to_owned(),
        ));
    }
    Ok(candidates)
}

/// Backend preference order for a desktop: KDE sessions get the Qt dialog
/// first, everything else the GTK one.
fn picker_preference(desktop: &str) -> Vec<PickerBackend> {
    let kde = desktop
        .split(':')
        .any(|part| part.eq_ignore_ascii_case("kde") || part.eq_ignore_ascii_case("lxqt"));
    if kde {
        vec![
            PickerBackend::KDialog,
            PickerBackend::Qarma,
            PickerBackend::Zenity,
            PickerBackend::Yad,
        ]
    } else {
        vec![
            PickerBackend::Zenity,
            PickerBackend::KDialog,
            PickerBackend::Qarma,
            PickerBackend::Yad,
        ]
    }
}

pub(crate) fn program_on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join(program)))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Program and arguments that open `backend`'s picker for `mode`.
pub fn picker_command(
    backend: PickerBackend,
    mode: PickerMode,
    start_dir: Option<&Path>,
) -> (String, Vec<String>) {
    let title = mode.title().to_owned();
    let start = start_dir.map(|dir| {
        let mut value = dir.display().to_string();
        if !value.ends_with('/') {
            value.push('/');
        }
        value
    });
    let mut args = Vec::new();
    match backend {
        PickerBackend::KDialog => {
            args.extend(["--title".to_owned(), title, "--getopenfilename".to_owned()]);
            if mode.multiple() {
                args.extend(["--multiple".to_owned(), "--separate-output".to_owned()]);
            }
            args.push(start.unwrap_or_else(|| ".".to_owned()));
            args.push(format!("{}|{}", mode.glob_patterns(), mode.filter_label()));
        }
        PickerBackend::Zenity | PickerBackend::Qarma | PickerBackend::Yad => {
            args.push(if backend == PickerBackend::Yad {
                "--file".to_owned()
            } else {
                "--file-selection".to_owned()
            });
            args.push(format!("--title={title}"));
            if mode.multiple() {
                args.extend(["--multiple".to_owned(), "--separator=\n".to_owned()]);
            }
            if let Some(start) = start {
                args.push(format!("--filename={start}"));
            }
            if mode.extensions().is_some() {
                args.push(format!(
                    "--file-filter={} | {}",
                    mode.filter_label(),
                    mode.glob_patterns()
                ));
                args.push("--file-filter=All files | *".to_owned());
            }
        }
        // The portal is driven over D-Bus, not as a program.
        PickerBackend::Portal => {}
        PickerBackend::Osascript => {
            let mut choose = format!("choose file with prompt \"{title}\"");
            match mode {
                PickerMode::Media => {
                    choose.push_str(" of type {\"public.image\", \"public.movie\"}")
                }
                PickerMode::Sticker => choose.push_str(" of type {\"public.image\"}"),
                PickerMode::Document => {}
            }
            if let Some(start) = start {
                choose.push_str(&format!(
                    " default location (POSIX file \"{}\")",
                    start.replace('"', "\\\"")
                ));
            }
            if mode.multiple() {
                choose.push_str(" with multiple selections allowed");
            }
            for line in [
                format!("set picked to ({choose})"),
                "if class of picked is not list then set picked to {picked}".to_owned(),
                "set out to \"\"".to_owned(),
                "repeat with item_ref in picked".to_owned(),
                "set out to out & POSIX path of item_ref & linefeed".to_owned(),
                "end repeat".to_owned(),
                "return out".to_owned(),
            ] {
                args.extend(["-e".to_owned(), line]);
            }
        }
    }
    (backend.program().to_owned(), args)
}

/// Parses the picker's stdout (one path per line) into absolute paths.
pub fn parse_picker_output(stdout: &str) -> Vec<PathBuf> {
    stdout
        .lines()
        .map(|line| line.trim_end_matches('\r').trim())
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.strip_prefix("file://")
                .map(percent_decode)
                .unwrap_or_else(|| line.to_owned())
        })
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .take(MAX_ATTACHMENTS_PER_PICK)
        .collect()
}

/// Folder the picker opens in: the last folder used, else the usual
/// per-mode user folder, else the home directory. Touches the filesystem.
pub fn picker_start_dir(mode: PickerMode, last_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = last_dir.filter(|dir| dir.is_dir()) {
        return Some(dir.to_path_buf());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let preferred = match mode {
        PickerMode::Media | PickerMode::Sticker => "Pictures",
        PickerMode::Document => "Documents",
    };
    let candidate = home.join(preferred);
    Some(if candidate.is_dir() { candidate } else { home })
}

/// Exit status the dynamic loader and shells use when a program cannot
/// start (missing shared library, bad interpreter): try the next picker.
const EXIT_CANNOT_START: i32 = 127;

/// Runs the native picker to completion, trying each candidate backend in
/// turn: a picker that cannot start (missing, or broken by a missing shared
/// library) falls through to the next one instead of failing the attach.
/// The child is killed when the returned future is dropped.
pub async fn run_file_picker(
    mode: PickerMode,
    last_dir: Option<PathBuf>,
) -> Result<Vec<PathBuf>, PickerError> {
    let (candidates, start_dir) = tokio::task::spawn_blocking(move || {
        picker_candidates()
            .map(|candidates| (candidates, picker_start_dir(mode, last_dir.as_deref())))
    })
    .await
    .map_err(|error| PickerError::Failed(format!("picker worker failed: {error}")))??;

    let mut failures = Vec::new();
    for backend in candidates {
        let result = if backend == PickerBackend::Portal {
            run_portal_picker(mode, start_dir.as_deref()).await
        } else {
            run_program_picker(backend, mode, start_dir.as_deref()).await
        };
        match result {
            Err(PickerError::Unavailable(reason)) => failures.push(reason),
            other => return other,
        }
    }
    Err(PickerError::Unavailable(format!(
        "no working file picker ({})",
        failures.join("; ")
    )))
}

async fn run_program_picker(
    backend: PickerBackend,
    mode: PickerMode,
    start_dir: Option<&Path>,
) -> Result<Vec<PathBuf>, PickerError> {
    let (program, args) = picker_command(backend, mode, start_dir);
    let child = tokio::process::Command::new(&program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| PickerError::Unavailable(format!("cannot start {program}: {error}")))?;
    let output = tokio::time::timeout(FILE_PICKER_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| PickerError::Failed(format!("{program} timed out")))?
        .map_err(|error| PickerError::Failed(format!("{program} failed: {error}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = first_line(&stderr);
        if output.status.code() == Some(EXIT_CANNOT_START) {
            return Err(PickerError::Unavailable(format!(
                "{program} is broken: {reason}"
            )));
        }
        // Every supported picker exits 1 when the dialog is dismissed.
        if output.status.code() == Some(1) {
            return Err(PickerError::Cancelled);
        }
        return Err(PickerError::Failed(format!(
            "{program} exited with {}{}{}",
            output.status,
            if reason.is_empty() { "" } else { ": " },
            reason
        )));
    }
    let paths = parse_picker_output(&stdout);
    if paths.is_empty() {
        return Err(PickerError::Cancelled);
    }
    Ok(paths)
}

pub(crate) fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
/// Portal `filters` option: `a(sa(us))`, where `0` marks a glob pattern.
fn portal_filters(mode: PickerMode) -> Vec<(String, Vec<(u32, String)>)> {
    let Some(extensions) = mode.extensions() else {
        return Vec::new();
    };
    let globs = extensions
        .iter()
        .flat_map(|ext| [format!("*.{ext}"), format!("*.{}", ext.to_uppercase())])
        .map(|glob| (0u32, glob))
        .collect();
    vec![
        (mode.filter_label().to_owned(), globs),
        ("All files".to_owned(), vec![(0, "*".to_owned())]),
    ]
}

#[cfg(target_os = "linux")]
/// Closes a still-open portal dialog when the pick is abandoned (the user
/// pressed Esc in the TUI, changed chat, or quit).
struct PortalRequestGuard {
    connection: zbus::Connection,
    path: String,
    armed: bool,
}

#[cfg(target_os = "linux")]
impl Drop for PortalRequestGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let connection = self.connection.clone();
        let path = self.path.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = connection
                    .call_method(
                        Some("org.freedesktop.portal.Desktop"),
                        path.as_str(),
                        Some("org.freedesktop.portal.Request"),
                        "Close",
                        &(),
                    )
                    .await;
            });
        }
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_portal_picker(
    _mode: PickerMode,
    _start_dir: Option<&Path>,
) -> Result<Vec<PathBuf>, PickerError> {
    Err(PickerError::Unavailable(
        "file portal is Linux-only".to_owned(),
    ))
}

/// Opens the desktop's file chooser through `org.freedesktop.portal`.
#[cfg(target_os = "linux")]
async fn run_portal_picker(
    mode: PickerMode,
    start_dir: Option<&Path>,
) -> Result<Vec<PathBuf>, PickerError> {
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedValue, Value};

    let unavailable =
        |error: zbus::Error| PickerError::Unavailable(format!("file portal: {error}"));
    let connection = zbus::Connection::session().await.map_err(unavailable)?;
    let sender = connection
        .unique_name()
        .map(|name| name.trim_start_matches(':').replace('.', "_"))
        .ok_or_else(|| PickerError::Unavailable("file portal: no bus name".to_owned()))?;
    let token = format!(
        "chat_cli_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_micros())
            .unwrap_or_default()
    );
    let request_path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");

    // Subscribe before calling so a fast response cannot be missed.
    let request = zbus::Proxy::new(
        &connection,
        "org.freedesktop.portal.Desktop",
        request_path.clone(),
        "org.freedesktop.portal.Request",
    )
    .await
    .map_err(unavailable)?;
    let mut responses = request
        .receive_signal("Response")
        .await
        .map_err(unavailable)?;

    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    options.insert("modal", Value::from(true));
    options.insert("multiple", Value::from(mode.multiple()));
    let filters = portal_filters(mode);
    if !filters.is_empty() {
        options.insert("filters", Value::from(filters));
    }
    if let Some(dir) = start_dir {
        let mut bytes = dir.as_os_str().as_encoded_bytes().to_vec();
        bytes.push(0);
        options.insert("current_folder", Value::from(bytes));
    }
    connection
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.FileChooser"),
            "OpenFile",
            &("", mode.title(), options),
        )
        .await
        .map_err(unavailable)?;
    let mut guard = PortalRequestGuard {
        connection: connection.clone(),
        path: request_path,
        armed: true,
    };

    let message = tokio::time::timeout(FILE_PICKER_TIMEOUT, async {
        use futures_util::StreamExt;
        responses.next().await
    })
    .await
    .map_err(|_| PickerError::Failed("file portal timed out".to_owned()))?
    .ok_or_else(|| PickerError::Failed("file portal closed the request".to_owned()))?;
    guard.armed = false;

    let (code, results): (u32, HashMap<String, OwnedValue>) = message
        .body()
        .deserialize()
        .map_err(|error| PickerError::Failed(format!("file portal reply: {error}")))?;
    match code {
        0 => {}
        1 => return Err(PickerError::Cancelled),
        other => return Err(PickerError::Failed(format!("file portal failed ({other})"))),
    }
    let uris: Vec<String> = results
        .get("uris")
        .and_then(|value| Vec::<String>::try_from(value.clone()).ok())
        .unwrap_or_default();
    let paths = parse_picker_output(&uris.join("\n"));
    if paths.is_empty() {
        return Err(PickerError::Cancelled);
    }
    Ok(paths)
}

/// Interprets a bracketed paste as files dropped onto the terminal.
///
/// Terminals paste dropped files as shell-quoted paths separated by spaces,
/// as one path per line, or as `file://` URIs. Returns `Some` only when every
/// token is an absolute path to an existing regular file, so ordinary text
/// (including a lone relative file name) is never mistaken for a drop.
/// Performs at most [`MAX_DROPPED_TOKENS`] `stat` calls.
pub fn parse_dropped_paths(text: &str) -> Option<Vec<PathBuf>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let tokens: Vec<String> = if trimmed.contains('\n') {
        trimmed
            .lines()
            .map(|line| line.trim_end_matches('\r').trim())
            .filter(|line| !line.is_empty())
            .map(|line| {
                shell_words(line)
                    .filter(|words| words.len() == 1)
                    .map(|mut words| words.remove(0))
                    .unwrap_or_else(|| line.to_owned())
            })
            .collect()
    } else {
        shell_words(trimmed)?
    };
    if tokens.is_empty() || tokens.len() > MAX_DROPPED_TOKENS {
        return None;
    }

    let mut paths = Vec::with_capacity(tokens.len());
    for token in tokens {
        let path = dropped_token_path(&token)?;
        if !std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
            return None;
        }
        paths.push(path);
    }
    Some(paths)
}

/// Resolves one path token (`file://` URI, `~/…`, or absolute path) to an
/// absolute path. Does not touch the filesystem.
pub fn dropped_token_path(token: &str) -> Option<PathBuf> {
    let raw = if let Some(rest) = token.strip_prefix("file://") {
        // `file://localhost/path` and `file:///path` are both valid.
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        percent_decode(rest)
    } else if token == "~" || token.starts_with("~/") {
        let home = std::env::var("HOME").ok()?;
        format!("{home}{}", &token[1..])
    } else {
        token.to_owned()
    };
    let path = PathBuf::from(raw);
    path.is_absolute().then_some(path)
}

/// Minimal POSIX-shell word splitting: whitespace separates words, single
/// quotes are literal, double quotes allow `\"`/`\\` escapes, and a backslash
/// outside quotes escapes the next character. Returns `None` for unbalanced
/// quotes.
pub(crate) fn shell_words(input: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = input.chars();
    while let Some(character) = chars.next() {
        match character {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        other => current.push(other),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            escaped @ ('"' | '\\' | '$' | '`') => current.push(escaped),
                            other => {
                                current.push('\\');
                                current.push(other);
                            }
                        },
                        other => current.push(other),
                    }
                }
            }
            '\\' => {
                in_word = true;
                current.push(chars.next()?);
            }
            whitespace if whitespace.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            other => {
                in_word = true;
                current.push(other);
            }
        }
    }
    if in_word {
        words.push(current);
    }
    Some(words)
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            decoded.push(high << 4 | low);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Short uppercase type badge for a file, like the icon label native apps
/// draw on document bubbles (`PDF`, `DOCX`, `ZIP`). Falls back to the MIME
/// subtype and finally to `FILE`.
pub fn file_type_badge(file_name: &str, mime_type: &str) -> String {
    let extension = Path::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 5
                && extension.chars().all(|c| c.is_ascii_alphanumeric())
        });
    if let Some(extension) = extension {
        return extension.to_ascii_uppercase();
    }
    mime_type
        .split('/')
        .nth(1)
        .map(|subtype| subtype.rsplit(['.', '-', '+']).next().unwrap_or(subtype))
        .filter(|subtype| !subtype.is_empty() && subtype.len() <= 5 && *subtype != "octet")
        .map(str::to_ascii_uppercase)
        .unwrap_or_else(|| "FILE".to_owned())
}

/// Compact byte size, e.g. `1.2 MB`, `640 KB`, `12 B`.
pub fn format_byte_size(size: u64) -> String {
    if size >= 1_000_000_000 {
        format!("{:.1} GB", size as f64 / 1_000_000_000.0)
    } else if size >= 1_000_000 {
        format!("{:.1} MB", size as f64 / 1_000_000.0)
    } else if size >= 1_000 {
        format!("{} KB", size / 1_000)
    } else {
        format!("{size} B")
    }
}

/// Human description of a document's type, e.g. `PDF document`.
pub fn file_type_description(file_name: &str, mime_type: &str) -> String {
    let badge = file_type_badge(file_name, mime_type);
    let kind = match badge.as_str() {
        "PDF" => "PDF document",
        "DOC" | "DOCX" | "ODT" | "RTF" | "PAGES" => "Word document",
        "XLS" | "XLSX" | "ODS" | "CSV" | "NUMBE" => "Spreadsheet",
        "PPT" | "PPTX" | "ODP" | "KEY" => "Presentation",
        "ZIP" | "RAR" | "7Z" | "TAR" | "GZ" | "XZ" | "BZ2" | "ZST" => "Archive",
        "TXT" | "MD" | "LOG" => "Text file",
        "MP3" | "M4A" | "OGG" | "OPUS" | "WAV" | "FLAC" | "AAC" => "Audio",
        "APK" => "Android app",
        _ => return format!("{badge} file"),
    };
    kind.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kdialog_command_filters_media_and_allows_multiple() {
        let (program, args) = picker_command(
            PickerBackend::KDialog,
            PickerMode::Media,
            Some(Path::new("/home/u")),
        );
        assert_eq!(program, "kdialog");
        assert!(args.contains(&"--getopenfilename".to_owned()));
        assert!(args.contains(&"--multiple".to_owned()));
        assert!(args.contains(&"--separate-output".to_owned()));
        assert!(args.contains(&"/home/u/".to_owned()));
        let filter = args.last().unwrap();
        assert!(filter.contains("*.jpg") && filter.contains("*.mp4"));
        assert!(filter.ends_with("|Photos & videos"));
    }

    #[test]
    fn zenity_document_command_has_no_type_filter() {
        let (program, args) = picker_command(PickerBackend::Zenity, PickerMode::Document, None);
        assert_eq!(program, "zenity");
        assert!(args.contains(&"--file-selection".to_owned()));
        assert!(args.contains(&"--separator=\n".to_owned()));
        assert!(!args.iter().any(|arg| arg.starts_with("--file-filter")));
    }

    #[test]
    fn sticker_picker_is_single_selection() {
        let (_, args) = picker_command(PickerBackend::Yad, PickerMode::Sticker, None);
        assert_eq!(args[0], "--file");
        assert!(!args.contains(&"--multiple".to_owned()));
        assert!(args.iter().any(|arg| arg.contains("*.webp")));
        let (_, mac) = picker_command(PickerBackend::Osascript, PickerMode::Sticker, None);
        assert!(!mac.iter().any(|arg| arg.contains("multiple selections")));
    }

    #[test]
    fn kde_sessions_prefer_kdialog() {
        assert_eq!(
            PickerBackend::from_name("portal"),
            Some(PickerBackend::Portal)
        );
        let filters = portal_filters(PickerMode::Sticker);
        assert_eq!(filters.len(), 2);
        assert!(filters[0].1.contains(&(0, "*.webp".to_owned())));
        assert!(portal_filters(PickerMode::Document).is_empty());
        assert_eq!(picker_preference("KDE")[0], PickerBackend::KDialog);
        assert_eq!(picker_preference("GNOME")[0], PickerBackend::Zenity);
    }

    #[test]
    fn picker_output_keeps_absolute_paths_and_decodes_uris() {
        let paths = parse_picker_output("/a/b.png\n\nrelative.txt\nfile:///c/d%20e.pdf\r\n");
        assert_eq!(
            paths,
            vec![PathBuf::from("/a/b.png"), PathBuf::from("/c/d e.pdf")]
        );
    }

    #[test]
    fn dropped_paths_accept_quoted_escaped_and_uri_forms() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let spaced = dir.path().join("my photo.jpg");
        let plain = dir.path().join("report.pdf");
        std::fs::write(&spaced, b"x")?;
        std::fs::write(&plain, b"x")?;

        let quoted = format!("'{}' {}", spaced.display(), plain.display());
        assert_eq!(
            parse_dropped_paths(&quoted),
            Some(vec![spaced.clone(), plain.clone()])
        );

        let escaped = spaced.display().to_string().replace(' ', "\\ ");
        assert_eq!(parse_dropped_paths(&escaped), Some(vec![spaced.clone()]));

        let uri = format!(
            "file://{}",
            spaced.display().to_string().replace(' ', "%20")
        );
        assert_eq!(parse_dropped_paths(&uri), Some(vec![spaced.clone()]));

        let lines = format!("{}\n{}\n", spaced.display(), plain.display());
        assert_eq!(parse_dropped_paths(&lines), Some(vec![spaced, plain]));
        Ok(())
    }

    #[test]
    fn ordinary_text_is_not_a_drop() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x")?;
        assert_eq!(parse_dropped_paths("hello world"), None);
        assert_eq!(parse_dropped_paths("README.md"), None);
        assert_eq!(parse_dropped_paths(""), None);
        // One real path plus prose is text, not a drop.
        assert_eq!(
            parse_dropped_paths(&format!("see {}", file.display())),
            None
        );
        // Directories are not attachable.
        assert_eq!(parse_dropped_paths(&dir.path().display().to_string()), None);
        assert_eq!(parse_dropped_paths("'unbalanced"), None);
        Ok(())
    }

    #[test]
    fn badges_describe_common_documents() {
        assert_eq!(file_type_badge("Invoice.PDF", "application/pdf"), "PDF");
        assert_eq!(file_type_badge("blob", "application/zip"), "ZIP");
        assert_eq!(file_type_badge("blob", "application/octet-stream"), "FILE");
        assert_eq!(
            file_type_badge(
                "x",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            ),
            "FILE"
        );
        assert_eq!(file_type_description("a.docx", ""), "Word document");
        assert_eq!(file_type_description("a.xyz", ""), "XYZ file");
    }
}
