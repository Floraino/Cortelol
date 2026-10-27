# Build Plan

## Product decisions for the first implementation

- Ship a Windows desktop application first.
- Store source media and project data locally. Users supply their own AI credentials.
- Require review before export and a separate explicit action before publishing.
- Use official social APIs when the connected account and application are eligible; keep local export available in every case.
- License Cortelol's own source under MIT. Keep `libx264` for H.264 export and use a GPL-compatible FFmpeg distribution when bundling binaries. Pin the exact build and provide its matching license notices, source, and build configuration before release.
- The initial Windows release hands off reviewed MP4/SRT files for manual platform upload. Direct TikTok/Instagram posting is deferred until an approved developer app and its OAuth prerequisites exist.

## Milestones

| Milestone | Deliverable | Acceptance check | Current status |
|---|---|---|---|
| 0. Foundation | Tauri 2, React, TypeScript, project conventions, and an import screen | Desktop app starts and frontend build passes | Complete |
| 1. Import | Local media picker, FFprobe validation, duration and stream metadata, and useful errors | A selected recording shows accurate metadata and survives restart | In progress: picker, probe, metadata view, and recent import persistence exist; picker interaction still needs desktop verification |
| 2. Transcription | Gemini and ElevenLabs adapters, chunking, timestamp normalization, readable editable transcript, and resumable jobs | A long episode produces a source-aligned transcript and survives app restart | In progress: extracted WAV validation, timed phrase grouping, bounded Gemini timing repair, one short-chunk recovery for severe timing errors, ElevenLabs Scribe v2 option, resumable SQLite jobs, provider provenance, source identity guard, correction storage, and UI are wired; live provider transcription and long-episode restart acceptance remain |
| 3. Discovery | Claude, OpenAI, and DeepSeek adapters, bounded transcript analysis, validated suggestions | Suggested 2–5 minute ranges develop a complete idea, seek to real source material, and can be accepted or rejected | In progress: corrected-transcript integration, persistent suggestions, accept/reject UI, source preview, safe provider diagnostics, phrase-boundary quote matching, per-suggestion discard, and long-form duration validation are wired; live provider acceptance remains |
| 4. Edit and export | Vertical crop, caption editor, FFmpeg render, MP4 and subtitle export | Exported 2–5 minute clip matches the previewed boundaries and caption edits | In progress: five-minute local fallback preview, legacy word-caption grouping, framing controls, editable captions, reviewed export, burned captions, and MP4/SRT render are wired; full desktop acceptance remains |
| 5. Publishing | TikTok and Instagram account connection, eligibility checks, review, upload, and status | Eligible account can publish a reviewed export; failures keep the local file | Deferred from initial release by product decision: manual MP4/SRT upload is the release path; official adapters are groundwork and must stay gated until approved OAuth, live testing, durable status, and Instagram media delivery exist |
| 6. Release | Accessibility pass, packaging, dependency licenses, and open source release files | A clean Windows machine can install and complete the core workflow | In progress: guarded NSIS candidate and source package exist; clean-machine acceptance, full license review, dependency notices, and tagged build remain |

## First vertical slice

Editorial discovery now asks for a specific viewer payoff, source evidence, and a complete 2–5 minute arc, then ranks verified suggestions by subjective score. The editor can suggest a 9:16 center using local Windows face detection or optional Gemini analysis of five sampled frames after consent; the selected method is saved per project. Captions can be regenerated from the corrected AI transcript. These additions passed automated checks but still need live quality review and installed desktop acceptance.

Build milestones 0 and 1 together: a real desktop window where the user selects a local recording and receives media metadata from FFprobe. Recent import persistence now uses a small local SQLite schema. Keep this slice independent of AI credentials and social platform approval.

The workspace now exposes every section, including Settings, before media import. Settings manages Gemini, ElevenLabs, Claude, OpenAI, and DeepSeek keys in the operating system credential store. Windows media subprocesses run without visible console windows. A rebuilt Windows candidate includes these changes; user acceptance is still pending.

## Maestri team assignments

The Maestri team has ten agents in addition to the Codex orchestrator, using only the `Antigravity` and `Codex` presets. Each agent has a workspace-scoped role and a separate file scope.

