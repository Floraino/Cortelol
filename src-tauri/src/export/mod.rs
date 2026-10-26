//! Local MP4 and subtitle export from explicitly reviewed edit decisions.

pub mod cloud_framing;
pub mod framing;
pub mod jobs;

use crate::media::probe_media;
use serde::Deserialize;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const EXPORT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_EXPORT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipRange {
    pub start_ms: u64,
    pub end_ms: u64,
}

impl ClipRange {
    pub fn validate(self, source_duration_seconds: f64) -> Result<Self, String> {
        if !source_duration_seconds.is_finite() || source_duration_seconds <= 0.0 {
            return Err("The source duration is invalid.".into());
        }
        let duration_ms = source_duration_seconds * 1_000.0;
        if !duration_ms.is_finite() || duration_ms > u64::MAX as f64 {
            return Err("The source duration is outside the supported range.".into());
        }
        if self.end_ms <= self.start_ms {
            return Err("The clip end must be after its start.".into());
        }
        if self.end_ms as f64 > duration_ms + 0.001 {
            return Err("The clip extends beyond the recording.".into());
        }
        Ok(self)
    }

    pub fn duration_ms(self) -> u64 {
        self.end_ms - self.start_ms
    }
}

#[derive(Debug)]
pub struct ExportRequest {
    pub source_path: PathBuf,
    pub destination_path: PathBuf,
    pub range: ClipRange,
    pub captions: Vec<CaptionCue>,
    pub framing: Framing,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CaptionCue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Framing {
    pub mode: FramingMode,
    pub horizontal_percent: u8,
    #[serde(default)]
    pub keyframes: Vec<FramingKeyframe>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FramingKeyframe {
    pub timestamp_ms: u64,
    pub mode: FramingMode,
    pub horizontal_percent: u8,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FramingMode {
    Crop,
    Fit,
}

#[derive(Debug, Clone)]
struct RelativeCue {
    start_ms: u64,
    end_ms: u64,
    text: String,
}

/// Render a reviewed clip. `cancelled` may be set from another thread; the
/// in-progress FFmpeg process is then killed and its temporary file removed.
pub fn render_clip(request: ExportRequest, cancelled: &AtomicBool) -> Result<PathBuf, String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("Export was cancelled.".into());
    }
    let source = request
        .source_path
        .canonicalize()
        .map_err(|_| "The source recording could not be found.".to_string())?;
    if !source.is_file() {
        return Err("The source recording must be a file.".into());
    }
    let source_str = source
        .to_str()
        .ok_or_else(|| "The source path is not supported.".to_string())?;
    let media = probe_media(source_str.to_owned())?;
    let range = request.range.validate(media.duration_seconds)?;
    if request.framing.horizontal_percent > 100 {
        return Err("The horizontal framing position must be between 0 and 100.".into());
    }
    validate_framing(&request.framing, range)?;
    let destination = checked_destination(&request.destination_path, &source)?;
    let captions = relative_captions(&request.captions, range)?;
    if captions.is_empty() {
        return Err("Add at least one reviewed caption before export.".into());
    }
    let srt_destination = destination.with_extension("srt");
    if fs::symlink_metadata(&srt_destination).is_ok() {
        return Err("A subtitle file with this name already exists.".into());
    }
    let (ass_path, mut ass_file) = reserve_temp_file(&destination, "ass")?;
    use std::io::Write;
    ass_file
        .write_all(ass_document(&captions).as_bytes())
        .and_then(|_| ass_file.sync_all())
        .map_err(|_| "The temporary captions could not be written.".to_string())?;
    drop(ass_file);
    let ass_temp = TempFile(ass_path);
    let (srt_path, mut srt_file) = reserve_temp_file(&destination, "srt")?;
    srt_file
        .write_all(srt_document(&captions).as_bytes())
        .and_then(|_| srt_file.sync_all())
        .map_err(|_| "The temporary subtitle file could not be written.".to_string())?;
    drop(srt_file);
    let srt_temp = TempFile(srt_path);
    let (temp_path, temp_file) = reserve_temp_file(&destination, "mp4")?;
    drop(temp_file);
    let temp = TempFile(temp_path);

    let mut child = ffmpeg_command(
        &source,
        &temp.0,
        range,
        media.has_video,
        request.framing,
        &ass_temp.0,
    )?
    .spawn()
    .map_err(|_| "FFmpeg is unavailable. Install FFmpeg and add it to PATH.".to_string())?;
    let started = Instant::now();
    let status = loop {
        if cancelled.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Export was cancelled.".into());
        }
        if started.elapsed() > EXPORT_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Export timed out after ten minutes. Try a shorter cut.".into());
        }
        if fs::metadata(&temp.0).is_ok_and(|metadata| metadata.len() > MAX_EXPORT_BYTES) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Export exceeded the 2 GB size limit. Try a shorter cut.".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("The FFmpeg export process could not be monitored.".into());
            }
        }
    };
    if !status.success() {
        return Err(
            "FFmpeg could not render this clip. Check the recording and export settings.".into(),
        );
    }
    let file = OpenOptions::new()
        .write(true)
        .open(&temp.0)
        .map_err(|_| "The rendered clip could not be opened.".to_string())?;
    if file
        .metadata()
        .map_err(|_| "The rendered clip could not be checked.".to_string())?
        .len()
        == 0
    {
        return Err("FFmpeg produced an empty clip.".into());
    }
    file.sync_all()
        .map_err(|_| "The rendered clip could not be saved.".to_string())?;
    if cancelled.load(Ordering::Relaxed) {
        return Err("Export was cancelled.".into());
    }

    // A hard link within the same directory publishes the completed file in
    // one filesystem operation and fails if an export already has this name.
    fs::hard_link(&srt_temp.0, &srt_destination)
        .map_err(|_| "The subtitle file could not be saved. Choose a new name.".to_string())?;
    if fs::hard_link(&temp.0, &destination).is_err() {
        let _ = fs::remove_file(&srt_destination);
        return Err("The export could not be saved. Choose a new file name and try again.".into());
    }
    Ok(destination)
}

