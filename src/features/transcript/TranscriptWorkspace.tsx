import { useEffect, useState } from "react";
import { useTranslation } from "../../i18n";
import { invoke } from "@tauri-apps/api/core";
import { Captions, Check, RotateCcw, StopCircle } from "lucide-react";
import { LocalTranscriptionSetup, localTranscriptionCopy, type LocalTranscriptionCopy } from "./LocalTranscriptionSetup";
import "./TranscriptWorkspace.css";

export type TranscriptSegment = {
  id: number;
  startSeconds: number;
  endSeconds: number;
  text: string;
  speaker: string | null;
  corrected: boolean;
  provider: TranscriptionProvider | null;
};

export type TranscriptionStatus = {
  provider: TranscriptionProvider;
  state: "idle" | "running" | "cancelled" | "failed" | "completed";
  stage: string;
  completedChunks: number;
  totalChunks: number;
  error: string | null;
  segments: TranscriptSegment[];
};

type TranscriptionProvider = "gemini" | "elevenlabs" | "local";
type CloudTranscriptionProvider = Exclude<TranscriptionProvider, "local">;

const providerName: Record<TranscriptionProvider, string> = {
  gemini: "Gemini",
  elevenlabs: "ElevenLabs",
  local: "Local",
};

const emptyStatus: TranscriptionStatus = {
  provider: "gemini", state: "idle", stage: "", completedChunks: 0, totalChunks: 0, error: null, segments: [],
};

function message(cause: unknown, fallback: string): string {
  return typeof cause === "string" ? cause : cause instanceof Error ? cause.message : fallback;
}

function time(seconds: number): string {
  const centiseconds = Math.round(Math.max(0, seconds) * 100);
  const minutes = Math.floor(centiseconds / 6000);
  return `${String(minutes).padStart(2, "0")}:${((centiseconds % 6000) / 100).toFixed(2).padStart(5, "0")}`;
}

function SegmentEditor({ segment, onSaved }: { segment: TranscriptSegment; onSaved: () => Promise<void> }) {
  const { t } = useTranslation();
  const [text, setText] = useState(segment.text);
  const [startSeconds, setStartSeconds] = useState(String(segment.startSeconds));
  const [endSeconds, setEndSeconds] = useState(String(segment.endSeconds));
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const changed = text !== segment.text || Number(startSeconds) !== segment.startSeconds || Number(endSeconds) !== segment.endSeconds;

  async function save(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const start = Number(startSeconds);
    const end = Number(endSeconds);
    if (!text.trim() || !Number.isFinite(start) || !Number.isFinite(end) || start < 0 || end <= start) {
      setError(t("transcript.enter.spoken.text.and.a"));
      return;
    }
    setSaving(true);
    setError(null);
    try {
      await invoke("update_transcript_segment", { id: segment.id, text: text.trim(), startSeconds: start, endSeconds: end });
      await onSaved();
    } catch (cause) {
      setError(message(cause,t("transcript.this.correction.could.not.be")));
    } finally {
      setSaving(false);
    }
  }

  return <form className="transcript-row" onSubmit={save}>
    <div className="transcript-row-heading">
      <span>{segment.speaker ||t("transcript.speaker.unknown")}</span>
      <span>{time(segment.startSeconds)}–{time(segment.endSeconds)}</span>
      {segment.corrected && <span className="correction-label"><Check size={12} /> {t("transcript.corrected")}</span>}
    </div>
    <div className="transcript-fields">
      <label>{t("transcript.start.seconds")}<input autoFocus type="number" min="0" step="0.01" value={startSeconds} onChange={(event) => setStartSeconds(event.target.value)} /></label>
      <label>{t("transcript.end.seconds")}<input type="number" min="0" step="0.01" value={endSeconds} onChange={(event) => setEndSeconds(event.target.value)} /></label>
    </div>
    <label className="transcript-text-label">{t("transcript.spoken.text")}<textarea rows={2} value={text} onChange={(event) => setText(event.target.value)} /></label>
    <div className="transcript-row-actions"><button type="submit" className="replace-button" disabled={!changed || saving}>{saving ?t("transcript.saving") :t("transcript.save.correction")}</button></div>
    {error && <p className="error" role="alert">{error}</p>}
  </form>;
}

