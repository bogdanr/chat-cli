//! Opening received media in desktop apps (play a video, open a PDF).
//!
//! `xdg-open` is not trustworthy on its own: in a session without a known
//! desktop it runs the default app in the background and exits 0 even when
//! that app cannot start (e.g. a player broken by a missing shared library).
//! So we resolve the default app ourselves, launch it directly, and watch it
//! briefly; if it dies at once we try the next candidate.
//!
//! Everything here blocks (filesystem lookups, `xdg-mime`, the startup
//! watch), so call [`open_verified`] from a blocking worker only.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Override for opening any file: a command line, the path is appended.
pub const OPENER_ENV: &str = "CHAT_CLI_OPENER";
/// Override for playing videos and voice notes.
pub const VIDEO_PLAYER_ENV: &str = "CHAT_CLI_VIDEO_PLAYER";

/// How long a launched app is watched for an immediate crash.
const STARTUP_WATCH: Duration = Duration::from_millis(1500);
const WATCH_STEP: Duration = Duration::from_millis(50);

/// What is being opened; picks the fallback apps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenKind {
    Video,
    Audio,
    Document,
}

impl OpenKind {
    pub fn verb(self) -> &'static str {
        match self {
            Self::Video | Self::Audio => "playing",
            Self::Document => "opened",
        }
    }
}

/// One way to open the file: program plus arguments (path included).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    /// Hands off and cannot be verified (`xdg-open`, `open`): last resort.
    pub unverified: bool,
}

/// Name of the app that ended up handling the file.
pub type Opened = String;

const PLAYERS: &[&str] = &[
    "mpv",
    "vlc",
    "celluloid",
    "totem",
    "haruna",
    "smplayer",
    "mplayer",
    "ffplay",
];
const DOCUMENT_VIEWERS: &[&str] = &[
    "okular", "evince", "zathura", "atril", "mupdf", "qpdfview", "eog", "gwenview", "feh",
];
const BROWSERS: &[&str] = &[
    "firefox",
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "brave",
    "brave-browser",
];

/// Ordered, de-duplicated ways to open `path`.
pub fn launch_candidates(path: &Path, mime_type: &str, kind: OpenKind) -> Vec<Launch> {
    let path_arg = path.display().to_string();
    let mut candidates = Vec::new();

    let env_override = match kind {
        OpenKind::Video | OpenKind::Audio => std::env::var(VIDEO_PLAYER_ENV)
            .ok()
            .or_else(|| std::env::var(OPENER_ENV).ok()),
        OpenKind::Document => std::env::var(OPENER_ENV).ok(),
    };
    if let Some(words) = env_override
        .as_deref()
        .and_then(crate::attach::shell_words)
        .filter(|words| !words.is_empty())
    {
        let mut words = words.into_iter();
        let program = words.next().unwrap_or_default();
        let mut args: Vec<String> = words.collect();
        args.push(path_arg.clone());
        candidates.push(Launch {
            program,
            args,
            unverified: false,
        });
    }

    if cfg!(target_os = "macos") {
        candidates.push(Launch {
            program: "open".to_owned(),
            args: vec![path_arg],
            unverified: true,
        });
        return candidates;
    }
    if cfg!(windows) {
        // `start` hands the file to its registered app; the empty argument
        // is the window title `start` expects before a quoted path.
        candidates.push(Launch {
            program: "cmd".to_owned(),
            args: vec!["/C".to_owned(), "start".to_owned(), String::new(), path_arg],
            unverified: true,
        });
        return candidates;
    }

    if let Some(launch) = default_app_launch(mime_type, &path_arg) {
        candidates.push(launch);
    }
    let fallbacks: &[&[&str]] = match kind {
        OpenKind::Video | OpenKind::Audio => &[PLAYERS, BROWSERS],
        OpenKind::Document => &[DOCUMENT_VIEWERS, BROWSERS],
    };
    for program in fallbacks.iter().flat_map(|list| list.iter()) {
        if !crate::attach::program_on_path(program) {
            continue;
        }
        let mut args = Vec::new();
        if *program == "ffplay" {
            args.extend([
                "-autoexit".to_owned(),
                "-loglevel".to_owned(),
                "error".to_owned(),
            ]);
        }
        args.push(path_arg.clone());
        candidates.push(Launch {
            program: (*program).to_owned(),
            args,
            unverified: false,
        });
    }
    for program in ["xdg-open", "gio"] {
        if crate::attach::program_on_path(program) {
            let mut args = Vec::new();
            if program == "gio" {
                args.push("open".to_owned());
            }
            args.push(path_arg.clone());
            candidates.push(Launch {
                program: program.to_owned(),
                args,
                unverified: true,
            });
            break;
        }
    }

    let mut seen = std::collections::HashSet::new();
    candidates.retain(|launch| seen.insert(program_name(&launch.program).to_owned()));
    candidates
}

