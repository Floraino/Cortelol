//! Temporary, bounded audio extraction from an immutable recording.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tempfile::{Builder, TempDir};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::AudioChunk;

/// The temporary directory deletes the extracted media when this value drops.
pub struct ExtractedAudioChunk {
    _directory: TempDir,
    pub path: PathBuf,
    pub mime_type: &'static str,
    pub chunk: AudioChunk,
    /// Duration of the PCM samples actually sent to Gemini, rounded up to ms.
    pub actual_duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractionError {
    InvalidSource,
    InvalidChunk,
    StagingUnavailable,
    FfmpegUnavailable,
    FfmpegFailed,
    TimedOut,
    EmptyOutput,
    InvalidOutput,
    Cancelled,
}

/// Extract one source-aligned WAV chunk. Callers may process one chunk at a
/// time to bound disk use and retry only incomplete chunks after a restart.
/// `staging_dir` must be a private application cache directory, never a source
/// or export directory. FFmpeg receives arguments directly without a shell.
pub async fn extract_wav_chunk(
    source: &Path,
    staging_dir: &Path,
    chunk: AudioChunk,
    cancellation: &CancellationToken,
) -> Result<ExtractedAudioChunk, ExtractionError> {
    if cancellation.is_cancelled() {
        return Err(ExtractionError::Cancelled);
    }
    let source = source
        .canonicalize()
        .map_err(|_| ExtractionError::InvalidSource)?;
    if !source.is_file() {
        return Err(ExtractionError::InvalidSource);
    }
    if chunk.end_ms <= chunk.start_ms || chunk.end_ms - chunk.start_ms > 30 * 60 * 1000 {
        return Err(ExtractionError::InvalidChunk);
    }
    std::fs::create_dir_all(staging_dir).map_err(|_| ExtractionError::StagingUnavailable)?;
    let directory = Builder::new()
        .prefix("transcription-")
        .tempdir_in(staging_dir)
        .map_err(|_| ExtractionError::StagingUnavailable)?;
    let path = directory.path().join("audio.wav");

    let start = format_seconds(chunk.start_ms);
    let duration = format_seconds(chunk.end_ms - chunk.start_ms);
    let mut command =
        Command::new(crate::media_tools::ffmpeg().map_err(|_| ExtractionError::FfmpegUnavailable)?);
    crate::media_tools::hide_console_async(&mut command);
    command
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(&source)
        .args([
            "-ss",
            &start,
            "-t",
            &duration,
            "-map",
            "0:a:0",
            "-vn",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-c:a",
            "pcm_s16le",
            "-f",
            "wav",
            "-n",
        ])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|_| ExtractionError::FfmpegUnavailable)?;
    let status = tokio::select! {
        _ = cancellation.cancelled() => {
            let _ = child.kill().await;
            return Err(ExtractionError::Cancelled);
        }
        _ = tokio::time::sleep(Duration::from_secs(30 * 60)) => {
            let _ = child.kill().await;
            return Err(ExtractionError::TimedOut);
        }
        status = child.wait() => status.map_err(|_| ExtractionError::FfmpegFailed)?,
    };
    if !status.success() {
        return Err(ExtractionError::FfmpegFailed);
    }
    let actual_duration_ms = wav_duration_ms(&path)?;
    // Sample boundaries and millisecond chunk boundaries may differ by one ms.
    // A larger excess means extraction did not respect the requested interval.
    if actual_duration_ms > chunk.end_ms - chunk.start_ms + 1 {
        return Err(ExtractionError::InvalidOutput);
    }
    Ok(ExtractedAudioChunk {
        _directory: directory,
        path,
        mime_type: "audio/wav",
        chunk,
        actual_duration_ms,
    })
}

