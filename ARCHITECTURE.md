# Architecture
# 01 System Overview
Desktop UI (Tauri + React) -> typed local commands -> Rust application services -> local SQLite and project media. Application services call FFmpeg for probing, audio extraction, cropping, subtitle rendering, and MP4 export. Provider adapters call user-selected Gemini or ElevenLabs for transcription and a selected Claude, OpenAI, or DeepSeek model for clip and copy suggestions. Publishing adapters call official TikTok and Instagram APIs only after user review and authorization.

The desktop process is the trust boundary: the webview receives only the commands and data it needs. Source video, rendered clips, transcript edits, job state, and account connections are local by default. No hosted Cortelol service is required for the core editing workflow. Direct publishing may need a temporary, access-controlled media URL or platform-supported upload mechanism; design that adapter separately and delete staged media after use.

# 02 Tech Stack
| Area | Tool |
|------------|---------------------------------------|
| Frontend | Tauri webview, React, TypeScript, Vite, Tailwind CSS |
| Backend | Rust Tauri commands and application services |
| Auth | No Cortelol account; provider API keys and social OAuth only |
| Payments | None in version 1; users pay their own AI providers |
| Email | None in version 1 |
| Analytics | Local diagnostics only by default; no automatic telemetry |
| Deployment | Reproducible Windows desktop release first; macOS and Linux after platform validation |

Additional local dependencies: SQLite for project metadata and job state; FFmpeg and FFprobe for media operations; OS credential storage for API keys and OAuth tokens. Pin FFmpeg distribution and document its license and build options for each release. Provider model IDs, pricing, scopes, and publishing limits are configuration or adapter concerns because they can change.

The Rust backend uses `rusqlite` for local SQLite data, `keyring` for the operating system credential store, `sha2` for corrected-transcript revision IDs, and async `reqwest`/`tokio` for provider HTTP work. Keep provider credentials and HTTP responses behind Rust services; the webview receives only validated project and job data. The Tauri asset protocol has an empty static scope. Import and restore add only the validated recording to the runtime scope so the editor can preview it locally.

Workspace navigation and credential settings are available before media import. Settings uses the same narrow Rust credential commands as the provider workspaces and never reads saved keys back into the webview. On Windows, media child processes use `CREATE_NO_WINDOW` so FFmpeg and FFprobe do not open console windows over the desktop UI.
The Windows main window uses a custom titlebar inside the React shell. Tauri removes native decorations, and the main-window capability grants only close, minimize, maximize toggle, and dragging commands needed by those controls. Workspace content scrolls independently of the fixed sidebar and window controls.