export function TranscriptWorkspace({ mediaIdentity, hasAudio }: { mediaIdentity: string; hasAudio: boolean }) {
  const { t } = useTranslation();
  const providerLabel = (value: TranscriptionProvider) => value === "local" ? t("transcript.localProviderName") : providerName[value];
  const [status, setStatus] = useState<TranscriptionStatus>(emptyStatus);
  const [loading, setLoading] = useState(true);
  const [provider, setProvider] = useState<CloudTranscriptionProvider>("gemini");
  const [keyAvailable, setKeyAvailable] = useState<Record<CloudTranscriptionProvider, boolean>>({ gemini: false, elevenlabs: false });
  const [localBusy, setLocalBusy] = useState(false);
  const [key, setKey] = useState("");
  const [savingKey, setSavingKey] = useState(false);
  const [consent, setConsent] = useState(false);
  const [actionBusy, setActionBusy] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selectedSegmentId, setSelectedSegmentId] = useState<number | null>(null);
  const [transcriptSearch, setTranscriptSearch] = useState("");

  async function refresh() {
    const current = await invoke<TranscriptionStatus>("get_transcription_status");
    setStatus(current);
    if ((current.state === "running" || current.state === "completed") && current.provider !== "local") setProvider(current.provider);
  }

  useEffect(() => {
    let active = true;
    setLoading(true);
    setConsent(false);
    setTranscriptSearch("");
    Promise.all([
      invoke<boolean>("has_api_key", { provider: "gemini" }),
      invoke<boolean>("has_api_key", { provider: "elevenlabs" }),
      invoke<TranscriptionStatus>("get_transcription_status"),
    ]).then(([geminiAvailable, elevenLabsAvailable, current]) => {
      if (active) {
        setKeyAvailable({ gemini: geminiAvailable, elevenlabs: elevenLabsAvailable });
        setStatus(current);
        if (current.provider !== "local") setProvider(current.provider);
        setSelectedSegmentId(null);
      }
    }).catch((cause) => {
      if (active) setError(message(cause,t("transcript.the.transcription.workspace.could.not")));
    }).finally(() => { if (active) setLoading(false); });
    return () => { active = false; };
  }, [mediaIdentity]);

  useEffect(() => {
    if (status.state !== "running" && !localBusy) return;
    const timer = window.setInterval(() => {
      refresh().catch((cause) => setError(message(cause,t("transcript.transcription.status.could.not.be"))));
    }, 1200);
    return () => window.clearInterval(timer);
  }, [status.state, localBusy]);

  async function saveKey(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!key.trim()) return;
    setSavingKey(true);
    setError(null);
    try {
      await invoke("save_api_key", { provider, key: key.trim() });
      setKey("");
      setKeyAvailable((current) => ({ ...current, [provider]: true }));
    } catch (cause) {
      setError(message(cause, t("transcript.saveProviderKeyError", { provider: providerName[provider] })));
    } finally {
      setSavingKey(false);
    }
  }

  async function start() {
    if (!consent || !keyAvailable[provider] || !hasAudio || localBusy) return;
    setError(null);
    setActionBusy(true);
    setStatus((current) => ({ ...current, provider, state: "running", stage:t("transcript.preparing.audio"), error: null }));
    try {
      await invoke("transcribe_recent", { consent: true, provider });
      await refresh();
    } catch (cause) {
      setError(message(cause,t("transcript.transcription.could.not.start")));
      try { await refresh(); } catch { setStatus((current) => ({ ...current, state: "failed" })); }
    } finally {
      setActionBusy(false);
    }
  }

  async function cancel() {
    setCancelling(true);
    setError(null);
    try {
      await invoke("cancel_transcription");
      await refresh();
    } catch (cause) {
      setError(message(cause,t("transcript.transcription.could.not.be.cancelled")));
    } finally {
      setCancelling(false);
    }
  }

  const progress = status.totalChunks > 0 ? Math.min(100, Math.round(status.completedChunks / status.totalChunks * 100)) : null;
  const providerLocked = localBusy || status.state === "running" || status.state === "completed";
  const selectedSegment = status.segments.find((segment) => segment.id === selectedSegmentId);
  const sourceProviders = new Set(status.segments.map((segment) => segment.provider).filter((source): source is TranscriptionProvider => source !== null));
  const mixedSources = sourceProviders.size > 1;
  const query = transcriptSearch.trim().toLocaleLowerCase();
  const visibleSegments = query ? status.segments.filter((segment) => segment.text.toLocaleLowerCase().includes(query) || segment.speaker?.toLocaleLowerCase().includes(query)) : status.segments;
  const localCopy = Object.fromEntries(Object.keys(localTranscriptionCopy).map((key) => {
    const catalogKey = `localTranscription.${key}`;
    const translated = t(catalogKey);
    return [key, translated === `⟦${catalogKey}⟧` ? localTranscriptionCopy[key as keyof LocalTranscriptionCopy] : translated];
  })) as LocalTranscriptionCopy;
  return <section className={`transcript-workspace${status.segments.length ? " has-transcript" : ""}`} aria-labelledby="transcript-title">
    <div className="section-title"><div><span>02</span><h2 id="transcript-title">{t("transcript.transcription")}</h2></div><small>{t("transcript.read.review.and.correct.the")}</small></div>
    <div className="transcript-panel">
      <div className="transcript-panel-intro"><Captions size={22} /><div><h3>{t("transcript.make.the.conversation.searchable")}</h3><p>{t("transcript.choose.a.transcription.provider.the")}</p></div></div>
      {loading ? <p className="transcript-muted" role="status">{t("transcript.loading.transcription")}</p> : <>
        <fieldset className="transcript-provider-options" disabled={providerLocked}>
          <legend>{t("transcript.transcription.provider")}</legend>
          <label><input type="radio" name="transcription-provider" value="gemini" checked={provider === "gemini"} onChange={() => { setProvider("gemini"); setConsent(false); setKey(""); setError(null); }} /> {t("transcript.gemini")}</label>
          <label><input type="radio" name="transcription-provider" value="elevenlabs" checked={provider === "elevenlabs"} onChange={() => { setProvider("elevenlabs"); setConsent(false); setKey(""); setError(null); }} /> {t("transcript.elevenlabs.scribe.v2")}</label>
        </fieldset>
        {status.state === "running" && <p className="transcript-muted">{t("transcript.activeProvider", { provider: providerLabel(status.provider) })}</p>}
        {status.completedChunks > 0 && status.state !== "completed" && status.state !== "running" && <p className="transcript-muted">{t(status.completedChunks === 1 ? "transcript.savedChunk" : "transcript.savedChunks", { count: status.completedChunks, provider: providerLabel(status.provider) })}</p>}
        {!keyAvailable[provider] && <form className="credential-form" onSubmit={saveKey}>
          <label htmlFor="transcription-key">{providerName[provider]} {t("transcript.api.key")}</label>
          <div><input id="transcription-key" type="password" autoComplete="off" value={key} onChange={(event) => setKey(event.target.value)} placeholder={t("transcript.keyPrompt", { provider: providerName[provider] })} required /><button type="submit" className="replace-button" disabled={savingKey || !key.trim()}>{savingKey ?t("transcript.saving") :t("transcript.save.key")}</button></div>
          <p>{t("transcript.the.key.is.saved.in")}</p>
        </form>}
        {keyAvailable[provider] && <p className="key-ready"><Check size={15} /> {t("transcript.providerKeySaved", { provider: providerName[provider] })}</p>}
        <div className="privacy-note"><strong>{t("transcript.before.sending.audio")}</strong><p>{t("transcript.audioDisclosure", { provider: providerName[provider] })}</p>{provider === "elevenlabs" && <p>{t("transcript.elevenlabsRetention")} <a href="https://elevenlabs.io/privacy-policy" target="_blank" rel="noreferrer">{t("transcript.privacy.policy")}</a> · <a href="https://elevenlabs.io/speech-to-text-terms" target="_blank" rel="noreferrer">{t("transcript.speech.to.text.terms")}</a> · <a href="https://elevenlabs.io/docs/eleven-api/resources/zero-retention-mode" target="_blank" rel="noreferrer">{t("transcript.zero.retention")}</a></p>}<p>{t("transcript.recoveryDisclosure", { provider: providerName[provider] })}</p></div>
        <label className="consent-row"><input type="checkbox" checked={consent} onChange={(event) => setConsent(event.target.checked)} /> {t("transcript.audioConsent", { provider: providerName[provider] })}</label>
        {!hasAudio && <p className="error" role="alert">{t("transcript.this.recording.has.no.audio")}</p>}
        <div className="transcript-controls">
          {status.state === "running" ? status.provider !== "local" && <button type="button" className="replace-button" onClick={cancel} disabled={cancelling}><StopCircle size={15} /> {cancelling ?t("transcript.cancelling") :t("transcript.cancel.transcription")}</button> : status.state !== "completed" && <button type="button" className="primary-button" onClick={start} disabled={!consent || !keyAvailable[provider] || !hasAudio || actionBusy || localBusy}>{status.state === "failed" || status.state === "cancelled" ? <RotateCcw size={15} /> : <Captions size={15} />}{status.state === "failed" || status.state === "cancelled" ?t("transcript.retry.transcription") : t("transcript.transcribeAction")}</button>}
          <span className="transcript-state" role="status">{status.state === "idle" ?t("transcript.ready.when.you.are") : status.state === "running" ? (status.stage || t("transcript.transcribingStage")) : status.state === "completed" ?t("transcript.transcription.complete") : status.state === "cancelled" ?t("transcript.transcription.cancelled") :t("transcript.transcription.failed")}</span>
        </div>
        {status.state !== "idle" && status.totalChunks > 0 && <div className="transcript-progress"><progress value={status.completedChunks} max={status.totalChunks} aria-label={t("transcript.transcription.progress")} /><span>{t("transcript.chunkProgress", { completed: status.completedChunks, total: status.totalChunks, progress: progress ?? 0 })}</span></div>}
        {status.error && <p className="error" role="alert">{status.error}</p>}
        {error && <p className="error" role="alert">{error}</p>}
      </>}
    </div>
    {!loading && <LocalTranscriptionSetup sourcePath={mediaIdentity} disabled={!hasAudio || (status.state === "running" && status.provider !== "local")} copy={localCopy} onBusyChange={(busy) => { setLocalBusy(busy); if (!busy) void refresh().catch((cause) => setError(message(cause, t("transcript.transcription.status.could.not.be")))); }} onCompleted={async () => { await refresh(); }} />}
    {status.segments.length > 0 ? <div className="transcript-results"><div className="transcript-results-heading"><div><h3>{t("transcript.transcript")}</h3><span>{t("transcript.phraseCount", { count: status.segments.length })}{mixedSources && <> · {t("transcript.sourcesMixed")}</>}</span></div><label className="transcript-search">{t("transcript.search.transcript")}<input type="search" value={transcriptSearch} onChange={(event) => setTranscriptSearch(event.target.value)} placeholder={t("transcript.find.spoken.text.or.speaker")} /></label></div>
      {selectedSegment && <div className="transcript-selected"><div className="transcript-selected-heading"><strong>{t("transcript.correct.selected.phrase")}</strong><button type="button" className="replace-button" onClick={() => { const phraseId = selectedSegment.id; setSelectedSegmentId(null); requestAnimationFrame(() => document.getElementById(`transcript-phrase-${phraseId}`)?.focus()); }}>{t("transcript.close.editor")}</button></div><SegmentEditor key={`${mediaIdentity}-${selectedSegment.id}`} segment={selectedSegment} onSaved={refresh} /></div>}
      <div className="transcript-reading" aria-label={t("transcript.transcript.text")}>{visibleSegments.map((segment) => <button id={`transcript-phrase-${segment.id}`} key={segment.id} type="button" className={`transcript-line${segment.id === selectedSegmentId ? " selected" : ""}${segment.corrected ? " corrected" : ""}`} onClick={() => setSelectedSegmentId(segment.id)} aria-label={segment.provider ? t("transcript.editPhraseWithProviderAria", { time: time(segment.startSeconds), text: segment.text, provider: providerLabel(segment.provider) }) : t("transcript.editPhraseAria", { time: time(segment.startSeconds), text: segment.text })} aria-pressed={segment.id === selectedSegmentId} title={`${time(segment.startSeconds)}–${time(segment.endSeconds)}`}><time>{time(segment.startSeconds)}</time><span className="transcript-line-content"><span className="transcript-line-text">{segment.text}</span>{segment.corrected && <span className="transcript-line-correction"><Check size={12} /> {t("transcript.corrected")}</span>}{segment.provider && <small>{t("transcript.sourceProvider", { provider: providerLabel(segment.provider) })}</small>}</span>{segment.speaker && <span className="transcript-line-speaker">{segment.speaker}</span>}</button>)}</div>
      {query && visibleSegments.length === 0 && <p className="transcript-empty">{t("transcript.no.matching.transcript.text")}</p>}
    </div> : status.state === "completed" ? <p className="transcript-empty">{t("transcript.no.spoken.segments.were.detected")}</p> : <p className="transcript-empty">{t("transcript.the.editable.transcript.will.appear")}</p>}
  </section>;
}
