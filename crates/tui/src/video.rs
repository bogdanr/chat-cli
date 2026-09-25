//! Video poster-frame and metadata extraction.
//!
//! Providers usually ship only a tiny embedded JPEG thumbnail with a video, and
//! cache the real file under a content-hash name. Once the video bytes are on
//! disk we can do much better: `ffprobe` reports duration and dimensions and
//! `ffmpeg` grabs a sharp poster frame. Both are external processes, so this
//! module must only ever run on blocking worker threads — never from input
//! handling or draw paths. Results are persisted next to the video (a poster
//! JPEG plus a small JSON sidecar) so restarts do not re-probe.
//!
//! When ffmpeg/ffprobe are missing or fail, callers fall back to the embedded
//! provider thumbnail; failures are reported as `Err` so they can be cached and
//! are not retried on every frame.
//!
//! GIF-style media gets a short, downscaled frame sequence ([`Animation`]) so
//! cards can loop it inline: silent short clips (WhatsApp sends GIFs as silent
//! MP4s) are sampled with ffmpeg, and real `.gif` files are decoded with the
//! `image` crate so they animate even without ffmpeg.
//!
//! The binaries default to `ffmpeg`/`ffprobe` on `PATH`; set
//! `CHAT_CLI_FFMPEG` / `CHAT_CLI_FFPROBE` to use a specific build (for example
//! when the system package is broken).

use image::{AnimationDecoder, ImageDecoder, codecs::gif::GifDecoder, imageops::FilterType};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs::File,
    io::{BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// Upper bound for a single ffprobe/ffmpeg invocation. Poster extraction seeks
/// near the start of the file, so healthy runs finish well within this.
const PROCESS_TIMEOUT: Duration = Duration::from_secs(20);
/// Longest edge of the extracted poster frame, in pixels. Large enough for the
/// full-screen image viewer, small enough to decode quickly for inline cards.
const POSTER_MAX_EDGE: u32 = 960;
/// Bumped whenever [`VideoInfo`] gains fields so older sidecars are re-probed.
const SIDECAR_VERSION: u32 = 2;
/// Silent clips up to this long are presented as looping GIFs.
const GIF_LIKE_MAX_DURATION_MS: u64 = 30_000;
/// Frame budget for one inline animation. Frames are decoded into every cache
/// that displays them, so this bounds memory and background decode work.
const ANIMATION_MAX_FRAMES: usize = 60;
/// Longest edge of an animation frame. Inline cards are at most 48 cells wide,
/// so larger frames only cost memory.
const ANIMATION_MAX_EDGE: u32 = 320;
/// Sampling rate bounds for animations extracted from video.
const ANIMATION_MIN_FPS: f64 = 5.0;
const ANIMATION_MAX_FPS: f64 = 12.0;
/// Browsers treat GIF delays below 20 ms as "as fast as possible" and play
/// them at 100 ms; mirror that so such GIFs do not spin at the redraw cap.
const GIF_MIN_DELAY_MS: u32 = 20;
const GIF_DEFAULT_DELAY_MS: u32 = 100;

/// Metadata and poster frame derived from a local video file.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct VideoInfo {
    pub duration_ms: Option<u64>,
    /// Display width (already corrected for rotation metadata).
    pub width: Option<u32>,
    /// Display height (already corrected for rotation metadata).
    pub height: Option<u32>,
    /// Whether the container has an audio stream; `None` when unknown.
    #[serde(default)]
    pub has_audio: Option<bool>,
    /// Extracted poster frame, when ffmpeg produced one.
    pub poster: Option<PathBuf>,
    /// Looping frame sequence for GIF-style media.
    #[serde(default)]
    pub animation: Option<Animation>,
}

impl VideoInfo {
    /// GIF-style media: real `.gif` files and silent short clips, which is how
    /// WhatsApp (and most chat apps) transport GIFs.
    pub fn is_gif_like(&self) -> bool {
        self.has_audio == Some(false)
            && self
                .duration_ms
                .is_some_and(|duration| duration <= GIF_LIKE_MAX_DURATION_MS)
    }
}

/// A short looping frame sequence stored as image files on disk, so every
/// existing path-keyed preview cache (half-block rows, terminal protocols)
/// can decode and cache frames independently.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Animation {
    pub frames: Vec<PathBuf>,
    /// Display time of each frame, parallel to `frames`.
    pub delays_ms: Vec<u32>,
}

