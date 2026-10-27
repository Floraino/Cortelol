//! Bounded local extractor candidate. This subprocess can access user files and
//! the network. Job assignment follows spawn, leaving a child-escape race.
//! Native AppContainer, broker, hostile-worker, and packaged-runtime tests remain
//! mandatory gates before an installer enables this path.
use super::{DownloadedVideo, youtube_url::YoutubeVideoId};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::sync::CancellationToken;

pub(super) const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_FILES: usize = 256;
const MAX_PROGRESS_LINE: usize = 8192;
const TIME_LIMIT: Duration = Duration::from_secs(30 * 60);

fn binaries() -> Result<(PathBuf, PathBuf), String> {
    let directory = std::env::current_exe()
        .map_err(|_| "The app binary cannot be located.")?
        .parent()
        .ok_or("The app binary directory is unavailable.")?
        .to_path_buf();
    let yt = directory.join("yt-dlp.exe");
    let deno = directory.join("deno.exe");
    if yt.is_file() && deno.is_file() {
        return Ok((yt, deno));
    }
    #[cfg(debug_assertions)]
    {
        fn find(name: &str) -> Option<PathBuf> {
            std::env::split_paths(&std::env::var_os("PATH")?)
                .map(|dir| dir.join(name))
                .find(|p| p.is_file())
        }
        return match (find("yt-dlp.exe"), find("deno.exe")) {
            (Some(yt), Some(deno)) => Ok((yt, deno)),
            _ => Err("yt-dlp.exe and deno.exe are required for development downloads.".into()),
        };
    }
    #[cfg(not(debug_assertions))]
    Err("The packaged yt-dlp.exe and deno.exe sidecars are missing.".into())
}

fn scan(directory: &Path) -> Result<(u64, Vec<PathBuf>), String> {
    let mut size = 0u64;
    let mut files = Vec::new();
    let mut dirs = vec![directory.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir).map_err(|_| "Download staging cannot be read.")? {
            let entry = entry.map_err(|_| "Download staging cannot be read.")?;
            let kind = entry
                .file_type()
                .map_err(|_| "Download staging cannot be read.")?;
            if kind.is_dir() {
                if dirs.len() + files.len() >= MAX_FILES {
                    return Err("The download exceeded its staging limits.".into());
                }
                dirs.push(entry.path());
                continue;
            }
            if !kind.is_file() {
                return Err("The extractor created an unsupported staging entry.".into());
            }
            size = size
                .checked_add(
                    entry
                        .metadata()
                        .map_err(|_| "Download staging cannot be read.")?
                        .len(),
                )
                .ok_or("The download exceeded the 2 GiB limit.")?;
            if size > MAX_BYTES || files.len() >= MAX_FILES {
                return Err("The download exceeded its staging limits.".into());
            }
            files.push(entry.path());
        }
    }
    Ok((size, files))
}

fn progress_line(line: &[u8]) -> Option<(u64, Option<u64>)> {
    let line = std::str::from_utf8(line).ok()?.trim();
    let rest = line.strip_prefix("DL:")?;
    let (done, total) = rest.split_once(':')?;
    let done = done.parse::<u64>().ok()?;
    if done > MAX_BYTES {
        return None;
    }
    let total = total.parse::<u64>().ok().filter(|n| *n <= MAX_BYTES);
    Some((done, total))
}

#[cfg(windows)]
mod tree {
    use std::ffi::c_void;
    type Handle = *mut c_void;
    const PROCESS_SET_QUOTA: u32 = 0x0100;
    const PROCESS_TERMINATE: u32 = 0x0001;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    #[repr(C)]
    struct IoCounters {
        a: [u64; 6],
    }
    #[repr(C)]
    struct BasicLimit {
        per_process: i64,
        per_job: i64,
        flags: u32,
        min_ws: usize,
        max_ws: usize,
        active: u32,
        affinity: usize,
        priority: u32,
        scheduling: u32,
    }
    #[repr(C)]
    struct ExtendedLimit {
        basic: BasicLimit,
        io: IoCounters,
        process_memory: usize,
        job_memory: usize,
        peak_process: usize,
        peak_job: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attrs: *mut c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(
            job: Handle,
            class: i32,
            info: *const c_void,
            length: u32,
        ) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }
    pub(super) struct Job(Handle);
    impl Job {
        pub(super) fn assign(pid: u32) -> Result<Self, String> {
            unsafe {
                let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if job.is_null() {
                    return Err("The process tree could not be contained.".into());
                }
                let result = Self(job);
                let mut limits: ExtendedLimit = std::mem::zeroed();
                limits.basic.flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    &limits as *const _ as _,
                    std::mem::size_of::<ExtendedLimit>() as u32,
                ) == 0
                {
                    return Err("The process tree could not be contained.".into());
                }
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
                if process.is_null() {
                    return Err("The process tree could not be contained.".into());
                }
                let assigned = AssignProcessToJobObject(job, process) != 0;
                CloseHandle(process);
                if !assigned {
                    return Err("The process tree could not be contained.".into());
                }
                Ok(result)
            }
        }
    }
    unsafe impl Send for Job {}
    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

