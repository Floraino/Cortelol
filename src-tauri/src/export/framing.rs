//! Bounded, local face sampling for an editable vertical crop suggestion.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::projects;

const SAMPLE_COUNT: usize = 7;
const MAX_SAMPLE_COUNT: usize = 30;
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_FRAME_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Default)]
pub struct FramingRuntime {
    active: Mutex<Option<Arc<AtomicBool>>>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FramingConfidence {
    None,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FramingReason {
    NoFace,
    StableFace,
    MovingFace,
    MultipleFaces,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FramingSuggestion {
    pub horizontal_percent: u8,
    pub confidence: FramingConfidence,
    pub reason: FramingReason,
    pub face_samples: usize,
    pub sample_count: usize,
    pub keyframes: Vec<FramingKeyframe>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FramingMode {
    Crop,
    Fit,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FramingKeyframe {
    pub timestamp_ms: u64,
    pub mode: FramingMode,
    pub horizontal_percent: u8,
}

#[derive(Debug, Clone, Copy)]
struct FaceSample {
    center_x: f64,
    aspect_ratio: f64,
    competing_faces: bool,
    needs_fit: bool,
}

#[tauri::command]
pub async fn suggest_local_framing(
    app: AppHandle,
    start_ms: u64,
    end_ms: u64,
) -> Result<FramingSuggestion, String> {
    let media = {
        let app = app.clone();
        tokio::task::spawn_blocking(move || projects::load_recent_media(app))
            .await
            .map_err(|_| "The recording lookup stopped unexpectedly.".to_string())??
            .ok_or_else(|| "Import a recording before suggesting a crop.".to_string())?
    };
    if !media.has_video {
        return Err("This recording has no video to frame.".into());
    }
    let duration_ms = (media.duration_seconds * 1_000.0) as u64;
    if start_ms >= end_ms || end_ms > duration_ms || end_ms - start_ms > 5 * 60 * 1_000 {
        return Err("Select a valid video clip of five minutes or less.".into());
    }
    let source = Path::new(&media.path)
        .canonicalize()
        .map_err(|_| "The source recording could not be found.".to_string())?;
    if !source.is_file() {
        return Err("The source recording must be a file.".into());
    }
    let runtime = app.state::<FramingRuntime>();
    let cancelled = Arc::new(AtomicBool::new(false));
    {
        let mut active = runtime
            .active
            .lock()
            .map_err(|_| "The framing analyzer is unavailable.")?;
        if active.is_some() {
            return Err("A framing suggestion is already running.".into());
        }
        *active = Some(cancelled.clone());
    }
    let worker_cancelled = cancelled.clone();
    let result = tokio::task::spawn_blocking(move || {
        analyze_local(&source, start_ms, end_ms, &worker_cancelled)
    })
    .await;
    if let Ok(mut active) = runtime.active.lock() {
        *active = None;
    }
    result.map_err(|_| "The framing analyzer stopped unexpectedly.".to_string())?
}

#[tauri::command]
pub fn cancel_local_framing(app: AppHandle) -> Result<(), String> {
    if let Some(cancelled) = app
        .state::<FramingRuntime>()
        .active
        .lock()
        .map_err(|_| "The framing analyzer is unavailable.")?
        .as_ref()
    {
        cancelled.store(true, Ordering::Relaxed);
    }
    Ok(())
}

fn analyze_local(
    source: &Path,
    start_ms: u64,
    end_ms: u64,
    cancelled: &AtomicBool,
) -> Result<FramingSuggestion, String> {
    #[cfg(not(windows))]
    {
        let _ = (source, start_ms, end_ms, cancelled);
        return Err("Local face detection currently requires Windows.".into());
    }
    #[cfg(windows)]
    {
        let _apartment = WinRtApartment::initialize()?;
        let directory = tempfile::tempdir()
            .map_err(|_| "Temporary framing images could not be prepared.".to_string())?;
        let sample_count = sample_count_for_duration(end_ms - start_ms);
        let mut samples = Vec::with_capacity(sample_count);
        for index in 0..sample_count {
            if cancelled.load(Ordering::Relaxed) {
                return Err("Framing analysis was cancelled.".into());
            }
            // Stay away from cut boundaries, which often contain transition frames.
            let fraction = (index as f64 + 0.5) / sample_count as f64;
            let timestamp = start_ms + ((end_ms - start_ms) as f64 * fraction) as u64;
            let image = directory.path().join(format!("frame-{index}.jpg"));
            extract_frame(source, &image, timestamp, cancelled)?;
            let detected = tokio::runtime::Handle::current()
                .block_on(tokio::time::timeout(SAMPLE_TIMEOUT, detect_faces(&image)))
                .map_err(|_| {
                    "Windows face detection timed out on a framing sample.".to_string()
                })??;
            samples.push((timestamp, detected));
            let _ = fs::remove_file(image);
        }
        Ok(aggregate_samples(start_ms, &samples))
    }
}

fn sample_count_for_duration(duration_ms: u64) -> usize {
    ((duration_ms.saturating_add(9_999) / 10_000) as usize).clamp(SAMPLE_COUNT, MAX_SAMPLE_COUNT)
}

#[cfg(windows)]
struct WinRtApartment;

#[cfg(windows)]
impl WinRtApartment {
    fn initialize() -> Result<Self, String> {
        use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};

        // This worker thread owns all WinRT image and detector objects.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }
            .map_err(|_| "Windows face detection could not initialize.".to_string())?;
        Ok(Self)
    }
}

#[cfg(windows)]
impl Drop for WinRtApartment {
    fn drop(&mut self) {
        use windows::Win32::System::WinRT::RoUninitialize;

        // Balance the successful RoInitialize call on the same worker thread.
        unsafe { RoUninitialize() };
    }
}

#[cfg(windows)]
fn extract_frame(
    source: &Path,
    output: &Path,
    timestamp_ms: u64,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let mut command = Command::new(crate::media_tools::ffmpeg()?);
    crate::media_tools::hide_console(&mut command);
    let mut child = command
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-ss"])
        .arg(super::seconds(timestamp_ms))
        .arg("-i")
        .arg(source)
        .args([
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-vf",
            "scale=640:640:force_original_aspect_ratio=decrease:force_divisible_by=2",
            "-q:v",
            "4",
            "-y",
        ])
        .arg(output)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "FFmpeg could not start for local framing analysis.".to_string())?;
    let started = Instant::now();
    loop {
        if cancelled.load(Ordering::Relaxed) || started.elapsed() > SAMPLE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(if cancelled.load(Ordering::Relaxed) {
                "Framing analysis was cancelled."
            } else {
                "A framing sample timed out. Try a shorter or simpler video."
            }
            .into());
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return Err("FFmpeg could not decode a framing sample.".into()),
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("The framing sample could not be monitored.".into());
            }
        }
    }
    let size = fs::metadata(output)
        .map_err(|_| "FFmpeg produced no framing sample.".to_string())?
        .len();
    if size == 0 || size > MAX_FRAME_BYTES {
        return Err("A framing sample has an invalid size.".into());
    }
    Ok(())
}

#[cfg(windows)]
async fn detect_faces(path: &Path) -> Result<Option<FaceSample>, String> {
    use windows::Graphics::Imaging::{BitmapDecoder, BitmapPixelFormat, SoftwareBitmap};
    use windows::Media::FaceAnalysis::FaceDetector;
    use windows::Storage::StorageFile;
    use windows::core::HSTRING;

    if !FaceDetector::IsSupported().unwrap_or(false) {
        return Err("Windows face detection is unavailable on this device.".into());
    }
    let path = path
        .to_str()
        .ok_or_else(|| "A temporary framing image path is unsupported.".to_string())?;
    let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path))
        .map_err(|_| "A local framing image could not be opened.".to_string())?
        .await
        .map_err(|_| "A local framing image could not be opened.".to_string())?;
    let stream = file
        .OpenReadAsync()
        .map_err(|_| "A local framing image could not be read.".to_string())?
        .await
        .map_err(|_| "A local framing image could not be read.".to_string())?;
    let decoder = BitmapDecoder::CreateAsync(&stream)
        .map_err(|_| "A local framing image could not be decoded.".to_string())?
        .await
        .map_err(|_| "A local framing image could not be decoded.".to_string())?;
    let bitmap = decoder
        .GetSoftwareBitmapAsync()
        .map_err(|_| "A local framing image could not be decoded.".to_string())?
        .await
        .map_err(|_| "A local framing image could not be decoded.".to_string())?;
    let gray = SoftwareBitmap::Convert(&bitmap, BitmapPixelFormat::Gray8)
        .map_err(|_| "Windows could not prepare the framing image.".to_string())?;
    let width = gray
        .PixelWidth()
        .map_err(|_| "The framing image has invalid dimensions.".to_string())?;
    let height = gray
        .PixelHeight()
        .map_err(|_| "The framing image has invalid dimensions.".to_string())?;
    if width <= 0 || height <= 0 {
        return Err("The framing image has invalid dimensions.".into());
    }
    let detector = FaceDetector::CreateAsync()
        .map_err(|_| "Windows face detection could not start.".to_string())?
        .await
        .map_err(|_| "Windows face detection could not start.".to_string())?;
    let faces = detector
        .DetectFacesAsync(&gray)
        .map_err(|_| "Windows could not analyze a framing sample.".to_string())?
        .await
        .map_err(|_| "Windows could not analyze a framing sample.".to_string())?;
    let count = faces
        .Size()
        .map_err(|_| "Windows returned invalid face detections.".to_string())?;
    let mut boxes = Vec::new();
    for index in 0..count {
        let bounds = faces
            .GetAt(index)
            .and_then(|face| face.FaceBox())
            .map_err(|_| "Windows returned invalid face detections.".to_string())?;
        if bounds.Width > 0 && bounds.Height > 0 {
            boxes.push(bounds);
        }
    }
    boxes.sort_by_key(|bounds| std::cmp::Reverse(bounds.Width as u64 * bounds.Height as u64));
    let Some(main) = boxes.first() else {
        return Ok(None);
    };
    let competing_faces = boxes.get(1).is_some_and(|other| {
        other.Width as u64 * other.Height as u64 * 2 >= main.Width as u64 * main.Height as u64
    });
    let crop_fraction = ((9.0 / 16.0) / (width as f64 / height as f64)).min(1.0);
    let significant = boxes.iter().filter(|face| {
        face.Width as u64 * face.Height as u64 * 2 >= main.Width as u64 * main.Height as u64
    });
    let (left, right) = significant.fold((width as f64, 0.0_f64), |(left, right), face| {
        (
            left.min(face.X as f64),
            right.max((face.X + face.Width) as f64),
        )
    });
    // Include some room around faces. If the people do not fit together in
    // a vertical crop, keep the whole frame visible for this sample.
    let needs_fit =
        competing_faces && group_needs_fit((right - left) / width as f64, crop_fraction);
    let center_x = if competing_faces {
        (left + right) / 2.0 / width as f64
    } else {
        (main.X as f64 + main.Width as f64 / 2.0) / width as f64
    };
    Ok(Some(FaceSample {
        center_x: center_x.clamp(0.0, 1.0),
        aspect_ratio: width as f64 / height as f64,
        competing_faces,
        needs_fit,
    }))
}