impl Animation {
    pub fn total_ms(&self) -> u64 {
        self.delays_ms.iter().map(|delay| u64::from(*delay)).sum()
    }

    /// Frame to show `elapsed_ms` into an endless loop.
    pub fn frame_at(&self, elapsed_ms: u64) -> usize {
        let total = self.total_ms();
        if total == 0 || self.frames.is_empty() {
            return 0;
        }
        let mut remaining = elapsed_ms % total;
        for (index, delay) in self.delays_ms.iter().enumerate() {
            let delay = u64::from(*delay);
            if remaining < delay {
                return index;
            }
            remaining -= delay;
        }
        self.frames.len() - 1
    }

    fn is_usable(&self) -> bool {
        self.frames.len() >= 2
            && self.frames.len() == self.delays_ms.len()
            && self.frames.iter().all(|frame| frame.exists())
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct Sidecar {
    version: u32,
    info: VideoInfo,
}

/// True for `.gif` files, which are decoded natively rather than via ffmpeg.
pub fn is_gif_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("gif"))
}

/// True when the path looks like a playable video container we can probe.
pub fn is_probeable_video(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["mp4", "m4v", "mov", "webm", "mkv", "avi", "3gp"]
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

/// Deterministic poster path stored next to the video file.
pub fn poster_path_for(video: &Path) -> PathBuf {
    sibling_with_suffix(video, "-poster.jpg")
}

fn sidecar_path_for(video: &Path) -> PathBuf {
    sibling_with_suffix(video, "-video.json")
}

fn frames_dir_for(video: &Path) -> PathBuf {
    sibling_with_suffix(video, "-frames")
}

/// True when `path` is one frame of an extracted inline animation, i.e. it
/// lives in a `<media>-frames` directory created by this module.
pub fn is_animation_frame(path: &Path) -> bool {
    path.parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with("-frames"))
}

/// Command for an FFmpeg tool, honouring the `CHAT_CLI_FFMPEG` /
/// `CHAT_CLI_FFPROBE` overrides.
fn tool_command(name: &str) -> Command {
    let variable = format!("CHAT_CLI_{}", name.to_ascii_uppercase());
    let program = std::env::var_os(&variable)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from(name));
    Command::new(program)
}

fn sibling_with_suffix(video: &Path, suffix: &str) -> PathBuf {
    let stem = video
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("video");
    video.with_file_name(format!("{stem}{suffix}"))
}

/// Returns cached metadata for `video`, probing it with ffprobe/ffmpeg when no
/// valid sidecar exists yet. Blocking: call from `spawn_blocking` only.
pub fn load_or_probe(video: &Path) -> Result<VideoInfo, String> {
    if let Some(info) = read_sidecar(video) {
        return Ok(info);
    }
    if !video.exists() {
        return Err(format!(
            "video {} is not available locally",
            video.display()
        ));
    }

    let info = if is_gif_file(video) {
        probe_gif(video)?
    } else {
        probe_video(video)?
    };
    write_sidecar(video, &info);
    Ok(info)
}

fn probe_video(video: &Path) -> Result<VideoInfo, String> {
    let mut info = probe_metadata(video)?;
    let poster = poster_path_for(video);
    info.poster = match extract_poster(video, &poster, info.duration_ms) {
        Ok(()) => Some(poster),
        // A missing poster is not fatal: the metadata alone still improves the
        // card, and the embedded provider thumbnail remains the preview.
        Err(_) => None,
    };
    if info.is_gif_like() {
        // Without frames the card still shows the poster, just not animated.
        info.animation = extract_video_frames(video, info.duration_ms).ok();
    }
    Ok(info)
}