pub(super) async fn run(
    root: &Path,
    id: &YoutubeVideoId,
    token: &CancellationToken,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<DownloadedVideo, String> {
    fs::create_dir_all(root).map_err(|_| "The download folder could not be created.")?;
    let stage = tempfile::Builder::new()
        .prefix("youtube-")
        .tempdir_in(root)
        .map_err(|_| "Private download staging could not be created.")?;
    let (yt, deno) = binaries()?;
    // A single progressive file must contain audio and video. Quality is limited
    // until a fixed, packaged FFmpeg muxer can be validated.
    let mut command = Command::new(yt);
    command
        .args([
            "--ignore-config",
            "--no-config-locations",
            "--no-plugin-dirs",
            "--no-remote-components",
            "--no-playlist",
            "--no-exec",
            "--no-cache-dir",
            "--no-write-info-json",
            "--no-write-thumbnail",
            "--no-write-subs",
            "--no-write-auto-subs",
            "--no-write-description",
            "--no-mtime",
            "--no-overwrites",
            "--no-continue",
            "--no-part",
            "--newline",
            "--progress",
            "--progress-template",
            "download:DL:%(progress.downloaded_bytes)s:%(progress.total_bytes_estimate)s",
            "--max-filesize",
            "2G",
            "--socket-timeout",
            "30",
            "--retries",
            "2",
            "--fragment-retries",
            "2",
            "--no-js-runtimes",
            "--js-runtimes",
        ])
        .arg(format!("deno:{}", deno.display()))
        .args([
            "--downloader",
            "native",
            "--format",
            "best[vcodec!=none][acodec!=none][ext=mp4]/best[vcodec!=none][acodec!=none][ext=webm]",
            "--output",
            "video.%(ext)s",
            "--paths",
        ])
        .arg(stage.path())
        .arg("--")
        .arg(id.canonical_url());
    command
        .current_dir(stage.path())
        .env_clear()
        .env("TEMP", stage.path())
        .env("TMP", stage.path())
        .env("DENO_DIR", stage.path().join("deno-cache"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let mut child = command
        .spawn()
        .map_err(|_| "The packaged yt-dlp worker could not start.")?;
    #[cfg(windows)]
    let tree = match tree::Job::assign(child.id().ok_or("The worker process ID is unavailable.")?) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill().await;
            return Err(error);
        }
    };
    let mut stdout = child
        .stdout
        .take()
        .ok_or("The worker output is unavailable.")?;
    let started = Instant::now();
    let mut pending = Vec::new();
    let mut discard_line = false;
    let mut buffer = [0u8; 4096];
    let outcome: Result<(), String> = loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => break Err("The download was canceled.".into()),
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                if started.elapsed() > TIME_LIMIT { break Err("The download exceeded the 30-minute time limit.".into()); }
                if scan(stage.path()).is_err() { break Err("The download exceeded its staging limits.".into()); }
                match child.try_wait() {
                    Ok(Some(status)) => break if status.success() { Ok(()) } else { Err("The video could not be downloaded. It may be unavailable or unsupported.".into()) },
                    Ok(None) => {},
                    Err(_) => break Err("The worker could not be monitored.".into()),
                }
            },
            read = stdout.read(&mut buffer) => match read {
                Ok(0) => {
                    let remaining = TIME_LIMIT.saturating_sub(started.elapsed());
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => break Err("The download was canceled.".into()),
                        result = tokio::time::timeout(remaining, child.wait()) => match result {
                            Ok(Ok(status)) => break if status.success() { Ok(()) } else { Err("The video could not be downloaded. It may be unavailable or unsupported.".into()) },
                            Ok(Err(_)) => break Err("The worker could not be monitored.".into()),
                            Err(_) => break Err("The download exceeded the 30-minute time limit.".into()),
                        }
                    }
                },
                Ok(n) => {
                    for byte in &buffer[..n] {
                        if *byte == b'\n' {
                            if !discard_line {
                                if let Some((done, total)) = progress_line(&pending) { progress(done, total); }
                            }
                            pending.clear();
                            discard_line = false;
                        } else if !discard_line {
                            if pending.len() < MAX_PROGRESS_LINE { pending.push(*byte); }
                            else { pending.clear(); discard_line = true; }
                        }
                    }
                },
                Err(_) => break Err("The worker output could not be read.".into()),
            }
        }
    };
    if outcome.is_err() {
        let _ = child.kill().await;
    }
    #[cfg(windows)]
    drop(tree); // Kills assigned descendants. Spawn-to-assignment remains a containment race.
    outcome?;
    let (_, files) = scan(stage.path())?;
    let media = files
        .into_iter()
        .find(|path| {
            matches!(
                path.extension().and_then(|x| x.to_str()),
                Some("mp4" | "webm")
            )
        })
        .ok_or("The worker did not produce a supported media file.")?;
    let size = fs::metadata(&media)
        .map_err(|_| "The downloaded media could not be read.")?
        .len();
    if size == 0 || size > MAX_BYTES {
        return Err("The downloaded media size is invalid.".into());
    }
    crate::media::probe_media(media.to_string_lossy().into_owned())?;
    publish(root, &media, id, size)
}

