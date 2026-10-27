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

# 06 Public Website
`docs/` contains a dependency-free static landing page published from the `main` branch through GitHub Pages. Its JavaScript switches between bundled Portuguese, English, Spanish, and French strings and advances synthetic application scenes as the visitor scrolls. It has no backend, analytics, form submission, access to project media, or API credentials. Download links point to the public repository source archive and the GitHub Releases page. An optional unauthenticated request to the public GitHub Releases API may enable a direct installer link only when a published release includes a Windows installer asset. A request failure leaves the source download and accurate preparation status available.

# 07 Authorized Desktop Extensions

## Desktop localization

`src/i18n/index.tsx` defines the desktop locale set `en`, `pt-BR`, `es`, `fr`, `de`, `it`, `ar`, `hi`, `zh-CN`, and `ja`, persists the selection in local storage, sets document language and direction, and falls back to English for missing keys. Static shell and feature catalogs cover all ten locales, including local ASR and acquisition controls. Dynamic Rust job stages and raw command errors may still appear in English. Route navigation, workspace copy, validation and job errors, consent, and accessible names through the same catalog; verify Arabic right-to-left layout and locale-specific formatting without changing persisted timestamps or media metadata. Keep transcript text, subtitles, creator edits, and generated copy in their source language unless the user separately requests translation. The public website's four-language switch is independent of the desktop preference.

## Owned-video acquisition

This paragraph records the earlier implemented Vimeo route. The proposed YouTube-only replacement and its boundary follow below; this documentation update does not remove or disable existing commands.

Acquisition is separate from local-file import. The standalone `scripts/download_video.py` uses `yt-dlp`; its validation cannot constrain every downstream request and it must not be invoked by the desktop app. The desktop acquisition panel instead provides YouTube Studio/Google Takeout and Vimeo's official download handoffs. The Rust direct-file downloader accepts a user-supplied official Vimeo file link after rights and method confirmation. It must disable implicit redirects and system proxies, validate every HTTPS URL and DNS answer against private/reserved ranges, pin the chosen public IP for each connection, revalidate each redirect, verify the connected peer, bound time and bytes, and atomically publish the file and local provenance receipt without persisting signed URL queries. The downloaded file then passes the existing `import_media` validation. Do not present a YouTube video-page URL extractor as a desktop feature. Never bypass DRM, authentication, age, geographic, paywall, rate-limit, or other access controls, or supply site cookies or credentials. Recheck [YouTube creator downloads](https://support.google.com/youtube/answer/56100), [Google Takeout](https://support.google.com/accounts/answer/3024190), and [Vimeo direct file links](https://help.vimeo.com/hc/en-us/articles/12426150952593-How-to-access-my-video-s-direct-links) before release.

## Proposed YouTube-only acquisition boundary (2026-10-27)

[PRD section 11](PRD.md#11-proposed-youtube-only-acquisition-scope-change) records the requested arbitrary-channel downloader as proposed. The official-source review defines the currently supported method as a user-operated Studio/Takeout export followed by existing local-file import. Fixed official help links may open in the user's external browser on an explicit action. Cortelol must not automate that browser, collect its session, inspect Premium offline storage, or exchange a pasted video URL for player, metadata, manifest, or CDN requests. An authorized local recording supplied by another creator uses the same existing import path.

For the proposed YouTube-only revision, remove the Vimeo form and handoffs and reject the old Vimeo transfer command at the Rust boundary, including direct invocations from a webview. Do not broaden the old downloader's hostname allowlist to YouTube or Google video hosts. Preserve downloaded sources, receipts, restored projects, and exports; retiring a transfer feature is not a cleanup operation. Standalone extraction scripts are outside this desktop boundary and must not be invoked or advertised as an automatic fallback. No new dependency, provider, extraction service, OAuth scope, or database migration is established by this proposal.

There is no enabled general YouTube URL download method in this design. A disabled or unsupported URL path must return a typed actionable result without network side effects or raw URL logging. Reuse the existing local import and FFprobe validation for files chosen after an official export; never ingest encrypted offline caches. Rights declarations remain user statements, not proof of platform authorization. The [YouTube terms](https://www.youtube.com/static?template=terms) and [API policies, III.E.1](https://developers.google.com/youtube/terms/developer-policies#e.-handling-youtube-data-and-content) do not establish a general arbitrary-channel download capability. This design does not certify legal or contractual compliance.

Before enabling any future URL transfer, document the specific supported method and authorization evidence in the PRD and architecture. Define typed inputs and results, request/redirect/DNS constraints, bounded bytes and time, cancellation, sanitized errors and provenance, temporary files and atomic non-overwriting publication, and validation through the existing import service. Verify command-level rejection of retired providers and unsupported URLs, absence of extractor/network fallback, and preservation of existing sources and receipts. No live arbitrary-channel download or permitted method has been verified in this documentation task; implementation and acceptance of the revised UI and command behavior remain pending.

The user's subsequent direction in [PRD section 11.1](PRD.md#111-public-video-downloader-direction-after-user-correction) removes platform-policy compliance as a gate for the new implementation plan. A local test candidate now accepts one canonical YouTube video ID and launches packaged yt-dlp and Deno executables with fixed arguments, private staging, time and size limits, cancellation, FFprobe validation, and non-overwriting file and receipt publication. `App.tsx` imports the saved file through `import_media`. The executable runs with the user's ordinary file and network privileges; Windows Job Object assignment follows spawn, leaving a race. The separate Rust broker and AppContainer spike are **not** connected to this download path. Treat native containment, mediated requests, live acceptance, and distribution review in [PLAN.md](PLAN.md#youtube-public-video-downloader-plan-2026-10-27) as open gates before a public release. The earlier paragraphs describe previous decisions and are retained as history.

## Local transcription script

`scripts/Transcribe-Video.py` remains a standalone optional tool. The desktop local ASR adapter embeds that fixed script, validates a user-selected Python executable and existing multilingual CTranslate2 model directory outside the repository, and uses Rust's bounded FFmpeg extraction before invoking Python on a prepared WAV. It must validate bounded JSON and source-relative timestamps at the Rust boundary, persist chunks under provider `local` in the existing project database, retain prior corrections and cloud chunk provenance, share the transcription concurrency gate, and expose progress/cancellation to the transcript review screen. Python, `faster-whisper`, and model weights are not bundled or downloaded by the app. The quality report flags uncertainty and gaps but cannot establish accuracy or completeness. No ASR can guarantee 100 percent accuracy, so users must review text, timing, and captions against the source before export.