fn program_name(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
}

/// The desktop's default app for `mime_type`, from `xdg-mime` and the
/// matching `.desktop` file's `Exec=` line.
fn default_app_launch(mime_type: &str, path_arg: &str) -> Option<Launch> {
    if mime_type.is_empty() || !crate::attach::program_on_path("xdg-mime") {
        return None;
    }
    let output = Command::new("xdg-mime")
        .args(["query", "default", mime_type])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let desktop_id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if desktop_id.is_empty() {
        return None;
    }
    let contents = application_dirs()
        .into_iter()
        .map(|dir| dir.join(&desktop_id))
        .find_map(|file| std::fs::read_to_string(file).ok())?;
    let exec = desktop_entry_exec(&contents)?;
    exec_launch(&exec, path_arg)
}

fn application_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match std::env::var_os("XDG_DATA_HOME") {
        Some(home) => dirs.push(PathBuf::from(home)),
        None => {
            if let Some(home) = std::env::var_os("HOME") {
                dirs.push(PathBuf::from(home).join(".local/share"));
            }
        }
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    dirs.extend(data_dirs.split(':').map(PathBuf::from));
    dirs.into_iter()
        .map(|dir| dir.join("applications"))
        .collect()
}

/// `Exec=` of the `[Desktop Entry]` group (not of an action group).
pub fn desktop_entry_exec(contents: &str) -> Option<String> {
    let mut in_entry = false;
    for line in contents.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if in_entry && let Some(exec) = line.strip_prefix("Exec=") {
            return Some(exec.trim().to_owned());
        }
    }
    None
}

/// Expands a desktop `Exec=` line for one file: `%f %F %u %U` become the
/// path, other field codes are dropped, and `%%` is a literal percent.
pub fn exec_launch(exec: &str, path_arg: &str) -> Option<Launch> {
    let words = crate::attach::shell_words(exec)?;
    let mut args = Vec::new();
    let mut used_path = false;
    for word in words {
        match word.as_str() {
            "%f" | "%F" | "%u" | "%U" => {
                args.push(path_arg.to_owned());
                used_path = true;
            }
            "%i" | "%c" | "%k" | "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => {}
            _ => args.push(word.replace("%%", "%")),
        }
    }
    if !used_path {
        args.push(path_arg.to_owned());
    }
    let program = (!args.is_empty()).then(|| args.remove(0))?;
    Some(Launch {
        program,
        args,
        unverified: false,
    })
}

/// Opens `path` with the first candidate that actually starts. Blocking.
pub fn open_verified(path: &Path, mime_type: &str, kind: OpenKind) -> Result<Opened, String> {
    if !path.is_file() {
        return Err(format!("{} is not downloaded", path.display()));
    }
    let candidates = launch_candidates(path, mime_type, kind);
    if candidates.is_empty() {
        return Err("no app found to open it".to_owned());
    }
    let mut failures = Vec::new();
    for launch in candidates {
        match start_and_watch(&launch, STARTUP_WATCH) {
            Ok(()) => return Ok(program_name(&launch.program).to_owned()),
            Err(reason) => failures.push(format!("{}: {reason}", program_name(&launch.program))),
        }
    }
    Err(format!("no app could open it ({})", failures.join("; ")))
}