| Agent | Preset | Responsibility | File scope |
|---|---|---|---|
| Lume | Antigravity | Desktop shell and navigation usability | `src/App.tsx`, `src/App.css` |
| Prisma | Antigravity | Transcript and discovery review flow | `src/features/transcript/`, `src/features/discovery/` |
| Mosaico | Antigravity | Editor, captions, and export usability | `src/features/export/` |
| Ancor | Codex | Import, source validation, and project persistence | `src-tauri/src/media.rs`, `src-tauri/src/projects.rs` |
| Cadencia | Codex | Transcription reliability and restart acceptance | `src-tauri/src/transcription/` |
| Radar | Codex | Discovery provider quality and validation | `src-tauri/src/discovery/` |
| Foco | Codex | FFmpeg preview, framing, and export correctness | `src-tauri/src/export/` |
| Vigia | Codex | Credential and publishing boundary review | Read-only review of `src-tauri/src/credentials.rs` and `src-tauri/src/publishing/` |
| Forja | Codex | Windows packaging and license evidence | `scripts/`, `legal/`, `RELEASE.md` |
| Ponte | Codex | Cross-workflow acceptance and regression review | Read-only integration review; report findings to the owning agents |

Every agent was asked to read `AGENTS.md`, `PRD.md`, `DESIGN_SYSTEM.md`, and `ARCHITECTURE.md`, preserve other agents' edits, and report completion and test evidence through Maestri. No live provider calls or publishing were performed. The ten agents were recruited and verified connected on October 26, 2026. Their launch commands included the project root as an additional working directory. At the user's request, Antigravity agents ran with `--dangerously-skip-permissions` and Codex agents with `--yolo` so their assigned work did not pause for tool approvals. After their reports and integrated tests were reviewed, the ten teammate terminals were dismissed at the user's request; the Codex Maestro terminal remains connected.

Earlier internal agents built the transcription, discovery, export, and publishing service foundations; other agents reviewed security, integration, and release. The Maestri team completed its assigned checks and fixes, and the combined frontend build and 125 Rust library tests passed with bundled FFmpeg on `PATH`. Manual desktop acceptance, live provider checks, the publishing findings, and release gates remain open.

## YouTube public-video downloader plan (2026-10-27)

**Goal:** let a user paste a public YouTube video page link, save a supported video locally, and pass the saved file through Cortelol's existing `import_media` validation and editing workflow. The first supported subset is one ordinary public video at a time. Public visibility does not imply that every video can be retrieved, that the user has editing rights, or that YouTube authorizes the retrieval method. Playlists, live streams, DRM, login-only, age-restricted, paywalled, and geographically restricted videos are outside the first subset. The existing Studio/Takeout and local-file import route stays available.

### Repository decision