fn checked_destination(requested: &Path, source: &Path) -> Result<PathBuf, String> {
    if !requested.is_absolute() {
        return Err("Choose an absolute export path.".into());
    }
    if !requested
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"))
    {
        return Err("The export file must use the .mp4 extension.".into());
    }
    let name = requested
        .file_name()
        .ok_or_else(|| "Choose a file name for the export.".to_string())?;
    let directory = requested
        .parent()
        .ok_or_else(|| "Choose an export directory.".to_string())?
        .canonicalize()
        .map_err(|_| "The export directory could not be found.".to_string())?;
    if !directory.is_dir() {
        return Err("The export directory is invalid.".into());
    }
    let destination = directory.join(name);
    if destination == source {
        return Err("The source recording cannot be used as the export file.".into());
    }
    if fs::symlink_metadata(&destination).is_ok() {
        return Err("An export with this file name already exists.".into());
    }
    Ok(destination)
}

fn reserve_temp_file(destination: &Path, extension: &str) -> Result<(PathBuf, File), String> {
    let directory = destination
        .parent()
        .ok_or_else(|| "The export directory is invalid.".to_string())?;
    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = format!(".cortelol-{}-{sequence}.{extension}", std::process::id());
        let path = directory.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err("A temporary export file could not be created.".into()),
        }
    }
    Err("A unique temporary export file could not be created.".into())
}

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn ffmpeg_command(
    source: &Path,
    output: &Path,
    range: ClipRange,
    has_video: bool,
    framing: Framing,
    ass_path: &Path,
) -> Result<Command, String> {
    let mut command = Command::new(crate::media_tools::ffmpeg()?);
    crate::media_tools::hide_console(&mut command);
    command.current_dir(output.parent().expect("export output has a parent"));
    command.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"]);
    if has_video {
        // Seeking the output drops frames after the subtitle filter has run.
        // Seek the input so filter timestamps start at zero for clip-relative cues.
        command.args(["-ss", &seconds(range.start_ms)]);
    }
    command.arg("-i").arg(source);
    let ass_name = ass_path
        .file_name()
        .expect("temporary caption has a filename")
        .to_string_lossy();
    // The generated ASCII filename is relative to the export directory. It
    // contains no FFmpeg filter metacharacters or user-controlled path text.
    let framing_filter = match framing.mode {
        FramingMode::Crop => format!(
            "scale=1080:1920:force_original_aspect_ratio=increase,crop=1080:1920:(in_w-out_w)*{}/100:(in_h-out_h)/2,setsar=1",
            framing.horizontal_percent
        ),
        FramingMode::Fit => "scale=1080:1920:force_original_aspect_ratio=decrease,pad=1080:1920:(ow-iw)/2:(oh-ih)/2:color=0x11151B,setsar=1".to_string(),
    };
    let filter = format!("{framing_filter},ass=filename={ass_name}");
    if has_video {
        if framing.keyframes.is_empty() {
            command.args(["-map", "0:v:0", "-map", "0:a:0", "-vf", &filter]);
        } else {
            let graph = dynamic_framing_filter(&framing.keyframes, range, &ass_name);
            command.args(["-filter_complex", &graph, "-map", "[vout]", "-map", "0:a:0"]);
        }
    } else {
        command.args(["-f", "lavfi", "-i", "color=c=0x11151B:s=1080x1920:r=30"]);
        command.args(["-map", "1:v:0", "-map", "0:a:0", "-vf", &filter]);
        // Output seeking would discard generated background frames for the
        // entire pre-roll. Trim decoded audio at sample precision instead.
        command.args([
            "-af",
            &format!(
                "atrim=start={}:duration={},asetpts=PTS-STARTPTS",
                seconds(range.start_ms),
                seconds(range.duration_ms())
            ),
        ]);
    }
    command
        .args(["-t", &seconds(range.duration_ms())])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
        .args(["-movflags", "+faststart", "-f", "mp4"])
        .arg(output)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

fn validate_framing(framing: &Framing, range: ClipRange) -> Result<(), String> {
    let frames = &framing.keyframes;
    if frames.len() > 150
        || (!frames.is_empty() && frames[0].timestamp_ms != range.start_ms)
        || frames.iter().enumerate().any(|(index, frame)| {
            frame.timestamp_ms < range.start_ms
                || frame.timestamp_ms >= range.end_ms
                || frame.horizontal_percent > 100
                || (index > 0 && frame.timestamp_ms <= frames[index - 1].timestamp_ms)
        })
    {
        return Err("The framing keyframes must be ordered within the reviewed clip.".into());
    }
    Ok(())
}

fn dynamic_framing_filter(frames: &[FramingKeyframe], range: ClipRange, ass_name: &str) -> String {
    // Crop position is evaluated for every decoded frame. Fit intervals show
    // the padded base instead, preserving all speakers in wide shots.
    let mut position = frames[0].horizontal_percent.to_string();
    for frame in frames.iter().skip(1) {
        let at = seconds(frame.timestamp_ms - range.start_ms);
        position = format!(
            "if(gte(t\\,{at})\\,{}\\,{position})",
            frame.horizontal_percent
        );
    }
    let crop_enabled = frames
        .iter()
        .enumerate()
        .filter_map(|(index, frame)| {
            if frame.mode != FramingMode::Crop {
                return None;
            }
            let start = seconds(frame.timestamp_ms - range.start_ms);
            let end = frames.get(index + 1).map_or(range.duration_ms(), |next| {
                next.timestamp_ms - range.start_ms
            });
            Some(format!("gte(t\\,{start})*lt(t\\,{})", seconds(end)))
        })
        .collect::<Vec<_>>()
        .join("+");
    let crop_enabled = if crop_enabled.is_empty() {
        "0"
    } else {
        &crop_enabled
    };
    format!(
        "[0:v]setpts=PTS-STARTPTS,split=2[fitin][cropin];\
         [fitin]scale=1080:1920:force_original_aspect_ratio=decrease,pad=1080:1920:(ow-iw)/2:(oh-ih)/2:color=0x11151B,setsar=1[base];\
         [cropin]scale=1080:1920:force_original_aspect_ratio=increase,crop=1080:1920:(in_w-out_w)*({position})/100:(in_h-out_h)/2,setsar=1[cropped];\
         [base][cropped]overlay=0:0:enable='{crop_enabled}',ass=filename={ass_name}[vout]"
    )
}

fn seconds(milliseconds: u64) -> String {
    format!("{}.{:03}", milliseconds / 1_000, milliseconds % 1_000)
}

fn relative_captions(
    captions: &[CaptionCue],
    range: ClipRange,
) -> Result<Vec<RelativeCue>, String> {
    if captions.len() > 500 {
        return Err("This clip has too many caption cues.".into());
    }
    let mut relative = Vec::new();
    for cue in captions {
        if cue.end_ms <= cue.start_ms {
            return Err("A caption ends before it starts.".into());
        }
        let text = cue.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() || text.chars().count() > 240 {
            return Err("Each caption needs 1 to 240 characters.".into());
        }
        let start = cue.start_ms.max(range.start_ms);
        let end = cue.end_ms.min(range.end_ms);
        if start < end {
            relative.push(RelativeCue {
                start_ms: start - range.start_ms,
                end_ms: end - range.start_ms,
                text,
            });
        }
    }
    relative.sort_by_key(|cue| (cue.start_ms, cue.end_ms));
    Ok(relative)
}

fn srt_document(captions: &[RelativeCue]) -> String {
    let mut output = String::new();
    for (index, cue) in captions.iter().enumerate() {
        output.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            index + 1,
            srt_timestamp(cue.start_ms),
            srt_timestamp(cue.end_ms),
            cue.text
        ));
    }
    output
}

