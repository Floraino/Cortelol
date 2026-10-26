use serde::Serialize;
use serde_json::Value;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const MAX_PROBE_OUTPUT_BYTES: u64 = 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    pub path: String,
    pub file_name: String,
    pub duration_seconds: f64,
    pub file_size_bytes: u64,
    pub has_video: bool,
    pub has_audio: bool,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub audio_channels: Option<u64>,
}

fn parse_probe_output(
    path: &Path,
    file_size_bytes: u64,
    output: &[u8],
) -> Result<MediaInfo, String> {
    let probe: Value = serde_json::from_slice(output)
        .map_err(|_| "FFprobe returned invalid media metadata.".to_string())?;

    let duration_seconds = probe["format"]["duration"]
        .as_str()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| "The recording has no valid duration.".to_string())?;

    let streams = probe["streams"]
        .as_array()
        .ok_or_else(|| "The recording has no readable audio or video streams.".to_string())?;
    let video = streams
        .iter()
        .find(|stream| stream["codec_type"] == "video");
    let audio = streams
        .iter()
        .find(|stream| stream["codec_type"] == "audio");
    if audio.is_none() {
        return Err("The recording must contain an audio track for transcription.".to_string());
    }
    let (width, height) = match video {
        Some(stream) => {
            let width = stream["width"].as_u64().filter(|value| *value > 0);
            let height = stream["height"].as_u64().filter(|value| *value > 0);
            match (width, height) {
                (Some(width), Some(height)) => (Some(width), Some(height)),
                _ => return Err("The recording has invalid video dimensions.".to_string()),
            }
        }
        None => (None, None),
    };

    Ok(MediaInfo {
        path: path.to_string_lossy().into_owned(),
        file_name: path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled recording".to_string()),
        duration_seconds,
        file_size_bytes,
        has_video: video.is_some(),
        has_audio: audio.is_some(),
        width,
        height,
        audio_channels: audio.and_then(|stream| stream["channels"].as_u64()),
    })
}

pub fn probe_media(path: String) -> Result<MediaInfo, String> {
    let source = Path::new(&path)
        .canonicalize()
        .map_err(|_| "The selected file could not be found.".to_string())?;
    if !source.is_file() {
        return Err("Select a media file, not a folder.".to_string());
    }
    let file_size_bytes = source
        .metadata()
        .map_err(|_| "The selected file could not be read.".to_string())?
        .len();
    if file_size_bytes == 0 {
        return Err("The selected file is empty.".to_string());
    }

    let mut command = Command::new(crate::media_tools::ffprobe()?);
    crate::media_tools::hide_console(&mut command);
    let mut child = command
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration:stream=codec_type,width,height,channels",
            "-of",
            "json",
        ])
        .arg(&source)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "FFprobe is unavailable. Install FFmpeg and add it to PATH.".to_string())?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "FFprobe output could not be read.".to_string())?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout
            .by_ref()
            .take(MAX_PROBE_OUTPUT_BYTES + 1)
            .read_to_end(&mut output)
            .map(|_| output)
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < PROBE_TIMEOUT => {
                thread::sleep(Duration::from_millis(50))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("FFprobe timed out while reading the recording.".to_string());
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("FFprobe could not be monitored.".to_string());
            }
        }
    };
    let output = reader
        .join()
        .map_err(|_| "FFprobe output could not be read.".to_string())?
        .map_err(|_| "FFprobe output could not be read.".to_string())?;
    if output.len() as u64 > MAX_PROBE_OUTPUT_BYTES {
        return Err("The recording contains too much stream metadata.".to_string());
    }
    if !status.success() {
        return Err("This file could not be read as audio or video.".to_string());
    }
    parse_probe_output(&source, file_size_bytes, &output)
}

#[cfg(test)]
mod tests {
    use super::parse_probe_output;
    use std::path::Path;

    #[test]
    fn parses_audio_video_metadata() {
        let json = br#"{"format":{"duration":"62.5"},"streams":[{"codec_type":"video","width":1920,"height":1080},{"codec_type":"audio","channels":2}]}"#;
        let media = parse_probe_output(Path::new("episode.mp4"), 1000, json).unwrap();
        assert_eq!(media.duration_seconds, 62.5);
        assert_eq!(media.width, Some(1920));
        assert_eq!(media.audio_channels, Some(2));
        assert!(media.has_video && media.has_audio);
    }

    #[test]
    fn rejects_invalid_duration() {
        let json = br#"{"format":{"duration":"NaN"},"streams":[{"codec_type":"audio"}]}"#;
        assert!(parse_probe_output(Path::new("episode.mp3"), 1000, json).is_err());
    }

    #[test]
    fn rejects_video_without_audio() {
        let json = br#"{"format":{"duration":"18.0"},"streams":[{"codec_type":"video","width":1920,"height":1080}]}"#;
        assert!(parse_probe_output(Path::new("silent.mp4"), 1000, json).is_err());
    }

    #[test]
    fn rejects_video_with_invalid_dimensions() {
        let json = br#"{"format":{"duration":"18.0"},"streams":[{"codec_type":"video","width":0,"height":1080},{"codec_type":"audio","channels":2}]}"#;
        assert!(parse_probe_output(Path::new("broken.mp4"), 1000, json).is_err());
    }
}
