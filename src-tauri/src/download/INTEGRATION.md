# YouTube acquisition integration contract

Updated 2026-10-27 for the local installer candidate. The earlier fail-closed
implementation is recorded in [YOUTUBE_FEASIBILITY.md](YOUTUBE_FEASIBILITY.md).
This candidate is **not approved for public release**: its yt-dlp subprocess has
ordinary user file and network privileges, and the separate AppContainer and Rust
broker prototypes are not connected to it.

## Desktop flow

`RemoteVideoImport` accepts one HTTPS YouTube watch, Shorts, or short link. It
calls `validate_youtube_video_url({ url })` to obtain a canonical URL and an
11-character video ID without network access. After the user's rights confirmation,
it calls `download_video({ request: { url: canonicalUrl, ownsMedia: true } })`.
The Rust command parses the ID again, starts one job, and returns a saved file and
receipt. The shell's `onDownloaded` callback calls `import_media`; if import fails,
the UI retains the saved path and offers a retry. YouTube Studio, Google Takeout,
and the existing local-file chooser remain available. Vimeo links are rejected.

The shell props are:

```tsx
<RemoteVideoImport
  disabled={busy || restoring}
  onBusyChange={setRemoteBusy}
  onImportFile={chooseRecording}
  onDownloaded={importDownloadedVideo}
/>
```

Registered Tauri commands are `validate_youtube_video_url`, `download_video`,
`get_download_status`, `cancel_download_video`, and `open_acquisition_help`.
`get_download_status` reports `idle`, `downloading`, `completed`, `canceled`, or
`failed`, downloaded bytes, optional total bytes, and an optional error. Cancel is
idempotent when no job is active. Help topics are limited to `youtubeStudio` and
`googleTakeout` and open fixed external browser URLs. The legacy
`officialVimeoLink` request field is accepted only to reject Vimeo explicitly.

The success result is:

```ts
type DownloadedVideo = {
  path: string;
  receiptPath: string;
  sha256: string;
  fileSizeBytes: number;
  sourceSite: "YouTube";
  acquisitionMethod: "yt-dlp-local-candidate";
};
```

## Worker and package

The release command resolves `yt-dlp.exe` and `deno.exe` beside `cortelol.exe`;
there is no release PATH fallback. The package pins both binaries in
`legal/downloader-win64.json`. The worker receives a canonical single-video URL
and fixed arguments, uses a private stage, and is bounded to 2 GiB and 30 minutes.
It selects one progressive MP4 or WebM file containing audio and video, so quality
and format coverage are limited. Rust probes the file, computes SHA-256, then
publishes unique non-overwriting media and receipt paths. The receipt records the
video ID, method, user confirmation, time, size, and checksum without storing a
signed media URL. The standalone `scripts/download_video.py` is not invoked.

This subprocess is **not** the brokered metadata worker in
`scripts/youtube_worker.py`. Job Object assignment follows process spawn, leaving
a race. It does not confine direct network requests or file reads. The broker and
AppContainer work in this directory are research prototypes and remain separate
release gates; see [PLAN.md](../../../PLAN.md#youtube-public-video-downloader-plan-2026-10-27).

## Verification

The local candidate passed `npm run build`, 157 Rust tests, a bounded live
yt-dlp/Deno CLI smoke on one short public video, a pre-installer Windows Sandbox
preview, and an offline Sandbox installation with the app and all four media and
download sidecars present. The CLI smoke used the backend's fixed options and
minimal environment; it did not exercise the Tauri UI command end to end. See
[RELEASE.md](../../../RELEASE.md#local-youtube-installer-candidate-2026-10-27)
for the artifact hash and remaining acceptance gates.