fn srt_timestamp(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60,
        milliseconds % 1000
    )
}

fn ass_document(captions: &[RelativeCue]) -> String {
    let mut output = String::from(
        "[Script Info]\nScriptType: v4.00+\nPlayResX: 1080\nPlayResY: 1920\nWrapStyle: 0\nScaledBorderAndShadow: yes\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Inter,76,&H00F5EEE9,&H00F5EEE9,&H001B1511,&H001B1511,-1,0,0,0,100,100,0,0,1,5,2,2,80,80,270,1\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
    );
    for cue in captions {
        // ASS braces introduce override tags and backslash controls newlines.
        // Replace only those control glyphs in untrusted caption text.
        let text = cue
            .text
            .replace('{', "｛")
            .replace('}', "｝")
            .replace('\\', "＼");
        output.push_str(&format!(
            "Dialogue: 0,{},{},Default,,0,0,0,,{}\n",
            ass_timestamp(cue.start_ms),
            ass_timestamp(cue.end_ms),
            text
        ));
    }
    output
}

fn ass_timestamp(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    format!(
        "{}:{:02}:{:02}.{:02}",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60,
        milliseconds % 1000 / 10
    )
}

#[cfg(test)]
mod tests {
    use super::{
        CaptionCue, ClipRange, ExportRequest, Framing, FramingKeyframe, FramingMode, ass_document,
        checked_destination, ffmpeg_command, relative_captions, render_clip, seconds, srt_document,
    };
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn range_uses_exact_millisecond_arithmetic() {
        let range = ClipRange {
            start_ms: 59_950,
            end_ms: 60_050,
        };
        assert_eq!(range.validate(60.05).unwrap().duration_ms(), 100);
        assert_eq!(seconds(range.start_ms), "59.950");
        assert_eq!(seconds(range.duration_ms()), "0.100");
    }

