import { useEffect, useRef, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import { ArrowRight, Captions, Check, CirclePlay, Clock3, Copy, FileVideo2, FolderOpen, HardDrive, Minus, Scissors, Settings, Sparkles, Square, Upload, X } from "lucide-react";
import { TranscriptWorkspace } from "./features/transcript/TranscriptWorkspace";
import { DiscoveryWorkspace } from "./features/discovery/DiscoveryWorkspace";
import { ExportWorkspace } from "./features/export/ExportWorkspace";
import { PublishWorkspace } from "./features/publishing/PublishWorkspace";
import { SettingsWorkspace } from "./features/settings/SettingsWorkspace";
import "@fontsource/inter/400.css";
import "@fontsource/inter/500.css";
import "@fontsource/inter/600.css";
import "@fontsource/inter/700.css";
import "./App.css";

type MediaInfo = {
  path: string;
  fileName: string;
  durationSeconds: number;
  fileSizeBytes: number;
  hasVideo: boolean;
  hasAudio: boolean;
  width: number | null;
  height: number | null;
  audioChannels: number | null;
};

const stages = [
  { id: "import", name: "Import", icon: FolderOpen },
  { id: "transcribe", name: "Transcribe", icon: Captions },
  { id: "discover", name: "Find moments", icon: Sparkles },
  { id: "edit", name: "Edit clips", icon: Scissors },
  { id: "publish", name: "Publish", icon: Upload },
  { id: "settings", name: "Settings", icon: Settings },
] as const;

type Stage = (typeof stages)[number]["id"];

function duration(seconds: number) {
  const total = Math.round(seconds);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  return h ? `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}` : `${m}:${String(s).padStart(2, "0")}`;
}

function size(bytes: number) {
  return bytes >= 1024 ** 3 ? `${(bytes / 1024 ** 3).toFixed(2)} GB` : `${(bytes / 1024 ** 2).toFixed(1)} MB`;
}

function App() {
  const mainRef = useRef<HTMLElement | null>(null);
  const [activeStage, setActiveStage] = useState<Stage>("import");
  const [credentialRevision, setCredentialRevision] = useState(0);
  const [settingsRevision, setSettingsRevision] = useState(0);
  const [media, setMedia] = useState<MediaInfo | null>(null);
  const [selectedRange, setSelectedRange] = useState<{ startSeconds: number; endSeconds: number; version: number } | undefined>();
  const [completedExportPath, setCompletedExportPath] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [restoring, setRestoring] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    invoke<MediaInfo | null>("load_recent_media_command")
      .then((recent) => { if (active) setMedia(recent); })
      .catch((cause) => { if (active) setError(typeof cause === "string" ? cause : "The recent recording could not be restored."); })
      .finally(() => { if (active) setRestoring(false); });
    return () => { active = false; };
  }, []);

  function navigate(stage: Stage) {
    if (activeStage === "settings" && stage !== "settings") setCredentialRevision((current) => current + 1);
    if (activeStage !== "settings" && stage === "settings") setSettingsRevision((current) => current + 1);
    setActiveStage(stage);
    mainRef.current?.scrollTo({ top: 0 });
  }

  async function chooseRecording() {
    setError(null);
    try {
      const path = await open({
        multiple: false,
        directory: false,
        title: "Choose a podcast recording",
        filters: [{ name: "Video and audio", extensions: ["mp4", "mov", "mkv", "webm", "mp3", "wav", "m4a", "aac", "flac"] }],
      });
      if (!path) return;
      setBusy(true);
      setMedia(await invoke<MediaInfo>("import_media", { path }));
      setSelectedRange(undefined);
      setCompletedExportPath(null);
      navigate("transcribe");
    } catch (cause) {
      setError(typeof cause === "string" ? cause : "The recording could not be imported.");
    } finally {
      setBusy(false);
    }
  }

  return <div className="shell">
    <aside className="sidebar">
      <div className="brand"><span className="brand-icon"><Scissors size={18} /></span><strong>cortelol<span>.</span></strong></div>
      <nav aria-label="Workspace sections" className="stages">
        {stages.map((stage) => { const Icon = stage.icon; const active = activeStage === stage.id; return <button type="button" className={`stage ${active ? "active" : ""}`} key={stage.id} aria-current={active ? "page" : undefined} aria-label={stage.name} title={stage.name} onClick={() => navigate(stage.id)}><Icon size={18} strokeWidth={1.6} /><span>{stage.name}</span>{active && <i aria-hidden="true" />}</button>; })}
      </nav>
      <div className="sidebar-bottom"><div className="local-note"><HardDrive size={18} /><div><strong>Local workspace</strong><p>Your source recording stays on this device.</p></div></div><small>OPEN SOURCE · VERSION 0.1</small></div>
    </aside>

    <WindowChrome />
    <main className="main" ref={mainRef}>
      <div className="page">
        <div hidden={activeStage !== "import"}>
        <div className="eyebrow"><i /> START HERE <span>01 / 05</span></div>
        <section className="hero"><div><h1>Give your best moments<br /><em>the spotlight.</em></h1><p>Bring in a recording. Cortelol will help you find the moments worth sharing, shape them into clips, and make every word count.</p></div><div className="hero-art" aria-hidden="true"><div className="orbit outer" /><div className="orbit inner" /><div className="play-mark"><CirclePlay size={35} strokeWidth={1.7} /></div></div></section>

        <div className="section-title"><div><span>01</span><h2>Your recording</h2></div><small>Supported video and audio files</small></div>
        <section className={`import-card ${media ? "imported" : ""}`} aria-label="Import recording">
          {media ? <>
            <div className="file-icon"><FileVideo2 size={28} /></div>
            <div className="file-details"><div className="ready"><Check size={13} /> READY TO WORK WITH</div><h3>{media.fileName}</h3><p title={media.path}>{media.path}</p><div className="chips"><span><Clock3 size={14} /> {duration(media.durationSeconds)}</span><span>{media.hasVideo ? `${media.width ?? "?"} × ${media.height ?? "?"}` : "Audio only"}</span><span>{media.hasAudio ? "Audio detected" : "No audio"}</span><span>{size(media.fileSizeBytes)}</span></div></div>
            <button type="button" className="replace-button" disabled={busy || restoring} onClick={chooseRecording}>{busy ? "Reading recording…" : "Replace file"}</button>
          </> : <>
            <div className="import-icon"><FolderOpen size={29} strokeWidth={1.6} /></div><h3>Start with your source file</h3><p>Select a podcast episode, interview, or recording from your computer.</p>
            <button type="button" className="primary-button" disabled={busy || restoring} onClick={chooseRecording}>{restoring ? "Loading recent recording…" : busy ? "Reading recording…" : "Choose recording"}<ArrowRight size={16} /></button>
            <small>MP4, MOV, MKV, WEBM, MP3, WAV, M4A, AAC, FLAC</small>
          </>}
        </section>
        {error && <div className="error" role="alert">{error}</div>}
        {media && <div className="import-next"><button type="button" className="primary-button" onClick={() => navigate("transcribe")}>Continue to transcription <ArrowRight size={16} /></button></div>}
        </div>

        <div hidden={activeStage !== "transcribe"}>{media ? <TranscriptWorkspace key={`${media.path}-${credentialRevision}`} mediaIdentity={media.path} hasAudio={media.hasAudio} /> : <StageEmpty title="Transcription" message="Import a recording to transcribe its speech and review the transcript." onImport={() => navigate("import")} />}</div>
        <div hidden={activeStage !== "discover"}>{media ? <DiscoveryWorkspace key={`discovery-${media.path}-${credentialRevision}`} mediaIdentity={media.path} mediaDurationSeconds={media.durationSeconds} hasVideo={media.hasVideo} onUseSuggestion={({ startSeconds, endSeconds }) => { setSelectedRange((current) => ({ startSeconds, endSeconds, version: (current?.version ?? 0) + 1 })); navigate("edit"); }} /> : <StageEmpty title="Find moments" message="Import a recording, then transcribe it to find moments worth reviewing." onImport={() => navigate("import")} />}</div>
        <div hidden={activeStage !== "edit"}>{media ? <ExportWorkspace key={`export-${media.path}`} media={media} selectedRange={selectedRange} onCompleted={setCompletedExportPath} /> : <StageEmpty title="Edit and export" message="Import a recording to choose a clip range, edit captions, and export an MP4." onImport={() => navigate("import")} />}</div>
        <div hidden={activeStage !== "publish"}>{media && completedExportPath ? <PublishWorkspace key={completedExportPath} exportPath={completedExportPath} /> : <StageEmpty title="Share your clip" message={media ? "Export a reviewed clip first. Your MP4 and SRT will be ready for manual upload." : "Import and export a recording first. Your finished clip will be ready for manual upload."} onImport={() => navigate(media ? "edit" : "import")} actionLabel={media ? "Go to editor" : "Go to import"} />}</div>
        <div hidden={activeStage !== "settings"}><SettingsWorkspace key={settingsRevision} /></div>
      </div>
    </main>
  </div>;
}