# 03 Project Structure
```text
cortelol/
|-- src/                    # React application shell and feature workspaces
|   |-- features/           # Settings, transcript, discovery, export, and publish UI
|-- src-tauri/              # Rust commands, permissions, and build configuration
|   |-- src/projects.rs     # Versioned SQLite schema, imports, jobs, corrections, reviews
|   |-- src/media.rs        # FFprobe boundary and media metadata
|   |-- src/credentials.rs  # Operating system credential store
|   |-- src/transcription/  # Chunking, extraction, Gemini, and job coordination
|   |-- src/discovery/      # Claude/OpenAI/DeepSeek adapters, validation, and job coordination
|   |-- src/export/         # FFmpeg render and export jobs
|   |-- src/publishing/     # Publishing state and platform adapters
|-- PRD.md                  # Product requirements
|-- DESIGN_SYSTEM.md        # UI contract
|-- ARCHITECTURE.md         # Technical architecture
`-- AGENTS.md               # Coding-agent instructions
```

Project media lives in a user-selected workspace directory, outside the source tree. Keep source files immutable and store edits as project metadata. Generated previews, extracted audio, and exports use distinct paths so cleanup cannot remove source media.

# 04 Data Flow
1. The user imports a local recording. FFprobe validates the file and records media metadata in SQLite; the application references the original file without copying it unless the user chooses to consolidate a project.
2. A persistent local job extracts audio and divides it into 15-minute overlapping chunks with source-time offsets. Each extracted WAV is checked for PCM format and sample duration before upload, so timestamp validation uses the audio actually sent rather than the container duration. The selected Gemini or ElevenLabs adapter receives an audio chunk and returns timed words; no provider call is made per word. The app groups valid words into readable timed phrases before saving them, then shows a continuous searchable transcript. Small boundary errors, point timestamps, and short reversed intervals are repaired within bounds. Larger Gemini timing errors and invalid ElevenLabs word timing trigger one bounded attempt with two shorter overlapping requests. A parent chunk is saved only after all required requests succeed. Completed chunks and user corrections remain available across retries, provider changes for pending chunks, and restarts. ElevenLabs receives audio only after provider-specific consent; its default retention applies because zero-retention mode is not available for ordinary API accounts.
3. The user corrects the transcript. The app stores corrections separately from raw provider output and uses the corrected version for captions and clip selection.
4. The selected Claude, OpenAI, or DeepSeek adapter receives bounded transcript windows and a strict response schema. Discovery asks for complete 120–300 second arcs with a specific viewer payoff, supporting evidence, development, and an ending; a short exact quote anchors each proposal inside the source. The service validates every generated range and quote against the source transcript, discards individual unverified suggestions, ranks verified clips by subjective editorial score, de-duplicates nearly identical cuts, and saves distinct clips for user review. A window with no verified suggestions can be marked complete with a visible discard count so one bad model answer does not block later windows. Malformed response envelopes and request failures remain errors. The discovery contract is versioned in the transcript revision key so older generic suggestions are not reused as completed editorial discovery.
5. The user accepts a suggestion, adjusts in/out points and framing, edits captions and copy, and previews the result. A per-project SQLite preference chooses local Windows face analysis or optional Gemini visual analysis for editable 9:16 framing suggestions. Local analysis samples bounded frames without upload; Gemini requires consent for each request and sends only five bounded JPEG samples without audio. Suggestions can provide time-indexed crop positions or fit choices for wide shots. The creator can edit these framing keyframes, and neither provider result changes the reviewed draft until the user applies it. Existing single-position drafts remain valid. Legacy word-level transcript rows are grouped into caption phrases for initial drafts, while saved user edits remain intact. Caption regeneration uses the corrected AI transcript without a new provider request and replaces draft caption edits only after the user asks. Local fallback previews support up to five minutes. FFmpeg renders an MP4 and optional subtitle file from the saved edit decisions. Export jobs use temporary outputs and atomically move completed files into the export folder.
6. The user reviews each exported clip and chooses a destination account. The publishing adapter verifies current account capabilities, obtains explicit consent and destination settings, uploads through the official API, polls processing status, and records the returned post ID or actionable failure. A failed post never invalidates the local export.

All external calls have timeouts, bounded retries, cancellation, and per-provider error mapping. Jobs persist their state so a crash does not silently lose edits. Do not log media, transcripts, credentials, or raw provider responses by default.

# 05 Scalability
- Keep media processing, AI providers, and social platforms behind separate interfaces; version project data and use migrations as schemas evolve.
- Limit simultaneous media renders and API requests to prevent exhausting local memory, disk, provider quota, or publishing rate limits.
- Cache transcript and rendered preview artifacts by source hash plus edit settings; invalidate only affected stages after user changes.
- Add macOS and Linux packaging only after confirming FFmpeg distribution, credential storage, file dialogs, and GPU behavior on each platform.
- Update this file before adding hosted services, new providers, major dependencies, or a new persistence model.
- Recheck official platform and model documentation before implementation or release: [Gemini transcription](https://ai.google.dev/gemini-api/docs/transcribe), [ElevenLabs Speech to Text](https://elevenlabs.io/docs/api-reference/speech-to-text/convert), [TikTok Direct Post](https://developers.tiktok.com/docs/en/content-posting-api-get-started), [Meta Instagram API](https://www.postman.com/meta/instagram/documentation/6yqw8pt/instagram-api), and [FFmpeg filters](https://ffmpeg.org/ffmpeg-filters.html). TikTok public Direct Post requires platform approval, and Instagram publishing is limited to eligible professional accounts and required permissions.
