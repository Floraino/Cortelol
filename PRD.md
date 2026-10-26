# Product Requirements Document
# 01 Product Overview
- **Product Name:** Cortelol
- **Tagline:** Turn long podcast episodes into reviewed, captioned short videos ready to publish.
- **Description:** Cortelol is an open source desktop application for independent podcasters, editors, and creators. It imports a local recording, uses Gemini to transcribe speech and a user-selected Claude or OpenAI model to propose strong moments, then helps the user edit, caption, export, and publish short clips. The creator keeps the source media and project files locally and supplies their own API credentials.

# 02 Problem
Finding compelling moments in a long recording, cutting them accurately, reframing video, correcting captions, and preparing platform-specific copy takes hours. Existing workflows scatter the transcript, editor, export settings, and publishing across several tools. AI suggestions can also invent quotes or select moments that lack context, so the creator needs a fast way to verify every proposed clip against the original recording before release.

# 03 Goal
Enable a creator to import a 60-minute podcast, review AI suggestions, and export at least three captioned vertical clips within 30 minutes of completed transcription, excluding provider processing and upload time. Treat this as a product target to validate with real users, not a guaranteed processing time.

# 04 Target Users
- Independent podcasters repurposing their own episodes for social platforms.
- Solo creators and small teams producing interview, discussion, or educational video.
- Video editors who want transcript-assisted discovery while retaining control over every cut.

# 05 Core Features (Version 1)
- **Local project and media import:** Create a project, import a local video or audio file, inspect duration and format, and keep source media on the user's machine. Audio-only sources can use a simple visual background for vertical export.
- **Gemini transcription:** Transcribe supported audio with timestamps and speaker labels where available. Show progress, estimated provider cost when available, failures, and a correction interface. Split recordings into supported chunks, preserve source offsets, and retain an editable transcript.
- **Long-form transcription option (scope extension):** Offer ElevenLabs Scribe v2 as a user-selected transcription provider for long recordings. Show a continuous, readable transcript while retaining timed phrases for clip selection and captions. Keep provider selection and consent explicit: ElevenLabs handles uploaded audio under its own retention terms, and Cortelol must explain that before sending. Preserve existing Gemini jobs and local corrections when adding the option.
- **AI moment discovery:** Send transcript excerpts and timing metadata to a user-selected Claude or OpenAI provider. Return a ranked list with start and end time, hook, rationale, and source quote. Validate proposed ranges against the transcript and let users accept, reject, or adjust them. Scores are editorial hints, never promises of virality.
- **Developed clip duration (scope clarification):** Discovery should propose coherent 2–5 minute excerpts with a setup, developed idea, and usable ending, rather than isolated 15-second hooks. The model's quote is a source anchor inside the proposed cut, not the full clip transcript. The creator can still trim or create a shorter cut manually during review. Local preview and export must support the full proposed duration.
- **DeepSeek discovery option (scope extension):** Offer DeepSeek as another user-selected text provider for moment discovery, using the same bounded corrected-transcript excerpts, validation, review controls, privacy consent, and locally stored API credentials as Claude and OpenAI. DeepSeek does not replace Gemini transcription or add automatic publishing.
- **Editorial discovery quality (scope clarification):** Rank 2–5 minute candidates by a specific, transcript-supported reason to watch or listen: a surprising claim, useful insight, clear disagreement, emotional turn, concrete story, or well-developed answer. Show the viewer promise, the evidence in the source, and why the excerpt stands alone. Do not equate controversy with quality or invent conflict, claims, or audience outcomes. The creator reviews every recommendation and can reject or change it.
- **Suggested vertical framing (scope extension):** For video clips, let the creator select local face analysis or Gemini visual analysis per project. Analyze only a small bounded sample of frames and suggest an editable 9:16 horizontal center when a subject can be identified. Local analysis is the default. Gemini analysis requires explicit consent for each request and sends only sampled frames, without audio or the full recording. Show uncertainty when faces move or multiple speakers compete for the crop. Never alter the recording or apply a framing decision to export without review.
- **Framing over time (scope extension):** Let the creator review and edit framing choices at points within a clip. A crop may follow different subjects over time; a wide shot may switch to a fit layout so participants remain visible in a 9:16 export. Suggestions remain bounded samples and require explicit application and review. Preserve existing single-position clip drafts.
- **AI-derived captions (scope clarification):** Use the selected speech AI provider's timed transcript and the creator's corrections to generate synchronized, editable subtitle phrases for each clip. Keep source timing and spoken meaning; do not invent dialogue. Any optional editorial overlay or summary text requires a separate review from faithful subtitles.
- **Clip editor:** Preview the source and selected cut, trim boundaries, choose a vertical crop or background, adjust framing, and maintain readable safe areas for overlays and platform controls. Support a straightforward 9:16 workflow first.
- **Captions:** Generate synchronized captions from the corrected transcript, offer editable text and timing, provide a small set of visual presets, and burn captions into exported video. Export subtitle files separately when useful.
- **Title and hashtag drafts:** Generate editable titles, descriptions, and platform-specific hashtag suggestions from each approved clip. Do not present invented claims, names, or quotes as facts.
- **Export and publishing:** Export MP4 clips locally. After an explicit review and publish action, support official TikTok and Instagram publishing integrations for eligible accounts and approved apps. Show account eligibility, required permissions, platform processing state, and clear errors; always offer manual export when direct publishing is unavailable. Publishing is never triggered automatically by a model response.
- **Job recovery and privacy controls:** Resume interrupted local projects, retry failed provider or render jobs, show external data sent to each provider, and allow deletion of local projects and temporary files.

# 06 Out of Scope (for now)
- Guaranteed viral performance, engagement prediction, or automated posting without user review.
- Multi-user collaboration, cloud project sync, hosted media storage, or a paid subscription service.
- A full nonlinear video editor with arbitrary tracks, effects, and complex motion graphics.
- Scraping or browser automation to bypass social platform publishing APIs, permissions, or review.
- Licensed music discovery or automatic clearance of third-party podcast footage and audio.

# 07 Success Metrics
- At least 80% of successful imports reach a usable, timestamped transcript without a manual restart in pilot sessions.
- At least 70% of pilot users export one reviewed, captioned clip from their first imported episode.
- Median hands-on time from completed transcription to three exported clips is 30 minutes or less for a 60-minute episode in pilot sessions.
- At least 90% of exported clips pass a manual spot check for caption synchronization and intended start and end boundaries.
- Publishing failures always surface a specific status and leave the local exported file available.

# 08 Initial Open Source Release Scope Decision
The initial Windows open source release uses reviewed local MP4 and SRT export as the publishing handoff. The creator uploads those files manually through the destination platform. Direct TikTok and Instagram posting remains a planned version 1 integration after a developer app has the required approval, OAuth connection, and platform capabilities; no approved app is available for the current release. The interface must not offer a live publish action without those prerequisites. The local export workflow and its review, caption, and source-preservation requirements remain in scope for release acceptance.

# 09 Public Landing Page Scope Decision
The project includes a static, multilingual public landing page hosted on GitHub Pages. It explains the local podcast-to-clip workflow with illustrative, synthetic interface scenes and links to the public source code. The page may link to GitHub Releases, but it must not offer an installer download or claim a production release until the Windows release checklist is complete and a reviewed installer is published. The site does not collect recordings, credentials, analytics, or contact information.
