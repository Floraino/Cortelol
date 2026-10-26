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

When this session has a Maestri connection, run `maestri list`, `maestri preset list`, and `maestri role list` before recruiting or changing existing terminals. Reuse connected Antigravity and Codex terminals when their roles fit. Assign these workspace-scoped responsibilities with `maestri role create` or `maestri role assign`:

| Terminal | Role | Ownership |
|---|---|---|
| Antigravity | Product and UI designer | Review flows, accessibility, and the design system; own frontend screens and visual checks |
| Codex | Media pipeline engineer | Own Rust import, FFprobe, transcript timing, and FFmpeg export services |
| Codex or available reviewer | Integration reviewer | Review provider boundaries, credential handling, publishing state, and test evidence without editing the implementer's files |

Select actual presets, model variants, and account names from `maestri preset list`; do not assume they exist. Give each terminal a disjoint file scope, require it to read the four root documents, and ask it to report completion and tests through Maestri. This assignment has not been activated because the current terminal has no `MAESTRI_PIPE` connection.

For the current session, the user approved internal agents while Maestri is unavailable. Separate agents built the transcription, discovery, export, and publishing service foundations; other agents reviewed security, integration, and release. Their modules are compiled and tested together, but the milestone acceptance checks above remain open.
