import { useEffect, useRef, useState } from "react";
import { convertFileSrc, invoke, isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import { ArrowRight, Captions, Check, CirclePlay, Clock3, Copy, FileVideo2, FolderOpen, Globe2, HardDrive, Minus, Pause, Play, Scissors, Settings, Sparkles, Square, Upload, X } from "lucide-react";
import { LanguageProvider, languages, translate, useTranslation } from "./i18n";
import { TranscriptWorkspace } from "./features/transcript/TranscriptWorkspace";
import { RemoteVideoImport, type DownloadedVideo } from "./features/import/RemoteVideoImport";
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
  { id: "import", nameKey: "nav.import", icon: FolderOpen },
  { id: "transcribe", nameKey: "nav.transcribe", icon: Captions },
  { id: "discover", nameKey: "nav.discover", icon: Sparkles },
  { id: "edit", nameKey: "nav.edit", icon: Scissors },
  { id: "publish", nameKey: "nav.publish", icon: Upload },
  { id: "settings", nameKey: "nav.settings", icon: Settings },
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

function LanguageMenu() {
  const { language, setLanguage, t } = useTranslation();
  const [open, setOpen] = useState(false);
  const container = useRef<HTMLDivElement | null>(null);
  const trigger = useRef<HTMLButtonElement | null>(null);
  const options = useRef<Array<HTMLButtonElement | null>>([]);

  useEffect(() => {
    if (!open) return;
    const closeOnOutsideClick = (event: PointerEvent) => {
      if (!container.current?.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener("pointerdown", closeOnOutsideClick);
    return () => document.removeEventListener("pointerdown", closeOnOutsideClick);
  }, [open]);

  function openMenu() {
    setOpen(true);
    requestAnimationFrame(() => options.current[languages.findIndex((item) => item.code === language)]?.focus());
  }

  function onMenuKeyDown(event: React.KeyboardEvent<HTMLDivElement>) {
    if (event.key === "Escape") { event.preventDefault(); setOpen(false); trigger.current?.focus(); return; }
    if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    const current = options.current.findIndex((option) => option === document.activeElement);
    const next = event.key === "Home" ? 0 : event.key === "End" ? languages.length - 1 : (current + (event.key === "ArrowDown" ? 1 : -1) + languages.length) % languages.length;
    options.current[next]?.focus();
  }

  return <div className="language-control" ref={container} onBlur={(event) => { if (!event.currentTarget.contains(event.relatedTarget)) setOpen(false); }}>
    <button ref={trigger} type="button" className="language-trigger" aria-label={t("language.choose")} title={t("language.choose")} aria-haspopup="menu" aria-expanded={open} onClick={() => open ? setOpen(false) : openMenu()} onKeyDown={(event) => { if (!open && (event.key === "ArrowDown" || event.key === "ArrowUp")) { event.preventDefault(); openMenu(); } }}><Globe2 size={17} strokeWidth={1.6} /></button>
    {open && <div className="language-menu" role="menu" aria-label={t("language.menu")} onKeyDown={onMenuKeyDown}>
      {languages.map((item, index) => <button key={item.code} ref={(element) => { options.current[index] = element; }} type="button" role="menuitemradio" aria-checked={item.code === language} lang={item.code} dir={item.dir} onClick={() => { setLanguage(item.code); setOpen(false); trigger.current?.focus(); }}><span>{item.label}</span>{item.code === language && <Check size={15} aria-hidden="true" />}</button>)}
    </div>}
  </div>;
}

function AppContent() {
  const { language, t } = useTranslation();
  const languageRef = useRef(language);
  languageRef.current = language;
  const mainRef = useRef<HTMLElement | null>(null);
  const [activeStage, setActiveStage] = useState<Stage>("import");
  const [credentialRevision, setCredentialRevision] = useState(0);
  const [settingsRevision, setSettingsRevision] = useState(0);
  const [media, setMedia] = useState<MediaInfo | null>(null);
  const [selectedRange, setSelectedRange] = useState<{ startSeconds: number; endSeconds: number; version: number } | undefined>();
  const [completedExportPath, setCompletedExportPath] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [downloadBusy, setDownloadBusy] = useState(false);
  const [restoring, setRestoring] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    invoke<MediaInfo | null>("load_recent_media_command")
      .then((recent) => { if (active) setMedia(recent); })
      .catch((cause) => { if (active) setError(typeof cause === "string" ? cause : translate(languageRef.current, "import.restoreError")); })
      .finally(() => { if (active) setRestoring(false); });
    return () => { active = false; };
  }, []);

  function navigate(stage: Stage) {
    if (activeStage === "settings" && stage !== "settings") setCredentialRevision((current) => current + 1);
    if (activeStage !== "settings" && stage === "settings") setSettingsRevision((current) => current + 1);
    setActiveStage(stage);
    mainRef.current?.scrollTo({ top: 0 });
    mainRef.current?.focus({ preventScroll: true });
  }

  async function chooseRecording() {
    setError(null);
    try {
      const path = await open({
        multiple: false,
        directory: false,
        title: t("import.dialogTitle"),
        filters: [{ name: t("import.dialogFilter"), extensions: ["mp4", "mov", "mkv", "webm", "mp3", "wav", "m4a", "aac", "flac"] }],
      });
      if (!path) return;
      setBusy(true);
      setMedia(await invoke<MediaInfo>("import_media", { path }));
      setSelectedRange(undefined);
      setCompletedExportPath(null);
      navigate("import");
    } catch (cause) {
      setError(typeof cause === "string" ? cause : t("import.error"));
    } finally {
      setBusy(false);
    }
  }

  async function importDownloadedVideo(video: DownloadedVideo) {
    setError(null);
    setBusy(true);
    try {
      const imported = await invoke<MediaInfo>("import_media", { path: video.path });
      setMedia(imported);
      setSelectedRange(undefined);
      setCompletedExportPath(null);
      navigate("import");
    } catch (cause) {
      setError(typeof cause === "string" ? cause : t("import.error"));
      throw cause;
    } finally {
      setBusy(false);
    }
  }

  return <div className="shell">
    <a className="skip-link" href="#main-content" onClick={(event) => { event.preventDefault(); mainRef.current?.focus(); }}>{t("shell.skip")}</a>
    <aside className="sidebar">
      <div className="brand-row"><div className="brand"><span className="brand-icon"><Scissors size={18} /></span><strong>cortelol<span>.</span></strong></div><LanguageMenu /></div>
      <nav aria-label={t("nav.sections")} className="stages">
        {stages.map((stage) => { const Icon = stage.icon; const active = activeStage === stage.id; const name = t(stage.nameKey); return <button type="button" className={`stage ${active ? "active" : ""}`} key={stage.id} aria-current={active ? "page" : undefined} aria-label={name} title={name} onClick={() => navigate(stage.id)}><Icon size={18} strokeWidth={1.6} /><span>{name}</span>{active && <i aria-hidden="true" />}</button>; })}
      </nav>
      <div className="sidebar-bottom"><div className="local-note"><HardDrive size={18} /><div><strong>{t("shell.local")}</strong><p>{t("shell.localDetail")}</p></div></div><small>{t("shell.openSource")}</small></div>
    </aside>

    <WindowChrome />
    <main className="main" id="main-content" tabIndex={-1} aria-label={t("shell.content")} ref={mainRef}>
      <header className="workspace-header"><span title={media?.path}>{t("shell.project")} / <bdi>{media ? media.fileName : t("shell.newProject")}</bdi></span><button type="button" onClick={() => navigate(activeStage === "import" ? "transcribe" : activeStage === "transcribe" ? "discover" : activeStage === "discover" ? "edit" : activeStage === "edit" ? "publish" : "import")}>{t(activeStage === "import" ? "nav.transcribe" : activeStage === "transcribe" ? "nav.discover" : activeStage === "discover" ? "shell.editClip" : activeStage === "edit" ? "shell.shareExport" : "shell.importMedia")}<ArrowRight size={13} /></button></header>
      <div className="page">
        <div hidden={activeStage !== "import"} className={media ? "import-workspace-ready" : undefined}>
        {!media && <><div className="eyebrow"><i /> {t("shell.start")} <span>01 / 05</span></div>
        <section className="hero"><div><h1>{t("shell.heroLead")}<br /><em>{t("shell.heroAccent")}</em></h1><p>{t("shell.heroBody")}</p></div><div className="hero-art" aria-hidden="true"><div className="orbit outer" /><div className="orbit inner" /><div className="play-mark"><CirclePlay size={35} strokeWidth={1.7} /></div></div></section></>}

        <div className="section-title"><div><span>01</span><h2>{t("import.recording")}</h2></div><small>{t("import.supported")}</small></div>
        <section className={`import-card ${media ? "imported" : ""}`} aria-label={t("import.region")}>
          {media ? <>
            <div className="file-icon"><FileVideo2 size={28} /></div>
            <div className="file-details"><div className="ready"><Check size={13} /> {t("import.ready")}</div><h3 dir="auto">{media.fileName}</h3><p title={media.path} dir="ltr">{media.path}</p><div className="chips"><span><Clock3 size={14} /> {duration(media.durationSeconds)}</span><span>{media.hasVideo ? `${media.width ?? "?"} × ${media.height ?? "?"}` : t("import.audioOnly")}</span><span>{media.hasAudio ? t("import.audioDetected") : t("import.noAudio")}</span><span>{size(media.fileSizeBytes)}</span></div></div>
            <button type="button" className="replace-button" disabled={busy || restoring || downloadBusy} onClick={chooseRecording}>{busy ? t("import.reading") : t("import.replace")}</button>
          </> : <>
            <div className="import-icon"><FolderOpen size={29} strokeWidth={1.6} /></div><h3>{t("import.startTitle")}</h3><p>{t("import.startBody")}</p>
            <button type="button" className="primary-button" disabled={busy || restoring || downloadBusy} onClick={chooseRecording}>{restoring ? t("import.restoring") : busy ? t("import.reading") : t("import.choose")}<ArrowRight size={16} /></button>
            <small>MP4, MOV, MKV, WEBM, MP3, WAV, M4A, AAC, FLAC</small>
          </>}
        </section>
        <RemoteVideoImport onDownloaded={importDownloadedVideo} onImportFile={chooseRecording} disabled={busy || restoring} onBusyChange={setDownloadBusy} />
        {error && <div className="error" role="alert">{error}</div>}
        {media && <SourcePreview key={media.path} media={media} active={activeStage === "import"} />}
        {media && <div className="import-next"><button type="button" className="primary-button" onClick={() => navigate("transcribe")}>{t("import.continue")} <ArrowRight size={16} /></button></div>}
        </div>

        <div hidden={activeStage !== "transcribe"}>{media ? <TranscriptWorkspace key={`${media.path}-${credentialRevision}`} mediaIdentity={media.path} hasAudio={media.hasAudio} /> : <StageEmpty title={t("empty.transcription")} message={t("empty.transcriptionBody")} onImport={() => navigate("import")} />}</div>
        <div hidden={activeStage !== "discover"}>{media ? <DiscoveryWorkspace key={`discovery-${media.path}-${credentialRevision}`} mediaIdentity={media.path} mediaDurationSeconds={media.durationSeconds} hasVideo={media.hasVideo} active={activeStage === "discover"} onUseSuggestion={({ startSeconds, endSeconds }) => { setSelectedRange((current) => ({ startSeconds, endSeconds, version: (current?.version ?? 0) + 1 })); navigate("edit"); }} /> : <StageEmpty title={t("nav.discover")} message={t("empty.discoverBody")} onImport={() => navigate("import")} />}</div>
        <div hidden={activeStage !== "edit"}>{media ? <ExportWorkspace key={`export-${media.path}`} media={media} selectedRange={selectedRange} onCompleted={setCompletedExportPath} /> : <StageEmpty title={t("empty.exportTitle")} message={t("empty.exportBody")} onImport={() => navigate("import")} />}</div>
        <div hidden={activeStage !== "publish"}>{media && completedExportPath ? <PublishWorkspace key={completedExportPath} exportPath={completedExportPath} /> : <StageEmpty title={t("empty.shareTitle")} message={t(media ? "empty.shareWithMedia" : "empty.shareWithoutMedia")} onImport={() => navigate(media ? "edit" : "import")} actionLabel={t(media ? "empty.goEditor" : "empty.goImport")} />}</div>
        <div hidden={activeStage !== "settings"}><SettingsWorkspace key={settingsRevision} /></div>
      </div>
      <footer className="workspace-footer"><span><i aria-hidden="true" /> {t("shell.localProject")}</span><span>{activeStage === "settings" ? t("nav.settings") : `${String(stages.findIndex((stage) => stage.id === activeStage) + 1).padStart(2, "0")} / 05`}</span></footer>
    </main>
  </div>;
}

function SourcePreview({ media, active }: { media: MediaInfo; active: boolean }) {
  const { t } = useTranslation();
  const player = useRef<HTMLMediaElement | null>(null);
  const [position, setPosition] = useState(0);
  const [playing, setPlaying] = useState(false);
  const [previewError, setPreviewError] = useState(false);
  const durationSeconds = Math.max(0, media.durationSeconds);

  useEffect(() => { if (!active) player.current?.pause(); }, [active]);

  function seek(next: number) {
    const target = Math.max(0, Math.min(durationSeconds, next));
    if (player.current) player.current.currentTime = target;
    setPosition(target);
  }

  async function togglePlayback() {
    if (!player.current) return;
    if (!player.current.paused) { player.current.pause(); return; }
    try { await player.current.play(); setPreviewError(false); }
    catch { setPreviewError(true); }
  }

  const playerProps = {
    src: convertFileSrc(media.path),
    controls: true,
    preload: "metadata" as const,
    onTimeUpdate: (event: React.SyntheticEvent<HTMLMediaElement>) => setPosition(event.currentTarget.currentTime),
    onPlay: () => setPlaying(true),
    onPause: () => setPlaying(false),
    onError: () => setPreviewError(true),
  };

  return <section className="source-preview" aria-label={t("preview.region")}>
    <div className="source-preview-heading"><div><strong>{t("preview.title")}</strong><span>{t("preview.body")}</span></div><span>{media.hasVideo ? `${media.width ?? "?"} × ${media.height ?? "?"} · ${t("preview.video")}` : t("preview.audio")}</span></div>
    <div className="source-preview-body">
      {media.hasVideo ? <video {...playerProps} ref={(element) => { player.current = element; }} playsInline aria-label={t("preview.of", { name: media.fileName })} /> : <div className="source-audio"><CirclePlay size={38} strokeWidth={1.4} /><audio {...playerProps} ref={(element) => { player.current = element; }} aria-label={t("preview.of", { name: media.fileName })} /></div>}
    </div>
    {previewError && <p className="source-preview-error" role="alert">{t("preview.error")}</p>}
    <div className="source-timeline"><button type="button" className="source-play" aria-label={playing ? t("preview.pause") : t("preview.play")} title={playing ? t("preview.pause") : t("preview.play")} onClick={() => { void togglePlayback(); }}>{playing ? <Pause size={15} /> : <Play size={15} />}</button><label htmlFor="source-position">{t("preview.timeline")}</label><input id="source-position" type="range" min={0} max={durationSeconds || 1} step="0.1" value={Math.min(position, durationSeconds || 1)} onChange={(event) => seek(Number(event.target.value))} style={{ background: `linear-gradient(to right, var(--accent) 0%, var(--accent) ${durationSeconds ? position / durationSeconds * 100 : 0}%, #7aa7ff66 ${durationSeconds ? position / durationSeconds * 100 : 0}%, #7aa7ff66 100%)` }} /><output htmlFor="source-position" dir="ltr">{duration(position)} / {duration(durationSeconds)}</output></div>
  </section>;
}

function WindowChrome() {
  const { t } = useTranslation();
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
        .catch(() => { if (active) setWindowError(t("window.unavailable")); });
    };
    refreshMaximized();
    void appWindow.onResized(refreshMaximized)
      .then((stop) => { if (active) unlisten = stop; else stop(); })
      .catch(() => { if (active) setWindowError(t("window.unavailable")); });
    return () => { active = false; unlisten?.(); };
  }, [desktop, t]);

  async function runWindowAction(action: "minimize" | "toggleMaximize" | "close") {
    if (!desktop) return;
    setWindowError(null);
    try {
      const appWindow = getCurrentWindow();
      await appWindow[action]();
      if (action === "toggleMaximize") setMaximized(await appWindow.isMaximized());
    } catch {
      setWindowError(t("window.failed"));
    }
  }

  return <div className="window-chrome">
    <div className="window-drag-region" data-tauri-drag-region aria-hidden="true" />
    {windowError && <span className="window-control-error" role="alert">{windowError}</span>}
    <div className="window-controls" role="group" aria-label={t("window.controls")}>
      <button type="button" aria-label={t("window.minimize")} title={t("window.minimizeShort")} disabled={!desktop} onClick={() => { void runWindowAction("minimize"); }}><Minus size={16} strokeWidth={1.5} /></button>
      <button type="button" aria-label={maximized ? t("window.restore") : t("window.maximize")} title={maximized ? t("window.restoreShort") : t("window.maximizeShort")} disabled={!desktop} onClick={() => { void runWindowAction("toggleMaximize"); }}>{maximized ? <Copy size={14} strokeWidth={1.5} /> : <Square size={14} strokeWidth={1.5} />}</button>
      <button type="button" className="window-close" aria-label={t("window.close")} title={t("window.closeShort")} disabled={!desktop} onClick={() => { void runWindowAction("close"); }}><X size={17} strokeWidth={1.5} /></button>
    </div>
  </div>;
}

function StageEmpty({ title, message, onImport, actionLabel }: { title: string; message: string; onImport: () => void; actionLabel?: string }) {
  const { t } = useTranslation();
  return <section className="stage-empty" aria-label={title}><div className="import-icon"><FolderOpen size={29} strokeWidth={1.6} /></div><h1>{title}</h1><p>{message}</p><button type="button" className="primary-button" onClick={onImport}>{actionLabel ?? t("empty.goImport")}<ArrowRight size={16} /></button></section>;
}

function App() {
  return <LanguageProvider><AppContent /></LanguageProvider>;
}

export default App;