    #[test]
    fn range_rejects_reversed_and_out_of_source_bounds() {
        assert!(
            ClipRange {
                start_ms: 1_000,
                end_ms: 1_000
            }
            .validate(2.0)
            .is_err()
        );
        assert!(
            ClipRange {
                start_ms: 1_001,
                end_ms: 1_000
            }
            .validate(2.0)
            .is_err()
        );
        assert!(
            ClipRange {
                start_ms: 0,
                end_ms: 2_001
            }
            .validate(2.0)
            .is_err()
        );
        assert!(
            ClipRange {
                start_ms: 0,
                end_ms: 1
            }
            .validate(f64::NAN)
            .is_err()
        );
    }

    #[test]
    fn ffmpeg_uses_individual_arguments_for_untrusted_paths() {
        let command = ffmpeg_command(
            Path::new("C:/episode; delete all.mp4"),
            Path::new("C:/export.mp4"),
            ClipRange {
                start_ms: 100,
                end_ms: 2_100,
            },
            true,
            Framing {
                mode: FramingMode::Crop,
                horizontal_percent: 75,
                keyframes: vec![],
            },
            Path::new("C:/safe.ass"),
        )
        .unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert!(args.iter().any(|arg| *arg == "C:/episode; delete all.mp4"));
        assert!(args.iter().any(|arg| *arg == "0.100"));
        assert!(args.iter().any(|arg| *arg == "2.000"));
        let source_position = args
            .iter()
            .position(|arg| *arg == "C:/episode; delete all.mp4")
            .unwrap();
        let seek_position = args.iter().position(|arg| *arg == "-ss").unwrap();
        assert!(seek_position < source_position);
    }