/// Decodes a `.gif` natively: dimensions, total duration, and (for animated
/// files) a downscaled frame sequence whose first frame doubles as poster.
fn probe_gif(path: &Path) -> Result<VideoInfo, String> {
    let file = File::open(path).map_err(|error| format!("opening {}: {error}", path.display()))?;
    let decoder = GifDecoder::new(BufReader::new(file))
        .map_err(|error| format!("reading gif {}: {error}", path.display()))?;
    let (width, height) = decoder.dimensions();

    let dir = frames_dir_for(path);
    let staging = staging_dir_for(&dir)?;
    let mut frames = Vec::new();
    let mut delays_ms = Vec::new();
    for (index, frame) in decoder.into_frames().take(ANIMATION_MAX_FRAMES).enumerate() {
        let frame = frame.map_err(|error| format!("decoding gif frame {index}: {error}"))?;
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        let delay = numerator.checked_div(denominator).unwrap_or(0);
        delays_ms.push(if delay < GIF_MIN_DELAY_MS {
            GIF_DEFAULT_DELAY_MS
        } else {
            delay
        });
        let buffer = frame.into_buffer();
        let (frame_width, frame_height) = fit_within(buffer.width(), buffer.height());
        let resized = if (frame_width, frame_height) == buffer.dimensions() {
            buffer
        } else {
            image::imageops::resize(&buffer, frame_width, frame_height, FilterType::Triangle)
        };
        let name = format!("f{:03}.png", index + 1);
        resized
            .save(staging.join(&name))
            .map_err(|error| format!("saving gif frame {index}: {error}"))?;
        frames.push(dir.join(name));
    }
    let duration_ms = delays_ms.iter().map(|delay| u64::from(*delay)).sum::<u64>();

    let animated = frames.len() >= 2;
    publish_staging_dir(&staging, &dir)?;
    Ok(VideoInfo {
        duration_ms: animated.then_some(duration_ms),
        width: (width > 0).then_some(width),
        height: (height > 0).then_some(height),
        has_audio: Some(false),
        poster: frames.first().cloned(),
        animation: animated.then_some(Animation { frames, delays_ms }),
    })
}

/// Scales `(width, height)` down to fit [`ANIMATION_MAX_EDGE`], keeping the
/// aspect ratio.
fn fit_within(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= ANIMATION_MAX_EDGE || longest == 0 {
        return (width.max(1), height.max(1));
    }
    let scale = f64::from(ANIMATION_MAX_EDGE) / f64::from(longest);
    (
        ((f64::from(width) * scale).round() as u32).max(1),
        ((f64::from(height) * scale).round() as u32).max(1),
    )
}

/// Sampling rate for an animation extracted from a clip: spread the frame
/// budget over the whole loop, within sensible smoothness bounds.
fn animation_fps(duration_ms: Option<u64>) -> f64 {
    let seconds = duration_ms.map(|ms| ms as f64 / 1000.0).unwrap_or(4.0);
    if seconds <= 0.0 {
        return ANIMATION_MAX_FPS;
    }
    (ANIMATION_MAX_FRAMES as f64 / seconds).clamp(ANIMATION_MIN_FPS, ANIMATION_MAX_FPS)
}

fn extract_video_frames(video: &Path, duration_ms: Option<u64>) -> Result<Animation, String> {
    let fps = animation_fps(duration_ms);
    let max_seconds = ANIMATION_MAX_FRAMES as f64 / fps;
    let dir = frames_dir_for(video);
    let staging = staging_dir_for(&dir)?;
    let mut command = tool_command("ffmpeg");
    command
        .args(["-v", "error", "-nostdin", "-y", "-t"])
        .arg(format!("{max_seconds:.3}"))
        .arg("-i")
        .arg(video)
        .args([
            "-an",
            "-vf",
            &format!(
                "fps={fps:.3},scale='min({ANIMATION_MAX_EDGE},iw)':'min({ANIMATION_MAX_EDGE},ih)':force_original_aspect_ratio=decrease"
            ),
            "-frames:v",
            &ANIMATION_MAX_FRAMES.to_string(),
            "-q:v",
            "4",
            "-f",
            "image2",
        ])
        .arg(staging.join("f%03d.jpg"));
    if let Err(error) = run_with_timeout(command, "ffmpeg") {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }

    let mut names = std::fs::read_dir(&staging)
        .map_err(|error| format!("listing frames: {error}"))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name())
        .filter(|name| name.to_string_lossy().ends_with(".jpg"))
        .collect::<Vec<_>>();
    names.sort();
    if names.len() < 2 {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("ffmpeg produced too few animation frames".to_owned());
    }
    publish_staging_dir(&staging, &dir)?;
    let delay = (1000.0 / fps).round() as u32;
    Ok(Animation {
        delays_ms: vec![delay; names.len()],
        frames: names.into_iter().map(|name| dir.join(name)).collect(),
    })
}

