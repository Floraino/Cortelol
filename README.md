# Cortelol

Cortelol is an open source Windows desktop app that helps podcast creators turn their own long recordings into short, reviewed videos. It brings transcription, moment discovery, clip editing, captions, vertical framing, and local export into one workspace. The source recording stays on the creator's computer and is never overwritten.

Explore the [multilingual landing page](https://floraino.github.io/Cortelol/) for an illustrated walkthrough. The public installer download remains unavailable until the Windows release checks are complete; the source code can be downloaded from this repository now.

## What it does

1. **Import a recording:** Open a local audio or video file and keep working on the same project after restarting the app.
2. **Transcribe and correct:** Choose Gemini or ElevenLabs Scribe v2, review the provider consent notice, and edit the resulting timestamped transcript.
3. **Find moments:** Use Claude, OpenAI, or DeepSeek to suggest coherent 2–5 minute excerpts from the corrected transcript. Check every suggestion against the source before accepting it.
4. **Edit clips:** Adjust the cut, captions, and 9:16 framing. Framing can change within a clip, including a fit layout for wide shots that show several participants.
5. **Export locally:** Render a reviewed MP4 with captions and optionally save an SRT subtitle file. Upload to a platform manually.

AI suggestions never publish on their own. Provider keys are stored in the operating system credential store. Transcription sends audio chunks to the chosen provider after consent; moment discovery sends bounded transcript excerpts; optional Gemini framing sends sampled video frames after separate consent. The local framing option does not upload frames. See [PRD.md](PRD.md) for the product scope and data flow.

## Current status

This is an early Windows development project, not a finished public release. Import, transcription, discovery, editing, and export are implemented, but full desktop acceptance and live provider checks are still in progress. The Windows installer is a local test candidate and is not published here. Direct TikTok and Instagram publishing requires approved developer apps and is not available in this release; reviewed local export is the publishing handoff. See [PLAN.md](PLAN.md) and [RELEASE.md](RELEASE.md) for open work and release requirements.

## Local transcription and video acquisition

The desktop Transcribe workspace offers an optional **Local transcription** section. Select an installed Python executable with `faster-whisper` and an existing multilingual CTranslate2 model directory. The app extracts audio with its FFmpeg toolchain, runs the bundled transcription script locally, saves resumable chunks and quality receipts, and shows phrases in the same correction workspace as hosted transcription. Python, `faster-whisper`, and model weights are not bundled or downloaded by the installer. The [local transcription guide](scripts/LOCAL_ASR.md) also documents the standalone `scripts/Transcribe-Video.py` command, which writes JSON, SRT, and a quality report. Accuracy varies; review text, timing, and captions against the source.

The desktop Import workspace accepts a single public YouTube video URL for a bounded local download using packaged yt-dlp and Deno executables. It also offers [YouTube Studio or Google Takeout](https://support.google.com/youtube/answer/56100) and local-file import. The download is a local test candidate: it does not support every public video, login, DRM, live streams, playlists, age or region restrictions, and it has not passed the planned Windows process and network isolation review. Do not treat the installer as a public release. Vimeo acquisition is no longer offered; previously saved media and receipts remain usable. The standalone `scripts/download_video.py` prototype is not used by the desktop app.

## Develop locally

You need Windows, the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/), Node.js with npm, Rust, and FFmpeg plus FFprobe on `PATH`. The FFmpeg build must include `libx264` and `libass` for captioned MP4 export.

```sh
npm install
npm run tauri dev
```

To run the project checks:

```sh
npm run build
npm test
```

The app saves project metadata in a local SQLite database under its application data directory. It refers to the imported recording by path, so that file must remain accessible. Do not put personal recordings or API keys in this repository. Release packaging and FFmpeg licensing details are in [RELEASE.md](RELEASE.md) and [legal/FFMPEG-WINDOWS.md](legal/FFMPEG-WINDOWS.md).

## Project documents

- [Product requirements](PRD.md)
- [Design system](DESIGN_SYSTEM.md)
- [Architecture](ARCHITECTURE.md)
- [Build plan](PLAN.md)
- [Windows release checklist](RELEASE.md)
- [Contributor instructions](AGENTS.md)

## License

Cortelol source code is licensed under [MIT](LICENSE). FFmpeg and other dependencies retain their own licenses; review the release checklist before distributing a bundled installer.
