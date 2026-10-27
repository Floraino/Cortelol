//! Open only fixed official pages in the user's browser; never an imported URL.

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AcquisitionHelp {
    YoutubeStudio,
    GoogleTakeout,
}

impl AcquisitionHelp {
    fn url(&self) -> &'static str {
        match self {
            Self::YoutubeStudio => "https://studio.youtube.com/",
            Self::GoogleTakeout => "https://takeout.google.com/",
        }
    }
}

pub(super) fn open_acquisition_help(topic: AcquisitionHelp) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new("explorer.exe");
        command.creation_flags(0x0800_0000);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");
    command.arg(topic.url()).spawn().map(|_| ()).map_err(|_| {
        "The official page could not be opened. Copy its link and open it in your browser.".into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_command_accepts_only_fixed_topics_never_arbitrary_urls() {
        assert!(serde_json::from_str::<AcquisitionHelp>("\"https://127.0.0.1/\"").is_err());
        for removed in ["vimeoDownload", "vimeoLibrary", "vimeoDirectLinks"] {
            assert!(serde_json::from_value::<AcquisitionHelp>(serde_json::json!(removed)).is_err());
        }
        for topic in [
            AcquisitionHelp::YoutubeStudio,
            AcquisitionHelp::GoogleTakeout,
        ] {
            assert!(topic.url().starts_with("https://"));
            assert!(!topic.url().contains('?'));
        }
    }
}
