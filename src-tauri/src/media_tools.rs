//! Resolve bundled media tools in a release and developer tools on PATH.

use std::path::PathBuf;
use std::process::Command;

// FFmpeg and FFprobe are console programs. A GUI process must suppress their
// console windows, including in development builds on Windows.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

pub fn hide_console_async(command: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn executable(name: &'static str) -> Result<PathBuf, String> {
    #[cfg(debug_assertions)]
    {
        Ok(PathBuf::from(name))
    }
    #[cfg(not(debug_assertions))]
    {
        let binary = std::env::current_exe()
            .map_err(|_| "The application executable path is unavailable.".to_string())?
            .parent()
            .ok_or_else(|| "The application executable directory is unavailable.".to_string())?
            .join(name);
        if !binary.is_file() {
            return Err(format!(
                "The bundled {name} media tool is missing. Reinstall Cortelol."
            ));
        }
        Ok(binary)
    }
}

pub fn ffmpeg() -> Result<PathBuf, String> {
    executable(if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    })
}

pub fn ffprobe() -> Result<PathBuf, String> {
    executable(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn development_uses_named_executables() {
        assert_eq!(
            ffmpeg().unwrap(),
            PathBuf::from(if cfg!(windows) {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            })
        );
        assert_eq!(
            ffprobe().unwrap(),
            PathBuf::from(if cfg!(windows) {
                "ffprobe.exe"
            } else {
                "ffprobe"
            })
        );
    }
}