/// Fresh staging directory next to `dir`, so a finished frame set appears
/// atomically and a crash never leaves a half-written sequence in place.
fn staging_dir_for(dir: &Path) -> Result<PathBuf, String> {
    let staging = dir.with_file_name(format!(
        "{}.part",
        dir.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "frames".to_owned())
    ));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .map_err(|error| format!("creating {}: {error}", staging.display()))?;
    Ok(staging)
}

fn publish_staging_dir(staging: &Path, dir: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::rename(staging, dir).map_err(|error| {
        let _ = std::fs::remove_dir_all(staging);
        format!("saving frames {}: {error}", dir.display())
    })
}

fn read_sidecar(video: &Path) -> Option<VideoInfo> {
    let bytes = std::fs::read(sidecar_path_for(video)).ok()?;
    let sidecar: Sidecar = serde_json::from_slice(&bytes).ok()?;
    if sidecar.version != SIDECAR_VERSION {
        return None;
    }
    // A sidecar pointing at a poster or frames that were cleaned up is stale.
    if sidecar
        .info
        .poster
        .as_ref()
        .is_some_and(|poster| !poster.exists())
        || sidecar
            .info
            .animation
            .as_ref()
            .is_some_and(|animation| !animation.is_usable())
    {
        return None;
    }
    Some(sidecar.info)
}

fn write_sidecar(video: &Path, info: &VideoInfo) {
    let sidecar = Sidecar {
        version: SIDECAR_VERSION,
        info: info.clone(),
    };
    if let Ok(bytes) = serde_json::to_vec(&sidecar) {
        let _ = std::fs::write(sidecar_path_for(video), bytes);
    }
}

fn probe_metadata(video: &Path) -> Result<VideoInfo, String> {
    let mut command = tool_command("ffprobe");
    command
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,width,height:stream_side_data=rotation:stream_tags=rotate:format=duration",
            "-of",
            "json",
        ])
        .arg(video);
    let output = run_with_timeout(command, "ffprobe")?;
    parse_ffprobe_json(&output)
}

/// Parses `ffprobe -of json` output into [`VideoInfo`] (without poster).
pub fn parse_ffprobe_json(raw: &[u8]) -> Result<VideoInfo, String> {
    let value: serde_json::Value =
        serde_json::from_slice(raw).map_err(|error| format!("parsing ffprobe output: {error}"))?;
    let streams = value
        .get("streams")
        .and_then(|streams| streams.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    let codec_type = |stream: &serde_json::Value| {
        stream
            .get("codec_type")
            .and_then(|kind| kind.as_str())
            .map(str::to_owned)
    };
    // Output without codec types (older callers) is treated as a single video
    // stream with unknown audio.
    let typed = streams.iter().any(|stream| codec_type(stream).is_some());
    let stream = if typed {
        streams
            .iter()
            .find(|stream| codec_type(stream).as_deref() == Some("video"))
    } else {
        streams.first()
    };
    let has_audio = typed.then(|| {
        streams
            .iter()
            .any(|stream| codec_type(stream).as_deref() == Some("audio"))
    });
    let dimension = |key: &str| {
        stream
            .and_then(|stream| stream.get(key))
            .and_then(|value| value.as_u64())
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)
    };
    let mut width = dimension("width");
    let mut height = dimension("height");

    let rotation = stream
        .and_then(|stream| stream.get("side_data_list"))
        .and_then(|list| list.as_array())
        .and_then(|list| {
            list.iter()
                .find_map(|entry| entry.get("rotation").and_then(json_number))
        })
        .or_else(|| {
            stream
                .and_then(|stream| stream.get("tags"))
                .and_then(|tags| tags.get("rotate"))
                .and_then(json_number)
        })
        .unwrap_or(0.0);
    let quarter_turns = ((rotation / 90.0).round() as i64).rem_euclid(4);
    if quarter_turns % 2 == 1 {
        std::mem::swap(&mut width, &mut height);
    }

    let duration_ms = value
        .get("format")
        .and_then(|format| format.get("duration"))
        .and_then(json_number)
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(|seconds| (seconds * 1000.0).round() as u64);

    if width.is_none() && height.is_none() && duration_ms.is_none() {
        return Err("ffprobe reported no video stream".to_owned());
    }
    Ok(VideoInfo {
        duration_ms,
        width,
        height,
        has_audio,
        poster: None,
        animation: None,
    })
}