fn group_needs_fit(face_span_fraction: f64, crop_fraction: f64) -> bool {
    face_span_fraction + 0.08 > crop_fraction
}

fn crop_percent(sample: FaceSample) -> u8 {
    let crop_fraction = ((9.0 / 16.0) / sample.aspect_ratio).clamp(0.0, 1.0);
    if crop_fraction >= 1.0 {
        50
    } else {
        let left = (sample.center_x - crop_fraction / 2.0).clamp(0.0, 1.0 - crop_fraction);
        (left / (1.0 - crop_fraction) * 100.0).round() as u8
    }
}

fn aggregate_samples(start_ms: u64, samples: &[(u64, Option<FaceSample>)]) -> FramingSuggestion {
    let found: Vec<FaceSample> = samples.iter().filter_map(|(_, sample)| *sample).collect();
    let keyframes = samples
        .iter()
        .enumerate()
        .map(|(index, (timestamp, sample))| {
            let (mode, horizontal_percent) = match sample {
                Some(sample) if !sample.needs_fit => (FramingMode::Crop, crop_percent(*sample)),
                _ => (FramingMode::Fit, 50),
            };
            FramingKeyframe {
                timestamp_ms: if index == 0 { start_ms } else { *timestamp },
                mode,
                horizontal_percent,
            }
        })
        .collect::<Vec<_>>();
    if found.is_empty() {
        return FramingSuggestion {
            horizontal_percent: 50,
            confidence: FramingConfidence::None,
            reason: FramingReason::NoFace,
            face_samples: 0,
            sample_count: samples.len(),
            keyframes,
        };
    }
    let competing = found.iter().any(|sample| sample.competing_faces);
    let mean = found.iter().map(|sample| sample.center_x).sum::<f64>() / found.len() as f64;
    let spread = found
        .iter()
        .map(|sample| (sample.center_x - mean).abs())
        .fold(0.0, f64::max);
    let confidence = if competing || spread > 0.18 || found.len() < samples.len() / 2 {
        FramingConfidence::Low
    } else if spread > 0.08 || found.len() < samples.len() {
        FramingConfidence::Medium
    } else {
        FramingConfidence::High
    };
    let reason = if competing {
        FramingReason::MultipleFaces
    } else if spread > 0.08 {
        FramingReason::MovingFace
    } else {
        FramingReason::StableFace
    };
    // FFmpeg scales to 1080x1920 before cropping. The available horizontal
    // travel depends on the decoded frame aspect ratio, including rotation.
    let horizontal_percent = keyframes
        .first()
        .map_or(50, |frame| frame.horizontal_percent);
    FramingSuggestion {
        horizontal_percent,
        confidence,
        reason,
        face_samples: found.len(),
        sample_count: samples.len(),
        keyframes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longer_cuts_sample_more_shots_with_a_fixed_bound() {
        assert_eq!(sample_count_for_duration(5_000), SAMPLE_COUNT);
        assert_eq!(sample_count_for_duration(120_000), 12);
        assert_eq!(sample_count_for_duration(300_000), MAX_SAMPLE_COUNT);
    }

    #[test]
    fn centered_face_maps_to_centered_crop() {
        let samples = (0..SAMPLE_COUNT)
            .map(|index| {
                (
                    index as u64 * 1_000,
                    Some(FaceSample {
                        center_x: 0.5,
                        aspect_ratio: 16.0 / 9.0,
                        competing_faces: false,
                        needs_fit: false,
                    }),
                )
            })
            .collect::<Vec<_>>();
        let result = aggregate_samples(0, &samples);
        assert_eq!(result.horizontal_percent, 50);
        assert_eq!(result.confidence, FramingConfidence::High);
        assert_eq!(result.keyframes.len(), SAMPLE_COUNT);
    }

    #[test]
    fn off_center_face_moves_crop_and_competing_faces_reduce_confidence() {
        let samples = (0..SAMPLE_COUNT)
            .map(|index| {
                (
                    index as u64 * 1_000,
                    Some(FaceSample {
                        center_x: 0.75,
                        aspect_ratio: 16.0 / 9.0,
                        competing_faces: true,
                        needs_fit: false,
                    }),
                )
            })
            .collect::<Vec<_>>();
        let result = aggregate_samples(0, &samples);
        assert!(result.horizontal_percent > 80);
        assert_eq!(result.confidence, FramingConfidence::Low);
        assert_eq!(result.reason, FramingReason::MultipleFaces);
    }

    #[test]
    fn no_face_falls_back_to_center_without_claiming_confidence() {
        let samples = (0..SAMPLE_COUNT)
            .map(|index| (index as u64 * 1_000, None))
            .collect::<Vec<_>>();
        let result = aggregate_samples(0, &samples);
        assert_eq!(result.horizontal_percent, 50);
        assert_eq!(result.confidence, FramingConfidence::None);
        assert_eq!(result.face_samples, 0);
        assert!(
            result
                .keyframes
                .iter()
                .all(|frame| frame.mode == FramingMode::Fit)
        );
    }

    #[test]
    fn changing_subject_and_wide_group_produce_independent_frame_decisions() {
        let samples = [
            (
                1_000,
                Some(FaceSample {
                    center_x: 0.2,
                    aspect_ratio: 16.0 / 9.0,
                    competing_faces: false,
                    needs_fit: false,
                }),
            ),
            (
                2_000,
                Some(FaceSample {
                    center_x: 0.8,
                    aspect_ratio: 16.0 / 9.0,
                    competing_faces: false,
                    needs_fit: false,
                }),
            ),
            (
                3_000,
                Some(FaceSample {
                    center_x: 0.5,
                    aspect_ratio: 16.0 / 9.0,
                    competing_faces: true,
                    needs_fit: true,
                }),
            ),
        ];
        let result = aggregate_samples(500, &samples);
        assert_eq!(result.keyframes[0].timestamp_ms, 500);
        assert_eq!(result.keyframes[0].mode, FramingMode::Crop);
        assert_eq!(result.keyframes[1].mode, FramingMode::Crop);
        assert!(result.keyframes[0].horizontal_percent < result.keyframes[1].horizontal_percent);
        assert_eq!(result.keyframes[2].mode, FramingMode::Fit);
    }

    #[test]
    fn wide_group_exceeds_vertical_crop_with_face_margin() {
        let crop_fraction = (9.0 / 16.0) / (16.0 / 9.0);
        assert!(!group_needs_fit(0.20, crop_fraction));
        assert!(group_needs_fit(0.26, crop_fraction));
    }
}
