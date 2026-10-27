import { useEffect, useId, useRef, useState, type FormEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import "./LocalTranscriptionSetup.css";

export type LocalAsrSetup = { pythonPath: string; modelPath: string; language: string | null };
export type LocalAsrSummary = {
  processedChunks: number;
  segmentCount: number;
  reviewSegmentCount: number;
  gapCount: number;
  detectedLanguages: string[];
};
export type LocalAsrStatus = {
  state: "idle" | "running" | "completed" | "cancelled" | "failed";
  sourcePath: string | null;
  stage: string;
  completedChunks: number;
  totalChunks: number;
  chunkProgress: number;
  summary: LocalAsrSummary;
  error: string | null;
};
export type LocalChunkReceipt = {
  schemaVersion: number;
  chunkIndex: number;
  audioStartSeconds: number;
  audioDurationSeconds: number;
  coreStartSeconds: number;
  coreEndSeconds: number;
  modelId: string;
  runtime: { engine: string; engineVersion: string; ctranslate2Version: string; pythonVersion: string } | null;
  summary: LocalAsrSummary;
  reviewRanges: { startSeconds: number; endSeconds: number; reasons: string[] }[];
  noOwnedAudio: boolean;
};
export type LocalTranscriptionReport = { chunks: LocalChunkReceipt[]; missingChunks: number[] };

// The integrating workspace can supply its catalog's translated strings. English
// remains an explicit fallback while additional locale entries are prepared.
export const localTranscriptionCopy = {
  title: "Local transcription",
  notice: "Audio and inference stay on this device. Install Python 3.10+ with faster-whisper and prepare a local CTranslate2 model first. No runtime or model is downloaded here.",
  resources: "CPU int8 mode. Larger models require more memory and disk space and may run slower than the recording. Keep space available for temporary audio. Use a multilingual model for non-English speech.",
  python: "Installed Python executable",
  model: "Existing model directory",
  choosePython: "Choose Python",
  chooseModel: "Choose model directory",
  language: "Speech language code (optional)",
  languageHint: "For example pt or en. Leave empty for detection in each chunk. This does not translate the recording.",
  check: "Check local setup",
  checking: "Checking setup…",
  ready: "Python dependency and model files found. Model compatibility will be checked during transcription.",
  start: "Transcribe locally / resume",
  busy: "Transcribing locally…",
  cancel: "Cancel local transcription",
  canceling: "Canceling…",
  review: "No ASR guarantees 100% accuracy. Review speech, names, numbers, timing, gaps and captions against the recording before using the transcript.",
  progress: "Saved chunks: {completed} / {total}",
  summary: "This run: {segments} timed phrases, {flagged} phrases flagged for review, {gaps} gaps of at least five seconds in processed audio windows.",
  summaryHint: "Flags and gaps are review prompts; gaps may be silence. Counts cover newly processed chunks only, include context windows, and are not an accuracy or completeness score. Completed chunks and corrections are preserved on retry.",
  empty: "No speech phrases were saved in this run. The source may be silent, or all chunks may already be completed. Listen to the source before continuing.",
  otherSource: "A local transcription for another recording is running.",
  failed: "Local transcription failed. Check the runtime and model, then retry to resume saved chunks.",
  unavailable: "Local transcription status is unavailable. Check that the desktop bridge is registered.",
  chooseFailed: "The selected path could not be opened.",
  checkFailed: "The local setup could not be validated.",
  cancelFailed: "Cancellation could not be requested. Try again.",
  refreshFailed: "The transcript was saved, but the workspace could not refresh. Reopen the transcript to review it.",
  completed: "Transcript saved. Review it against the recording.",
  cancelled: "Local transcription cancelled. Saved chunks remain available for retry.",
  savedReport: "Saved local quality receipts",
  savedChunk: "Chunk {chunk}: {segments} phrases; {flagged} review flags.",
  savedMissing: "Some saved local chunks have no readable quality receipt. Review their text and timing against the source.",
  reportFailed: "Saved local quality receipts could not be loaded.",
};
export type LocalTranscriptionCopy = typeof localTranscriptionCopy;
export type LocalTranscriptionSetupProps = {
  sourcePath: string;
  disabled?: boolean;
  onCompleted: (summary: LocalAsrSummary) => Promise<void> | void;
  onBusyChange?: (busy: boolean) => void;
  copy?: Partial<LocalTranscriptionCopy>;
};

function errorMessage(cause: unknown, fallback: string) {
  return typeof cause === "string" ? cause : cause instanceof Error ? cause.message : fallback;
}
function format(template: string, values: Record<string, number>) {
  return template.replace(/\{(\w+)\}/g, (placeholder, key: string) => values[key] === undefined ? placeholder : String(values[key]));
}

export function LocalTranscriptionSetup({ sourcePath, disabled = false, onCompleted, onBusyChange, copy }: LocalTranscriptionSetupProps) {
  const text = { ...localTranscriptionCopy, ...copy };
  const id = useId();
  const mounted = useRef(true);
  const busyCallback = useRef(onBusyChange);
  busyCallback.current = onBusyChange;
  const [pythonPath, setPythonPath] = useState("");
  const [modelPath, setModelPath] = useState("");
  const [language, setLanguage] = useState("");
  const [validated, setValidated] = useState(false);
  const [checking, setChecking] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [canceling, setCanceling] = useState(false);
  const [status, setStatus] = useState<LocalAsrStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pollError, setPollError] = useState(false);
  const [report, setReport] = useState<LocalTranscriptionReport | null>(null);
  const [reportError, setReportError] = useState(false);
  const busy = submitting || status?.state === "running";
  const locked = busy || checking || disabled;
  const sameSource = status?.sourcePath === sourcePath;
  const request: LocalAsrSetup = { pythonPath, modelPath, language: language.trim() || null };

  useEffect(() => {
    mounted.current = true;
    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function poll() {
      try {
        const next = await invoke<LocalAsrStatus>("get_local_transcription_status");
        if (!stopped) { setStatus(next); setPollError(false); }
      } catch { if (!stopped) setPollError(true); }
      if (!stopped) timer = setTimeout(() => { void poll(); }, 1000);
    }
    void poll();
    return () => { stopped = true; mounted.current = false; clearTimeout(timer); };
  }, []);
  useEffect(() => { busyCallback.current?.(busy); }, [busy]);
  useEffect(() => { if (!busy) setCanceling(false); }, [busy]);
  useEffect(() => {
    let stopped = false;
    setReport(null); setReportError(false);
    if (sourcePath && !busy) {
      void invoke<LocalTranscriptionReport>("get_local_transcription_report", { sourcePath }).then(
        (next) => { if (!stopped) setReport(next); },
        () => { if (!stopped) setReportError(true); },
      );
    }
    return () => { stopped = true; };
  }, [sourcePath, busy]);

  async function choose(kind: "python" | "model") {
    setError(null);
    try {
      const path = await open({ multiple: false, directory: kind === "model", title: kind === "model" ? text.chooseModel : text.choosePython });
      if (typeof path === "string" && mounted.current) {
        if (kind === "model") setModelPath(path); else setPythonPath(path);
        setValidated(false);
      }
    } catch (cause) { if (mounted.current) setError(errorMessage(cause, text.chooseFailed)); }
  }

  async function check() {
    setChecking(true); setValidated(false); setError(null);
    try {
      await invoke("validate_local_transcription_setup", { request });
      if (mounted.current) setValidated(true);
    } catch (cause) { if (mounted.current) setError(errorMessage(cause, text.checkFailed)); }
    finally { if (mounted.current) setChecking(false); }
  }

  async function start(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!validated || locked || !sourcePath) return;
    setSubmitting(true); setCanceling(false); setError(null);
    try {
      const summary = await invoke<LocalAsrSummary>("transcribe_local_recent", { request, sourcePath });
      if (mounted.current) setStatus((previous) => ({ state: "completed", sourcePath, stage: text.completed,
        completedChunks: previous?.completedChunks ?? 0, totalChunks: previous?.totalChunks ?? 0,
        chunkProgress: 0, summary, error: null }));
      try { await onCompleted(summary); }
      catch (cause) { if (mounted.current) setError(errorMessage(cause, text.refreshFailed)); }
    } catch (cause) {
      if (mounted.current) {
        setError(errorMessage(cause, text.failed));
        setStatus((previous) => previous ? { ...previous, state: "failed" } : null);
      }
    } finally {
      if (mounted.current) { setSubmitting(false); setCanceling(false); }
    }
  }

  async function cancel() {
    setCanceling(true); setError(null);
    try { await invoke("cancel_local_transcription"); }
    catch (cause) { if (mounted.current) { setError(errorMessage(cause, text.cancelFailed)); setCanceling(false); } }
  }

  const fraction = status && status.totalChunks > 0
    ? Math.min(1, (status.completedChunks + status.chunkProgress) / status.totalChunks) : undefined;
  return <section className="local-asr-setup" aria-labelledby={`${id}-title`} aria-busy={busy || checking}>
    <h3 id={`${id}-title`}>{text.title}</h3>
    <p>{text.notice}</p><p className="local-asr-hint">{text.resources}</p>
    <form onSubmit={(event) => { void start(event); }}>
      <fieldset disabled={locked}>
        <label htmlFor={`${id}-python`}>{text.python}</label>
        <div className="local-asr-path"><input id={`${id}-python`} value={pythonPath} onChange={(event) => { setPythonPath(event.target.value); setValidated(false); }} required autoComplete="off" spellCheck={false} /><button type="button" onClick={() => { void choose("python"); }}>{text.choosePython}</button></div>
        <label htmlFor={`${id}-model`}>{text.model}</label>
        <div className="local-asr-path"><input id={`${id}-model`} value={modelPath} onChange={(event) => { setModelPath(event.target.value); setValidated(false); }} required autoComplete="off" spellCheck={false} /><button type="button" onClick={() => { void choose("model"); }}>{text.chooseModel}</button></div>
        <label htmlFor={`${id}-language`}>{text.language}</label>
        <input id={`${id}-language`} className="local-asr-language" value={language} maxLength={3} pattern="[a-z]{2,3}" onChange={(event) => { setLanguage(event.target.value); setValidated(false); }} aria-describedby={`${id}-language-hint`} autoComplete="off" spellCheck={false} />
        <small id={`${id}-language-hint`}>{text.languageHint}</small>
      </fieldset>
      <p className="local-asr-review">{text.review}</p>
      <div className="local-asr-actions">
        <button type="button" onClick={() => { void check(); }} disabled={locked || !pythonPath || !modelPath}>{checking ? text.checking : text.check}</button>
        <button type="submit" className="local-asr-start" disabled={locked || !validated || !sourcePath}>{busy ? text.busy : text.start}</button>
        {busy && <button type="button" onClick={() => { void cancel(); }} disabled={canceling}>{canceling ? text.canceling : text.cancel}</button>}
      </div>
      {validated && !busy && <p role="status">{text.ready}</p>}
    </form>
    {busy && <div className="local-asr-progress">
      <p role="status">{sameSource ? status?.stage || text.busy : submitting ? text.busy : text.otherSource}</p>
      <progress max={1} value={fraction} aria-label={text.busy} />
      {status && <small>{format(text.progress, { completed: status.completedChunks, total: status.totalChunks })}</small>}
    </div>}
    {sameSource && status && !busy && status.state === "completed" && <div className="local-asr-quality" role="status">
      <p>{text.completed}</p>
      <p>{format(text.summary, { segments: status.summary.segmentCount, flagged: status.summary.reviewSegmentCount, gaps: status.summary.gapCount })}</p>
      {status.summary.segmentCount === 0 && <p>{text.empty}</p>}
      <small>{text.summaryHint}</small>
    </div>}
    {sameSource && status?.state === "cancelled" && <p role="status">{text.cancelled}</p>}
    {report && (report.chunks.length > 0 || report.missingChunks.length > 0) && <details className="local-asr-quality">
      <summary>{text.savedReport}</summary>
      {report.chunks.map((chunk) => <div key={chunk.chunkIndex}>
        <p>{format(text.savedChunk, { chunk: chunk.chunkIndex + 1, segments: chunk.summary.segmentCount, flagged: chunk.summary.reviewSegmentCount })}</p>
        <small>{chunk.modelId}{chunk.runtime ? ` · ${chunk.runtime.engine} ${chunk.runtime.engineVersion} · CTranslate2 ${chunk.runtime.ctranslate2Version} · Python ${chunk.runtime.pythonVersion}` : ""}</small>
        {chunk.reviewRanges.map((range, index) => <small key={index}>{range.startSeconds.toFixed(3)}–{range.endSeconds.toFixed(3)} s · {range.reasons.join(", ")}</small>)}
      </div>)}
      {report.missingChunks.length > 0 && <p>{text.savedMissing}</p>}
    </details>}
    {reportError && <p className="local-asr-error" role="alert">{text.reportFailed}</p>}
    {(error || sameSource && status?.error) && <p className="local-asr-error" role="alert">{error || status?.error}</p>}
    {pollError && <p className="local-asr-error" role="alert">{text.unavailable}</p>}
  </section>;
}
