mod credentials;
mod discovery;
mod download;
mod export;
mod local_asr;
mod media;
mod media_tools;
mod projects;
mod publishing;
mod transcription;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(transcription::jobs::TranscriptionRuntime::default())
        .manage(local_asr::LocalAsrRuntime::default())
        .manage(download::DownloadRuntime::default())
        .manage(discovery::jobs::DiscoveryRuntime::default())
        .manage(export::jobs::ExportRuntime::default())
        .manage(export::framing::FramingRuntime::default())
        .manage(export::cloud_framing::CloudFramingRuntime::default())
        .manage(publishing::jobs::PublishRuntime::default())
        .invoke_handler(tauri::generate_handler![
            credentials::save_api_key,
            credentials::has_api_key,
            credentials::delete_api_key,
            projects::import_media,
            download::download_video,
            download::get_download_status,
            download::cancel_download_video,
            download::open_acquisition_help,
            download::youtube_url::validate_youtube_video_url,
            projects::load_recent_media_command,
            projects::get_framing_analysis_mode,
            projects::set_framing_analysis_mode,
            transcription::jobs::transcribe_recent,
            transcription::jobs::get_transcription_status,
            transcription::jobs::cancel_transcription,
            transcription::jobs::update_transcript_segment,
            local_asr::validate_local_transcription_setup,
            local_asr::transcribe_local_recent,
            local_asr::get_local_transcription_status,
            local_asr::get_local_transcription_report,
            local_asr::cancel_local_transcription,
            discovery::jobs::get_discovery_status,
            discovery::jobs::discover_recent,
            discovery::jobs::cancel_discovery,
            discovery::jobs::review_suggestion,
            export::jobs::get_export_status,
            export::jobs::get_export_draft,
            export::jobs::save_export_draft,
            export::jobs::review_export_draft,
            export::jobs::start_export_reviewed,
            export::jobs::cancel_export,
            export::jobs::prepare_export_preview,
            export::jobs::cancel_export_preview,
            export::jobs::discard_export_preview,
            export::framing::suggest_local_framing,
            export::framing::cancel_local_framing,
            export::cloud_framing::suggest_gemini_framing,
            export::cloud_framing::cancel_gemini_framing,
            publishing::jobs::get_publish_status,
            publishing::jobs::get_publish_capabilities,
            publishing::jobs::prepare_publish,
            publishing::jobs::confirm_publish,
            publishing::jobs::cancel_publish,
            publishing::jobs::refresh_publish_status
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