| Candidate | Useful part | Integration decision |
| --- | --- | --- |
| [yt-dlp](https://github.com/yt-dlp/yt-dlp) | Mature YouTube extraction, format selection, progress, and error handling | Primary extraction candidate for an isolated prototype. Pin an exact source/runtime/EJS tuple. Its normal HTTP request path, external downloaders, and JavaScript runtime need independent containment; command flags alone do not provide it. Review bundled-binary GPL obligations before distribution. |
| [YoutubeDownloader](https://github.com/Tyrrrz/YoutubeDownloader) | Desktop workflow examples and `YoutubeExplode` with an injectable `HttpClient` | Reference for UX and a possible alternative extractor if the primary candidate fails. It would add a .NET runtime/bridge and still need the same request and process controls. Do not copy its Avalonia UI. |
| [Cobalt](https://github.com/imputnet/cobalt) | Format and processing ideas, typed API outcomes | Reference only. Its Node server/API is not a local Tauri library, hosted instances require permission for other projects, and copied API code would introduce AGPL obligations. Do not depend on a public Cobalt instance. |

### Delivery sequence and stop gates

1. **Set the product and technical boundary.** The user has chosen to pursue downloads of public YouTube videos without making YouTube policy compliance an implementation gate. Record the supported cases, data sent to YouTube, cookie/authentication policy, failure cases, distribution licenses, and user-facing limitations in the PRD, architecture, and design copy. Do not describe the method as YouTube-approved or promise that every public video works. This product choice does not remove Windows security, media integrity, or dependency-license checks.
2. **Prove Windows containment in a separate spike.** Run a fixed worker under a restricted AppContainer identity with no direct network capability, private staging ACLs, no inherited secrets or browser profile access, and a Job Object covering all descendants. Verify the effective token and OS behavior before the worker starts. If isolation setup fails, refuse to run rather than launching an ordinary process. The EJS runtime and any media tool must also be contained. Test hostile workers that try direct TCP/UDP/DNS, IPv4/IPv6/loopback/LAN, file reads outside staging, child escape, and proxy bypass on a supported Windows installation. Keep this spike outside the registered desktop download command.
3. **Build a narrow Rust network broker.** Make the broker the only path to external networking. Canonicalize a single YouTube video ID locally before I/O. The worker sends bounded, typed requests over private IPC; the broker validates method, host, port, headers, URL, response size, redirects, DNS answers, TLS hostname, and actual peer for every request. Pin one public IP per hop, disable system proxies and resolver fallback, deny private/reserved addresses and credential headers, and enforce per-job request, byte, concurrency, and time budgets. Reject unknown protocols, hosts, response types, and redirects. Do not repurpose the retired direct-file transport merely by adding YouTube hosts.
4. **Prototype the supported extractor subset.** Pin and verify yt-dlp, `yt-dlp-ejs`, a local JavaScript runtime, and FFmpeg/FFprobe versions and hashes. Disable plugins, user config, remote components, updates, browser cookies, credential collection, generic site extractors, and external network downloaders. The worker receives only a canonical video ID; no arbitrary URL, output path, shell command, or plugin path. Route metadata and media requests through the broker; reject unsupported formats or any route that cannot be mediated. Keep a second implementation candidate only if a measured failure justifies it.
5. **Persist and import safely.** Stage each job in a unique private directory; bound aggregate bytes, file count, disk use, and wall time. Validate metadata, stream/container, media duration, and FFprobe result before publication. Hash the completed file and atomically publish a unique source file plus durable receipt containing canonical video ID, retrieval method/version, timestamp, user declaration, and checksum, without storing signed media URLs, cookies, or raw headers. Preserve existing projects, sources, exports, and receipts on failure, cancellation, crash, collision, or restart. Then call the existing `import_media` path; a failed import leaves the downloaded file available for retry.
6. **Expose a typed Tauri job and localized UI only after gates 1–5.** Replace the fail-closed `download_video` stub with a new versioned YouTube-only request/result contract; remove the legacy `officialVimeoLink` field. Add paste/validate, format selection limited to tested formats, progress, cancel, retry, saved-file recovery, and specific unsupported/error states to `RemoteVideoImport`. Keep Studio/Takeout and local import visible. Translate new static copy in all ten locales and keyboard-test the workflow. Never show credentials or claim that a rights checkbox proves platform authorization.
7. **Verify before packaging.** Run unit fixtures for video-ID parsing, hostile URLs/DNS/redirects, malformed metadata, fragment/manifest escape, byte/time quotas, and atomic publication. Run native Windows adversarial tests of the worker, broker, EJS child, cancellation, crash recovery, and network denial. Exercise the supported flow with a public test video and record versions and trace results; do not use login, cookies, or access-control workarounds. Run the full Rust/frontend suite and inspect the Tauri desktop workflow inside the sandbox before building an installer. Review exact dependency licenses and notices before a release candidate. If containment fails, keep the URL command disabled and show the actionable local-import route.

**Current state:** a local test candidate now accepts one YouTube video URL, invokes a fixed yt-dlp executable and Deno sidecar, validates and saves the resulting file, and offers import, progress, cancel, and retry in the UI. The broker and AppContainer prototypes remain separate; the downloader process still has ordinary user file and network privileges. A local installer can be generated for testing, but this does not meet the security gates above for a public release.

### Implementation checkpoint (2026-10-27)

- Added the local `validate_youtube_video_url` Tauri command. It canonicalizes one supported HTTPS YouTube watch, short, or share URL to an 11-character video ID without network access. `download_video` invokes the bounded local test candidate.
- Built an offline yt-dlp metadata worker and a separate Rust broker prototype. The worker uses a single IPC request handler; the broker validates the request destination, headers, DNS answers, redirects, limits, and pinned peer. Their versioned frame adapter passed offline contract tests; neither is registered as a production transfer path.
- Ran a standalone Windows AppContainer and Job Object isolation spike. On this host it denied the tested local TCP/UDP, outside-profile file read, and process breakaway attempts, and failed closed on an injected setup failure. Broader WFP/DNS/LAN/child-process and packaged-runtime tests remain.
- The candidate uses a pinned official Windows yt-dlp executable with bundled EJS, a pinned Deno sidecar, a single progressive audio/video file, private staging, a 2 GiB/30-minute bound, and non-overwriting file and checksum receipt publication. Format coverage and quality are limited. Native containment and broker mediation are still absent; live packaged workflow acceptance is pending.

**Next security milestone:** launch the pinned extractor and EJS runtime inside the verified Windows boundary, connect every request to the Rust broker, and prove direct network denial for the real process tree before public release.