/// Read the produced PCM sample count rather than trusting the source container
/// duration. A video stream can outlast its audio stream by many seconds.
fn wav_duration_ms(path: &Path) -> Result<u64, ExtractionError> {
    let mut file = std::fs::File::open(path).map_err(|_| ExtractionError::InvalidOutput)?;
    let file_len = file
        .metadata()
        .map_err(|_| ExtractionError::InvalidOutput)?
        .len();
    if file_len < 12 {
        return Err(ExtractionError::EmptyOutput);
    }
    let mut header = [0u8; 12];
    file.read_exact(&mut header)
        .map_err(|_| ExtractionError::InvalidOutput)?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
        return Err(ExtractionError::InvalidOutput);
    }
    let riff_end = u32::from_le_bytes(header[4..8].try_into().unwrap()) as u64 + 8;
    if riff_end > file_len || riff_end < 36 {
        return Err(ExtractionError::InvalidOutput);
    }

    let mut format_valid = false;
    let mut data_bytes = None;
    let mut chunks_seen = 0usize;
    while file
        .stream_position()
        .map_err(|_| ExtractionError::InvalidOutput)?
        + 8
        <= riff_end
    {
        chunks_seen += 1;
        if chunks_seen > 128 {
            return Err(ExtractionError::InvalidOutput);
        }
        let mut chunk_header = [0u8; 8];
        file.read_exact(&mut chunk_header)
            .map_err(|_| ExtractionError::InvalidOutput)?;
        let chunk_len = u32::from_le_bytes(chunk_header[4..8].try_into().unwrap()) as u64;
        let body_start = file
            .stream_position()
            .map_err(|_| ExtractionError::InvalidOutput)?;
        let next = body_start
            .checked_add(chunk_len)
            .and_then(|value| value.checked_add(chunk_len % 2))
            .ok_or(ExtractionError::InvalidOutput)?;
        if next > riff_end {
            return Err(ExtractionError::InvalidOutput);
        }
        match &chunk_header[..4] {
            b"fmt " => {
                if format_valid || chunk_len < 16 {
                    return Err(ExtractionError::InvalidOutput);
                }
                let mut format = [0u8; 16];
                file.read_exact(&mut format)
                    .map_err(|_| ExtractionError::InvalidOutput)?;
                format_valid = u16::from_le_bytes(format[0..2].try_into().unwrap()) == 1
                    && u16::from_le_bytes(format[2..4].try_into().unwrap()) == 1
                    && u32::from_le_bytes(format[4..8].try_into().unwrap()) == 16_000
                    && u32::from_le_bytes(format[8..12].try_into().unwrap()) == 32_000
                    && u16::from_le_bytes(format[12..14].try_into().unwrap()) == 2
                    && u16::from_le_bytes(format[14..16].try_into().unwrap()) == 16;
                if !format_valid {
                    return Err(ExtractionError::InvalidOutput);
                }
            }
            b"data" => {
                if data_bytes.replace(chunk_len).is_some() {
                    return Err(ExtractionError::InvalidOutput);
                }
            }
            _ => {}
        }
        file.seek(SeekFrom::Start(next))
            .map_err(|_| ExtractionError::InvalidOutput)?;
    }
    let bytes = data_bytes.ok_or(ExtractionError::InvalidOutput)?;
    if !format_valid || bytes % 2 != 0 {
        return Err(ExtractionError::InvalidOutput);
    }
    let samples = bytes / 2;
    samples
        .checked_mul(1000)
        .and_then(|value| value.checked_add(15_999))
        .map(|value| value / 16_000)
        .ok_or(ExtractionError::InvalidOutput)
}

fn format_seconds(milliseconds: u64) -> String {
    format!("{}.{:03}", milliseconds / 1000, milliseconds % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_source_is_rejected_before_running_ffmpeg() {
        let staging = tempfile::tempdir().unwrap();
        let chunk = AudioChunk {
            index: 0,
            start_ms: 0,
            end_ms: 1_000,
            core_start_ms: 0,
            core_end_ms: 1_000,
        };
        let result = extract_wav_chunk(
            &staging.path().join("missing.mp4"),
            staging.path(),
            chunk,
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(result, Err(ExtractionError::InvalidSource)));
    }

    #[test]
    fn ffmpeg_time_arguments_keep_exact_millisecond_offsets() {
        assert_eq!(format_seconds(0), "0.000");
        assert_eq!(format_seconds(3_600_000_123), "3600000.123");
    }

    #[tokio::test]
    async fn wav_duration_tracks_audio_stream_in_longer_video_container() {
        let Ok(ffmpeg) = crate::media_tools::ffmpeg() else {
            return;
        };
        let mut check = std::process::Command::new(&ffmpeg);
        crate::media_tools::hide_console(&mut check);
        if check.arg("-version").output().is_err() {
            return;
        }
        let staging = tempfile::tempdir().unwrap();
        let source = staging.path().join("long-video-short-audio.mkv");
        let mut generate = std::process::Command::new(ffmpeg);
        crate::media_tools::hide_console(&mut generate);
        assert!(
            generate
                .args([
                    "-nostdin",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=black:s=160x90:r=25:d=2",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=16000:duration=1",
                    "-map",
                    "0:v:0",
                    "-map",
                    "1:a:0",
                    "-c:v",
                    "mpeg4",
                    "-c:a",
                    "aac",
                    "-y",
                ])
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );

        let full_chunk = AudioChunk {
            index: 0,
            start_ms: 0,
            end_ms: 2_000,
            core_start_ms: 0,
            core_end_ms: 2_000,
        };
        let full = extract_wav_chunk(
            &source,
            staging.path(),
            full_chunk,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            (900..=1_100).contains(&full.actual_duration_ms),
            "actual WAV duration {} ms",
            full.actual_duration_ms
        );
        assert!(full.actual_duration_ms < full_chunk.end_ms);

        let tail_chunk = AudioChunk {
            index: 1,
            start_ms: 1_500,
            end_ms: 2_000,
            core_start_ms: 1_500,
            core_end_ms: 2_000,
        };
        let tail = extract_wav_chunk(
            &source,
            staging.path(),
            tail_chunk,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(tail.actual_duration_ms, 0);
    }
}