fn publish(
    root: &Path,
    media: &Path,
    id: &YoutubeVideoId,
    size: u64,
) -> Result<DownloadedVideo, String> {
    let extension = media
        .extension()
        .and_then(|x| x.to_str())
        .ok_or("Invalid media extension.")?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(media)
        .map_err(|_| "The downloaded media could not be read.")?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|_| "The downloaded media could not be hashed.")?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    let sha256 = format!("{:x}", hash.finalize());
    let unique = media
        .parent()
        .and_then(Path::file_name)
        .and_then(|x| x.to_str())
        .ok_or("Invalid staging name.")?;
    let target = root.join(format!("{unique}.{extension}"));
    let receipt_path = root.join(format!("{unique}.{extension}.source.json"));
    let acquired_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "The system clock is invalid.")?
        .as_millis();
    let receipt = serde_json::json!({
        "schemaVersion": 1, "sourceSite": "YouTube", "videoId": id.as_str(),
        "acquisitionMethod": "yt-dlp-local-candidate", "fileName": target.file_name().unwrap().to_string_lossy(),
        "fileSizeBytes": size, "sha256": sha256, "acquiredAtUnixMs": acquired_at_unix_ms,
        "ownsMediaConfirmed": true
    });
    let mut temp_receipt = tempfile::NamedTempFile::new_in(root)
        .map_err(|_| "The acquisition receipt could not be created.")?;
    serde_json::to_writer_pretty(&mut temp_receipt, &receipt)
        .map_err(|_| "The acquisition receipt could not be written.")?;
    temp_receipt
        .write_all(b"\n")
        .map_err(|_| "The acquisition receipt could not be written.")?;
    temp_receipt
        .as_file()
        .sync_all()
        .map_err(|_| "The acquisition receipt could not be saved.")?;
    file.sync_all()
        .map_err(|_| "The downloaded media could not be saved.")?;
    fs::hard_link(media, &target).map_err(
        |_| "The downloaded media could not be published without overwriting an existing file.",
    )?;
    if temp_receipt.persist_noclobber(&receipt_path).is_err() {
        let _ = fs::remove_file(&target);
        return Err("The acquisition receipt could not be published.".into());
    }
    Ok(DownloadedVideo {
        path: target.to_string_lossy().into_owned(),
        receipt_path: receipt_path.to_string_lossy().into_owned(),
        sha256,
        file_size_bytes: size,
        source_site: "YouTube",
        acquisition_method: "yt-dlp-local-candidate",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_only_bounded_numeric_progress() {
        assert_eq!(progress_line(b"DL:42:100\n"), Some((42, Some(100))));
        assert_eq!(progress_line(b"DL:42:NA"), Some((42, None)));
        assert_eq!(progress_line(b"DL:999999999999:100"), None);
        assert_eq!(progress_line(b"https://secret"), None);
    }
    #[test]
    fn fixture_publication_does_not_store_signed_urls_or_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let stage = tempfile::Builder::new()
            .prefix("youtube-")
            .tempdir_in(dir.path())
            .unwrap();
        let media = stage.path().join("video.mp4");
        fs::write(&media, b"fixture-media").unwrap();
        let id = YoutubeVideoId::parse("https://youtu.be/YE7VzlLtp-4").unwrap();
        let saved = publish(dir.path(), &media, &id, 13).unwrap();
        let receipt = fs::read_to_string(saved.receipt_path).unwrap();
        assert!(receipt.contains("YE7VzlLtp-4"));
        assert!(!receipt.contains("https:"));
        assert!(publish(dir.path(), &media, &id, 13).is_err());
        assert_eq!(fs::read(saved.path).unwrap(), b"fixture-media");
    }
}