/// ffprobe emits numbers either as JSON numbers or as strings.
fn json_number(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|raw| raw.trim().parse().ok()))
}

/// Picks the poster timestamp: skip the first second (often black or a fade)
/// when the clip is long enough, otherwise sample a quarter of the way in.
fn poster_seek_seconds(duration_ms: Option<u64>) -> f64 {
    match duration_ms {
        Some(ms) if ms >= 4_000 => 1.0,
        Some(ms) => ms as f64 / 4_000.0,
        None => 0.0,
    }
}

fn extract_poster(video: &Path, poster: &Path, duration_ms: Option<u64>) -> Result<(), String> {
    let seek = poster_seek_seconds(duration_ms);
    let attempt = |seek: f64| -> Result<(), String> {
        let partial = poster.with_extension("part.jpg");
        let mut command = tool_command("ffmpeg");
        command
            .args(["-v", "error", "-nostdin", "-y", "-ss"])
            .arg(format!("{seek:.3}"))
            .arg("-i")
            .arg(video)
            .args([
                "-frames:v",
                "1",
                "-vf",
                &format!(
                    "scale='min({POSTER_MAX_EDGE},iw)':'min({POSTER_MAX_EDGE},ih)':force_original_aspect_ratio=decrease"
                ),
                "-q:v",
                "3",
                "-f",
                "image2",
            ])
            .arg(&partial);
        let result = run_with_timeout(command, "ffmpeg").and_then(|_| {
            let written = std::fs::metadata(&partial)
                .map(|metadata| metadata.len() > 0)
                .unwrap_or(false);
            if written {
                std::fs::rename(&partial, poster)
                    .map_err(|error| format!("saving poster {}: {error}", poster.display()))
            } else {
                Err("ffmpeg produced no poster frame".to_owned())
            }
        });
        if result.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        result
    };
    // Seeking past the end of very short or oddly indexed clips yields no
    // frame; retry from the first frame before giving up.
    attempt(seek).or_else(|error| if seek > 0.0 { attempt(0.0) } else { Err(error) })
}

/// Runs `command`, returning stdout on success. Enforces [`PROCESS_TIMEOUT`]
/// so a wedged ffmpeg can never pin a worker thread indefinitely.
fn run_with_timeout(mut command: Command, name: &str) -> Result<Vec<u8>, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("{name} not found; install FFmpeg for video previews")
        } else {
            format!("starting {name}: {error}")
        }
    })?;

    // Drain pipes on helper threads so verbose output cannot fill the pipe
    // buffer and deadlock the child while we poll for exit.
    let stdout = spawn_pipe_reader(child.stdout.take());
    let stderr = spawn_pipe_reader(child.stderr.take());
    let status = wait_with_deadline(&mut child, name)?;
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();

    if status.success() {
        Ok(stdout)
    } else {
        let detail = String::from_utf8_lossy(&stderr);
        let detail = detail.lines().last().unwrap_or("").trim();
        Err(format!("{name} failed ({status}): {detail}"))
    }
}

fn spawn_pipe_reader<R: Read + Send + 'static>(
    pipe: Option<R>,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    })
}

fn wait_with_deadline(child: &mut Child, name: &str) -> Result<std::process::ExitStatus, String> {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{name} timed out after {PROCESS_TIMEOUT:?}"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(format!("waiting for {name}: {error}")),
        }
    }
}