    #[test]
    fn audio_only_trims_after_inputs_at_sample_precision() {
        let command = ffmpeg_command(
            Path::new("C:/episode.mp3"),
            Path::new("C:/export.mp4"),
            ClipRange {
                start_ms: 1_250,
                end_ms: 2_000,
            },
            false,
            Framing {
                mode: FramingMode::Fit,
                horizontal_percent: 50,
                keyframes: vec![],
            },
            Path::new("C:/safe.ass"),
        )
        .unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert!(!args.iter().any(|arg| *arg == "-ss"));
        let audio_input_position = args
            .iter()
            .position(|arg| *arg == "C:/episode.mp3")
            .unwrap();
        let trim_position = args.iter().position(|arg| *arg == "-af").unwrap();
        assert!(audio_input_position < trim_position);
        assert_eq!(
            args[trim_position + 1],
            "atrim=start=1.250:duration=0.750,asetpts=PTS-STARTPTS"
        );
    }

    #[test]
    fn existing_export_is_never_selected_for_render() {
        let directory =
            std::env::temp_dir().join(format!("cortelol-export-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let source = directory.join("source.mp4");
        let destination = directory.join("clip.mp4");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(&destination, b"existing export").unwrap();
        assert!(checked_destination(&destination, &source).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"existing export");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(destination);
        let _ = std::fs::remove_dir(directory);
    }

    #[test]
    fn caption_offsets_clip_at_reviewed_boundaries() {
        let cues = relative_captions(
            &[
                CaptionCue {
                    start_ms: 900,
                    end_ms: 1_300,
                    text: "  first   line ".into(),
                },
                CaptionCue {
                    start_ms: 1_400,
                    end_ms: 2_100,
                    text: "second line".into(),
                },
            ],
            ClipRange {
                start_ms: 1_000,
                end_ms: 2_000,
            },
        )
        .unwrap();
        assert_eq!(
            srt_document(&cues),
            "1\n00:00:00,000 --> 00:00:00,300\nfirst line\n\n2\n00:00:00,400 --> 00:00:01,000\nsecond line\n\n"
        );
    }

    #[test]
    fn ass_text_cannot_inject_override_tags() {
        let cues = relative_captions(
            &[CaptionCue {
                start_ms: 0,
                end_ms: 1_000,
                text: "{\\N} safe".into(),
            }],
            ClipRange {
                start_ms: 0,
                end_ms: 1_000,
            },
        )
        .unwrap();
        let ass = ass_document(&cues);
        assert!(ass.contains("｛＼N｝ safe"));
        assert!(!ass.contains("{\\N}"));
    }

    #[test]
    fn ffmpeg_renders_captioned_audio_clip_and_matching_srt() {
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_err()
        {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("voice.wav");
        let output = directory.path().join("short.mp4");
        assert!(
            std::process::Command::new("ffmpeg")
                .args([
                    "-nostdin",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=1",
                    "-c:a",
                    "pcm_s16le",
                    "-y"
                ])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let rendered = render_clip(
            ExportRequest {
                source_path: source,
                destination_path: output.clone(),
                range: ClipRange {
                    start_ms: 100,
                    end_ms: 800,
                },
                captions: vec![CaptionCue {
                    start_ms: 50,
                    end_ms: 900,
                    text: "Reviewed words".into(),
                }],
                framing: Framing {
                    mode: FramingMode::Fit,
                    horizontal_percent: 50,
                    keyframes: vec![],
                },
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(rendered.file_name(), output.file_name());
        assert!(std::fs::metadata(&output).unwrap().len() > 1_000);
        assert_eq!(
            std::fs::read_to_string(output.with_extension("srt")).unwrap(),
            "1\n00:00:00,000 --> 00:00:00,700\nReviewed words\n\n"
        );
    }

    #[test]
    fn ffmpeg_renders_captioned_video_with_vertical_crop() {
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_err()
        {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("camera.mp4");
        let output = directory.path().join("vertical.mp4");
        assert!(
            std::process::Command::new("ffmpeg")
                .args([
                    "-nostdin",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=black:s=320x180:r=24:d=3",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=3",
                    "-c:v",
                    "libx264",
                    "-c:a",
                    "aac",
                    "-shortest",
                    "-y"
                ])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        render_clip(
            ExportRequest {
                source_path: source,
                destination_path: output.clone(),
                range: ClipRange {
                    start_ms: 1_200,
                    end_ms: 1_900,
                },
                captions: vec![CaptionCue {
                    start_ms: 1_200,
                    end_ms: 1_900,
                    text: "Vertical".into(),
                }],
                framing: Framing {
                    mode: FramingMode::Crop,
                    horizontal_percent: 80,
                    keyframes: vec![],
                },
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        let probed = crate::media::probe_media(output.to_string_lossy().into_owned()).unwrap();
        assert_eq!((probed.width, probed.height), (Some(1080), Some(1920)));
        assert!(probed.duration_seconds > 0.6 && probed.duration_seconds < 0.8);
        // A black source makes it possible to verify that reviewed captions
        // are actually burned into the MP4 after seeking into the recording.
        let frame = std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-ss",
                "0.350",
                "-i",
            ])
            .arg(&output)
            .args([
                "-frames:v",
                "1",
                "-vf",
                "format=gray",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .output()
            .unwrap();
        assert!(frame.status.success());
        assert_eq!(frame.stdout.len(), 1080 * 1920);
        assert!(frame.stdout.iter().filter(|&&pixel| pixel > 180).count() > 100);
    }

    #[test]
    fn ffmpeg_applies_timed_crop_and_fit_to_exported_frames() {
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_err()
        {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("wide.mp4");
        let output = directory.path().join("framed.mp4");
        assert!(std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
                "color=c=red:s=320x180:r=12:d=2,drawbox=x=160:y=0:w=160:h=180:color=blue:t=fill",
                "-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-c:v", "libx264",
                "-c:a", "aac", "-shortest", "-y"])
            .arg(&source).status().unwrap().success());
        render_clip(
            ExportRequest {
                source_path: source,
                destination_path: output.clone(),
                range: ClipRange {
                    start_ms: 0,
                    end_ms: 1_500,
                },
                captions: vec![CaptionCue {
                    start_ms: 0,
                    end_ms: 1_500,
                    text: "Speakers".into(),
                }],
                framing: Framing {
                    mode: FramingMode::Crop,
                    horizontal_percent: 0,
                    keyframes: vec![
                        FramingKeyframe {
                            timestamp_ms: 0,
                            mode: FramingMode::Crop,
                            horizontal_percent: 0,
                        },
                        FramingKeyframe {
                            timestamp_ms: 500,
                            mode: FramingMode::Fit,
                            horizontal_percent: 50,
                        },
                        FramingKeyframe {
                            timestamp_ms: 1_000,
                            mode: FramingMode::Crop,
                            horizontal_percent: 100,
                        },
                    ],
                },
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        let pixel = |at: &str, y: usize| -> Vec<u8> {
            let frame = std::process::Command::new("ffmpeg")
                .args([
                    "-nostdin",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-ss",
                    at,
                    "-i",
                ])
                .arg(&output)
                .args([
                    "-frames:v",
                    "1",
                    "-vf",
                    &format!("crop=2:2:540:{y},format=rgb24"),
                    "-f",
                    "rawvideo",
                    "pipe:1",
                ])
                .output()
                .unwrap();
            assert!(frame.status.success());
            frame.stdout
        };
        let left = pixel("0.250", 900);
        let wide = pixel("0.750", 100);
        let right = pixel("1.250", 900);
        assert!(
            left[0] > left[2] * 2,
            "left crop should show the red speaker"
        );
        assert!(
            wide[0] < 50 && wide[2] < 50,
            "wide shot should show padding"
        );
        assert!(
            right[2] > right[0] * 2,
            "right crop should show the blue speaker"
        );
    }
}
