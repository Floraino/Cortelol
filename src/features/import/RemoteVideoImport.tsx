import { useEffect, useId, useRef, useState, type FormEvent, type MouseEvent } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { useTranslation } from "../../i18n";
import "./RemoteVideoImport.css";

export type DownloadedVideo = {
  path: string;
  receiptPath: string;
  sha256: string;
  fileSizeBytes: number;
  sourceSite: string;
  acquisitionMethod: string;
};

type DownloadStatus = {
  state: "idle" | "downloading" | "completed" | "canceled" | "failed";
  downloadedBytes: number;
  totalBytes: number | null;
  error: string | null;
};
type ValidatedLink = { videoId: string; canonicalUrl: string };
export type RemoteVideoImportProps = {
  onDownloaded: (video: DownloadedVideo) => Promise<void> | void;
  onImportFile: () => Promise<void> | void;
  disabled?: boolean;
  onBusyChange?: (busy: boolean) => void;
};
type Phase = "idle" | "validating" | "downloading" | "canceling" | "importing";
type HelpTopic = "youtubeStudio" | "googleTakeout";

export function RemoteVideoImport({ onDownloaded, onImportFile, disabled = false, onBusyChange }: RemoteVideoImportProps) {
  const { t, language } = useTranslation();
  const id = useId();
  const [url, setUrl] = useState("");
  const [rights, setRights] = useState(false);
  const [phase, setPhase] = useState<Phase>("idle");
  const [status, setStatus] = useState<DownloadStatus | null>(null);
  const [saved, setSaved] = useState<DownloadedVideo | null>(null);
  const [imported, setImported] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const mounted = useRef(true);
  const active = useRef(false);
  const phaseRef = useRef<Phase>("idle");
  const busyCallback = useRef(onBusyChange);
  busyCallback.current = onBusyChange;
  const message = (key: string) => t(`remoteImport.${key}`);

  function activity(next: Phase) {
    phaseRef.current = next;
    if (mounted.current) {
      setPhase(next);
      busyCallback.current?.(next !== "idle");
    }
  }
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; busyCallback.current?.(false); };
  }, []);
  useEffect(() => {
    if (phase !== "downloading" && phase !== "canceling") return;
    let stopped = false;
    const poll = async () => {
      try {
        const next = await invoke<DownloadStatus>("get_download_status");
        if (!stopped && mounted.current) setStatus(next);
      } catch {
        if (!stopped && mounted.current) setError("statusFailed");
      }
    };
    void poll();
    const timer = window.setInterval(() => { void poll(); }, 500);
    return () => { stopped = true; window.clearInterval(timer); };
  }, [phase]);

  async function chooseFile() {
    if (active.current || disabled) return;
    active.current = true; setError(null); activity("importing");
    try { await onImportFile(); }
    catch { if (mounted.current) setError("localFailed"); }
    finally { active.current = false; activity("idle"); }
  }
  async function importSaved(video: DownloadedVideo) {
    if (active.current || disabled) return;
    active.current = true; setError(null); activity("importing");
    try {
      await onDownloaded(video);
      if (mounted.current) setImported(true);
    } catch {
      if (mounted.current) setError("importFailed");
    } finally { active.current = false; activity("idle"); }
  }
  async function download(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (active.current || disabled || !rights || !url.trim()) return;
    active.current = true; setError(null); setStatus(null); activity("validating");
    try {
      const link = await invoke<ValidatedLink>("validate_youtube_video_url", { url: url.trim() });
      if (!link.canonicalUrl || !link.videoId) throw new Error("invalidLink");
      activity("downloading");
      const video = await invoke<DownloadedVideo>("download_video", { request: { url: link.canonicalUrl, ownsMedia: true } });
      if (!video.path || !video.receiptPath || !video.sha256 || video.sourceSite !== "YouTube") throw new Error("invalidResult");
      if (mounted.current) { setSaved(video); setImported(false); setStatus(null); }
      activity("importing");
      try {
        await onDownloaded(video);
        if (mounted.current) setImported(true);
      } catch {
        if (mounted.current) setError("importFailed");
      }
    } catch (reason) {
      if (mounted.current) {
        const detail = String(reason).toLowerCase();
        setError(phaseRef.current === "validating" || detail.includes("invalidlink") ? "invalidLink"
          : detail.includes("unavailable") || detail.includes("cannot be downloaded") ? "unavailable"
          : detail.includes("cancel") ? "canceled" : "failed");
      }
    } finally { active.current = false; activity("idle"); }
  }
  async function cancel() {
    if (phaseRef.current !== "downloading") return;
    activity("canceling");
    try { await invoke("cancel_download_video"); }
    catch { if (mounted.current) { setError("cancelFailed"); activity("downloading"); } }
  }
  async function openHelp(event: MouseEvent<HTMLAnchorElement>, topic: HelpTopic) {
    if (!isTauri()) return;
    event.preventDefault();
    try { await invoke("open_acquisition_help", { topic }); }
    catch { if (mounted.current) setError("openFailed"); }
  }

  const busy = phase !== "idle";
  const bytes = status?.downloadedBytes ?? 0;
  const total = status?.totalBytes ?? null;
  const percent = total && total > 0 ? Math.min(100, Math.floor(bytes / total * 100)) : null;
  return <section className="remote-import" aria-labelledby={`${id}-title`}>
    <h3 id={`${id}-title`}>{message("youtubeOnlyTitle")}</h3>
    <p>{message("intro")}</p>
    <div className="remote-import-guides"><article>
      <h4>{message("youtube")}</h4><p>{message("youtubeSteps")}</p>
      <div className="remote-import-links">
        <a href="https://studio.youtube.com/" target="_blank" rel="noopener noreferrer" onClick={(event) => { void openHelp(event, "youtubeStudio"); }}>{message("studio")}</a>
        <a href="https://takeout.google.com/" target="_blank" rel="noopener noreferrer" onClick={(event) => { void openHelp(event, "googleTakeout"); }}>{message("takeout")}</a>
      </div>
    </article></div>
    <p>{message("browser")}</p>
    <div className="remote-import-direct">
      <h4>{message("linkTitle")}</h4><p>{message("linkHint")}</p>
      <form onSubmit={(event) => { void download(event); }}>
        <label htmlFor={`${id}-url`}>{message("linkLabel")}</label>
        <input id={`${id}-url`} type="url" value={url} onChange={(event) => { setUrl(event.target.value); setError(null); }} required maxLength={2048} autoComplete="off" spellCheck={false} placeholder="https://www.youtube.com/watch?v=…" disabled={disabled || busy} aria-describedby={`${id}-hint`} />
        <small id={`${id}-hint`}>{message("privacy")}</small>
        <label className="remote-import-consent"><input type="checkbox" checked={rights} onChange={(event) => setRights(event.target.checked)} disabled={disabled || busy} />{message("rights")}</label>
        <p>{message("platformPermission")}</p>
        <div className="remote-import-actions">
          <button type="submit" className="primary-button" disabled={disabled || busy || !rights || !url.trim()}>{phase === "validating" ? message("validating") : message("download")}</button>
          {(phase === "downloading" || phase === "canceling") && <button type="button" className="replace-button" disabled={phase === "canceling"} onClick={() => { void cancel(); }}>{phase === "canceling" ? message("canceling") : message("cancel")}</button>}
          <button type="button" className="replace-button" disabled={disabled || busy} onClick={() => { void chooseFile(); }}>{t("import.choose")}</button>
        </div>
      </form>
      {(phase === "downloading" || phase === "canceling") && <div className="remote-import-progress" role="status" aria-live="polite">
        <p>{message("downloading")} {percent === null ? "" : `${percent}%`}</p>
        <progress value={percent === null ? undefined : percent} max={100} aria-label={message("downloading")} />
        {bytes > 0 && <p>{message("progressBytes").replace("{downloaded}", new Intl.NumberFormat(language).format(bytes)).replace("{total}", total ? new Intl.NumberFormat(language).format(total) : "—")}</p>}
      </div>}
      {saved && <div className="remote-import-source" role="status">
        <p>{message("saved")}</p><p>{message("savedPath")}</p><code dir="auto">{saved.path}</code>
        {!imported && <button type="button" className="replace-button" disabled={disabled || busy} onClick={() => { void importSaved(saved); }}>{message("retryImport")}</button>}
      </div>}
      {phase === "importing" && <p className="remote-import-notice" role="status">{t("import.reading")}</p>}
      {error && <p className="remote-import-error" role="alert">{message(error)}</p>}
    </div>
  </section>;
}