/// Formats a duration as `m:ss` or `h:mm:ss`.
pub fn format_duration(duration_ms: u64) -> String {
    let total_seconds = (duration_ms + 500) / 1000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Whether a media file name carries information for a human, as opposed to a
/// provider cache key such as WhatsApp's 64-hex-digit media hash or a generic
/// placeholder name.
pub fn is_meaningful_file_name(file_name: &str) -> bool {
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(file_name)
        .trim();
    if stem.is_empty() {
        return false;
    }
    let generic = [
        "video",
        "image",
        "file",
        "whatsapp-video",
        "whatsapp-image",
        "whatsapp-file",
    ];
    if generic
        .iter()
        .any(|candidate| stem.eq_ignore_ascii_case(candidate))
    {
        return false;
    }
    let hexish = stem.len() >= 16
        && stem
            .chars()
            .all(|character| character.is_ascii_hexdigit() || character == '-');
    !hexish
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffprobe_output_with_rotation() {
        let raw = br#"{
            "streams": [{"width": 1920, "height": 1080,
                         "side_data_list": [{"rotation": -90}]}],
            "format": {"duration": "42.480000"}
        }"#;
        let info = parse_ffprobe_json(raw).expect("parse");
        assert_eq!(info.width, Some(1080));
        assert_eq!(info.height, Some(1920));
        assert_eq!(info.duration_ms, Some(42_480));
    }

    #[test]
    fn parses_legacy_rotate_tag_and_missing_duration() {
        let raw =
            br#"{"streams":[{"width":640,"height":360,"tags":{"rotate":"180"}}],"format":{}}"#;
        let info = parse_ffprobe_json(raw).expect("parse");
        assert_eq!((info.width, info.height), (Some(640), Some(360)));
        assert_eq!(info.duration_ms, None);
    }

    #[test]
    fn rejects_output_without_video_stream() {
        assert!(parse_ffprobe_json(br#"{"streams":[],"format":{}}"#).is_err());
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(42_480), "0:42");
        assert_eq!(format_duration(62_000), "1:02");
        assert_eq!(format_duration(3_723_000), "1:02:03");
    }

    #[test]
    fn detects_hash_and_generic_file_names() {
        assert!(!is_meaningful_file_name(
            "3f9a0c1b2d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8.mp4"
        ));
        assert!(!is_meaningful_file_name("whatsapp-video.mp4"));
        assert!(!is_meaningful_file_name("video.mov"));
        assert!(is_meaningful_file_name("holiday-2026.mp4"));
        assert!(is_meaningful_file_name("demo.webm"));
    }

    #[test]
    fn poster_seek_skips_first_second_only_for_longer_clips() {
        assert_eq!(poster_seek_seconds(None), 0.0);
        assert_eq!(poster_seek_seconds(Some(2_000)), 0.5);
        assert_eq!(poster_seek_seconds(Some(60_000)), 1.0);
    }

    #[test]
    fn derives_sibling_paths() {
        let video = Path::new("/cache/abc.mp4");
        assert_eq!(
            poster_path_for(video),
            PathBuf::from("/cache/abc-poster.jpg")
        );
        assert_eq!(
            sidecar_path_for(video),
            PathBuf::from("/cache/abc-video.json")
        );
    }

    #[test]
    fn probes_real_video_when_ffmpeg_is_available() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let video = dir.path().join("clip.mp4");
        let status = tool_command("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x180:rate=10:duration=2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-shortest",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&video)
            .status()
            .expect("run ffmpeg");
        if !status.success() {
            return;
        }

        let info = load_or_probe(&video).expect("probe");
        assert_eq!((info.width, info.height), (Some(320), Some(180)));
        assert!(
            info.duration_ms
                .is_some_and(|ms| (1_500..=2_500).contains(&ms))
        );
        assert_eq!(info.has_audio, Some(true));
        assert!(!info.is_gif_like());
        assert!(info.animation.is_none());
        let poster = info.poster.clone().expect("poster");
        assert!(poster.exists());
        assert!(sidecar_path_for(&video).exists());
        // Second call is served from the sidecar.
        assert_eq!(load_or_probe(&video).expect("cached"), info);
    }

    #[test]
    fn silent_short_clip_becomes_looping_animation() {
        if !ffmpeg_available() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let video = dir.path().join("gif.mp4");
        let status = tool_command("ffmpeg")
            .args([
                "-v",
                "error",
                "-nostdin",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=200x200:rate=25:duration=3",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&video)
            .status()
            .expect("run ffmpeg");
        if !status.success() {
            return;
        }

        let info = load_or_probe(&video).expect("probe");
        assert_eq!(info.has_audio, Some(false));
        assert!(info.is_gif_like());
        let animation = info.animation.clone().expect("animation");
        // 3 s at the 12 fps cap.
        assert!((30..=40).contains(&animation.frames.len()));
        assert!(animation.frames.iter().all(|frame| frame.exists()));
        assert_eq!(animation.frames.len(), animation.delays_ms.len());
        assert!(!frames_dir_for(&video).with_extension("part").exists());
        assert_eq!(load_or_probe(&video).expect("cached"), info);
    }

    #[test]
    fn animated_gif_is_decoded_without_ffmpeg() {
        use image::{Delay, Frame, Rgba, RgbaImage, codecs::gif::GifEncoder};
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("party.gif");
        {
            let file = File::create(&path).expect("create");
            let mut encoder = GifEncoder::new(file);
            let frames = [Rgba([255, 0, 0, 255]), Rgba([0, 0, 255, 255])]
                .into_iter()
                .map(|color| {
                    Frame::from_parts(
                        RgbaImage::from_pixel(640, 320, color),
                        0,
                        0,
                        Delay::from_numer_denom_ms(150, 1),
                    )
                });
            encoder.encode_frames(frames).expect("encode");
        }

        let info = load_or_probe(&path).expect("probe gif");
        assert_eq!((info.width, info.height), (Some(640), Some(320)));
        assert!(info.is_gif_like());
        let animation = info.animation.clone().expect("animation");
        assert_eq!(animation.frames.len(), 2);
        assert_eq!(animation.delays_ms, vec![150, 150]);
        let first = image::open(&animation.frames[0]).expect("frame");
        // Downscaled to the animation edge budget, aspect preserved.
        assert_eq!((first.width(), first.height()), (320, 160));
        assert_eq!(info.poster.as_ref(), animation.frames.first());
    }

    #[test]
    fn animation_frame_timing_loops() {
        let animation = Animation {
            frames: vec!["a".into(), "b".into(), "c".into()],
            delays_ms: vec![100, 200, 100],
        };
        assert_eq!(animation.total_ms(), 400);
        assert_eq!(animation.frame_at(0), 0);
        assert_eq!(animation.frame_at(99), 0);
        assert_eq!(animation.frame_at(100), 1);
        assert_eq!(animation.frame_at(299), 1);
        assert_eq!(animation.frame_at(300), 2);
        assert_eq!(animation.frame_at(400), 0);
        assert_eq!(animation.frame_at(1_150), 2);
        assert_eq!(animation.frame_at(1_250), 0);
    }

    #[test]
    fn recognizes_extracted_animation_frames() {
        let video = Path::new("/cache/media/abc.mp4");
        let frame = frames_dir_for(video).join("frame-001.png");
        assert!(is_animation_frame(&frame));
        assert!(!is_animation_frame(&poster_path_for(video)));
        assert!(!is_animation_frame(video));
    }

    #[test]
    fn gif_like_requires_silence_and_short_duration() {
        let silent = VideoInfo {
            duration_ms: Some(4_320),
            has_audio: Some(false),
            ..VideoInfo::default()
        };
        assert!(silent.is_gif_like());
        let voiced = VideoInfo {
            has_audio: Some(true),
            ..silent.clone()
        };
        assert!(!voiced.is_gif_like());
        let unknown = VideoInfo {
            has_audio: None,
            ..silent.clone()
        };
        assert!(!unknown.is_gif_like());
        let long = VideoInfo {
            duration_ms: Some(95_000),
            ..silent
        };
        assert!(!long.is_gif_like());
    }

    #[test]
    fn parses_audio_presence_from_typed_streams() {
        let raw = br#"{"streams":[{"codec_type":"video","width":200,"height":200},
                                   {"codec_type":"audio"}],"format":{"duration":"4.3"}}"#;
        assert_eq!(
            parse_ffprobe_json(raw).expect("parse").has_audio,
            Some(true)
        );
        let raw = br#"{"streams":[{"codec_type":"video","width":200,"height":200}],
                       "format":{"duration":"4.3"}}"#;
        let info = parse_ffprobe_json(raw).expect("parse");
        assert_eq!(info.has_audio, Some(false));
        assert!(info.is_gif_like());
    }

    #[test]
    fn animation_fps_spreads_frame_budget() {
        assert_eq!(animation_fps(Some(2_000)), ANIMATION_MAX_FPS);
        assert_eq!(animation_fps(Some(10_000)), 6.0);
        assert_eq!(animation_fps(Some(30_000)), ANIMATION_MIN_FPS);
    }

    fn ffmpeg_available() -> bool {
        tool_command("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}