function WindowChrome() {
  const [maximized, setMaximized] = useState(false);
  const [windowError, setWindowError] = useState<string | null>(null);
  const desktop = isTauri();

  useEffect(() => {
    if (!desktop) return;
    const appWindow = getCurrentWindow();
    let active = true;
    let unlisten: (() => void) | undefined;
    const refreshMaximized = () => {
      void appWindow.isMaximized()
        .then((value) => { if (active) setMaximized(value); })
        .catch(() => { if (active) setWindowError("Window controls are unavailable."); });
    };
    refreshMaximized();
    void appWindow.onResized(refreshMaximized)
      .then((stop) => { if (active) unlisten = stop; else stop(); })
      .catch(() => { if (active) setWindowError("Window controls are unavailable."); });
    return () => { active = false; unlisten?.(); };
  }, [desktop]);

  async function runWindowAction(action: "minimize" | "toggleMaximize" | "close") {
    if (!desktop) return;
    setWindowError(null);
    try {
      const appWindow = getCurrentWindow();
      await appWindow[action]();
      if (action === "toggleMaximize") setMaximized(await appWindow.isMaximized());
    } catch {
      setWindowError("Window action failed. Try again.");
    }
  }

  return <div className="window-chrome">
    <div className="window-drag-region" data-tauri-drag-region aria-hidden="true" />
    {windowError && <span className="window-control-error" role="alert">{windowError}</span>}
    <div className="window-controls" role="group" aria-label="Window controls">
      <button type="button" aria-label="Minimize window" title="Minimize" disabled={!desktop} onClick={() => { void runWindowAction("minimize"); }}><Minus size={16} strokeWidth={1.5} /></button>
      <button type="button" aria-label={maximized ? "Restore window" : "Maximize window"} title={maximized ? "Restore" : "Maximize"} disabled={!desktop} onClick={() => { void runWindowAction("toggleMaximize"); }}>{maximized ? <Copy size={14} strokeWidth={1.5} /> : <Square size={14} strokeWidth={1.5} />}</button>
      <button type="button" className="window-close" aria-label="Close window" title="Close" disabled={!desktop} onClick={() => { void runWindowAction("close"); }}><X size={17} strokeWidth={1.5} /></button>
    </div>
  </div>;
}

function StageEmpty({ title, message, onImport, actionLabel = "Go to import" }: { title: string; message: string; onImport: () => void; actionLabel?: string }) {
  return <section className="stage-empty" aria-label={title}><div className="import-icon"><FolderOpen size={29} strokeWidth={1.6} /></div><h1>{title}</h1><p>{message}</p><button type="button" className="primary-button" onClick={onImport}>{actionLabel}<ArrowRight size={16} /></button></section>;
}

export default App;