/// Starts `launch` detached from the terminal and waits up to `watch` for
/// an immediate failure. A process still running after `watch`, or one that
/// exits successfully (handing off to an already running instance), counts
/// as success.
pub fn start_and_watch(launch: &Launch, watch: Duration) -> Result<(), String> {
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group: terminal signals (Ctrl+C) do not reach the app.
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let mut stderr = child.stderr.take();
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let mut text = String::new();
                if let Some(stderr) = stderr.as_mut() {
                    use std::io::Read;
                    let _ = stderr.take(8 * 1024).read_to_string(&mut text);
                }
                let reason = crate::attach::first_line(&text);
                return Err(if reason.is_empty() {
                    format!("exited with {status}")
                } else {
                    reason.to_owned()
                });
            }
            Ok(None) if launch.unverified || started.elapsed() >= watch => break,
            Ok(None) => std::thread::sleep(WATCH_STEP),
            Err(error) => return Err(error.to_string()),
        }
    }
    // Still running: keep draining stderr (so the app never blocks on a full
    // pipe) and reap it when it exits.
    std::thread::spawn(move || {
        if let Some(mut stderr) = stderr {
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        }
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_exec_is_read_from_the_main_entry_only() {
        let contents = "[Desktop Entry]\nName=Player\nExec=gmplayer --fs %F\n\n[Desktop Action New]\nExec=gmplayer --new\n";
        assert_eq!(
            desktop_entry_exec(contents).as_deref(),
            Some("gmplayer --fs %F")
        );
        let action_first = "[Desktop Action X]\nExec=wrong\n[Desktop Entry]\nExec=right %u\n";
        assert_eq!(
            desktop_entry_exec(action_first).as_deref(),
            Some("right %u")
        );
    }

    #[test]
    fn exec_field_codes_expand_to_the_file() {
        let launch =
            exec_launch("mpv --player-operation-mode=pseudo-gui -- %U", "/v/a b.mp4").unwrap();
        assert_eq!(launch.program, "mpv");
        assert_eq!(
            launch.args,
            ["--player-operation-mode=pseudo-gui", "--", "/v/a b.mp4"]
        );
        let launch = exec_launch("okular %i --caption %c", "/d.pdf").unwrap();
        assert_eq!(launch.args, ["--caption", "/d.pdf"]);
        let launch = exec_launch("\"/opt/My App/app\" --x=100%%", "/f").unwrap();
        assert_eq!(launch.program, "/opt/My App/app");
        assert_eq!(launch.args, ["--x=100%", "/f"]);
    }

    #[cfg(unix)]
    #[test]
    fn broken_app_is_reported_and_working_app_is_accepted() {
        let broken = Launch {
            program: "sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "echo 'libjxl.so.0.12: cannot open shared object file' >&2; exit 127".to_owned(),
            ],
            unverified: false,
        };
        let error = start_and_watch(&broken, Duration::from_secs(2)).unwrap_err();
        assert!(error.contains("libjxl"), "{error}");

        let running = Launch {
            program: "sleep".to_owned(),
            args: vec!["5".to_owned()],
            unverified: false,
        };
        let started = Instant::now();
        start_and_watch(&running, Duration::from_millis(200)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));

        let handoff = Launch {
            program: "true".to_owned(),
            args: Vec::new(),
            unverified: false,
        };
        start_and_watch(&handoff, Duration::from_secs(2)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn env_override_comes_first() {
        // SAFETY: tests in this module do not read these variables concurrently.
        unsafe { std::env::set_var(VIDEO_PLAYER_ENV, "myplayer --loop") };
        let candidates = launch_candidates(Path::new("/tmp/x.mp4"), "", OpenKind::Video);
        unsafe { std::env::remove_var(VIDEO_PLAYER_ENV) };
        assert_eq!(candidates[0].program, "myplayer");
        assert_eq!(candidates[0].args, ["--loop", "/tmp/x.mp4"]);
    }
}
