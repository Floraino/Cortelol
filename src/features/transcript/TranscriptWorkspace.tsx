import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Captions, Check, RotateCcw, StopCircle } from "lucide-react";

export type TranscriptSegment = {
  id: number;
  startSeconds: number;
  endSeconds: number;
  text: string;
  speaker: string | null;
  corrected: boolean;
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

type TranscriptionProvider = "gemini" | "elevenlabs";

const providerName: Record<TranscriptionProvider, string> = {
  gemini: "Gemini",
  elevenlabs: "ElevenLabs",
};

const emptyStatus: TranscriptionStatus = {
  provider: "gemini", state: "idle", stage: "", completedChunks: 0, totalChunks: 0, error: null, segments: [],
};

function message(cause: unknown, fallback: string): string {
  return typeof cause === "string" ? cause : cause instanceof Error ? cause.message : fallback;
}

function time(seconds: number): string {
  const minutes = Math.floor(seconds / 60);
  return `${String(minutes).padStart(2, "0")}:${(seconds % 60).toFixed(2).padStart(5, "0")}`;
}

function groupSegments(segments: TranscriptSegment[]): TranscriptSegment[][] {
  const groups: TranscriptSegment[][] = [];
  for (const segment of segments) {
    const currentGroup = groups[groups.length - 1];
    const previous = currentGroup?.[currentGroup.length - 1];
    if (!previous || segment.speaker !== previous.speaker || segment.startSeconds - previous.endSeconds > 8 || currentGroup.length >= 24) {
      groups.push([segment]);
    } else {
      currentGroup.push(segment);
    }
  }
  return groups;
}

function SegmentEditor({ segment, onSaved }: { segment: TranscriptSegment; onSaved: () => Promise<void> }) {
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
      setError("Enter spoken text and a valid start and end time.");
      return;
    }
    setSaving(true);
    setError(null);
    try {
      await invoke("update_transcript_segment", { id: segment.id, text: text.trim(), startSeconds: start, endSeconds: end });
      await onSaved();
    } catch (cause) {
      setError(message(cause, "This correction could not be saved."));
    } finally {
      setSaving(false);
    }
  }

  return <form className="transcript-row" onSubmit={save}>
    <div className="transcript-row-heading">
      <span>{segment.speaker || "Speaker unknown"}</span>
      <span>{time(segment.startSeconds)}–{time(segment.endSeconds)}</span>
      {segment.corrected && <span className="correction-label"><Check size={12} /> Corrected</span>}
    </div>
    <div className="transcript-fields">
      <label>Start (seconds)<input type="number" min="0" step="0.01" value={startSeconds} onChange={(event) => setStartSeconds(event.target.value)} /></label>
      <label>End (seconds)<input type="number" min="0" step="0.01" value={endSeconds} onChange={(event) => setEndSeconds(event.target.value)} /></label>
    </div>
    <label className="transcript-text-label">Spoken text<textarea rows={2} value={text} onChange={(event) => setText(event.target.value)} /></label>
    <div className="transcript-row-actions"><button type="submit" className="replace-button" disabled={!changed || saving}>{saving ? "Saving…" : "Save correction"}</button></div>
    {error && <p className="error" role="alert">{error}</p>}
  </form>;
}

export function TranscriptWorkspace({ mediaIdentity, hasAudio }: { mediaIdentity: string; hasAudio: boolean }) {
  const [status, setStatus] = useState<TranscriptionStatus>(emptyStatus);
  const [loading, setLoading] = useState(true);
  const [provider, setProvider] = useState<TranscriptionProvider>("gemini");
  const [keyAvailable, setKeyAvailable] = useState<Record<TranscriptionProvider, boolean>>({ gemini: false, elevenlabs: false });
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
    if (current.state === "running" || current.state === "completed") setProvider(current.provider);
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
        setProvider(current.provider);
        setSelectedSegmentId(null);
      }
    }).catch((cause) => {
      if (active) setError(message(cause, "The transcription workspace could not be loaded."));
    }).finally(() => { if (active) setLoading(false); });
    return () => { active = false; };
  }, [mediaIdentity]);

  useEffect(() => {
    if (status.state !== "running") return;
    const timer = window.setInterval(() => {
      refresh().catch((cause) => setError(message(cause, "Transcription status could not be refreshed.")));
    }, 1200);
    return () => window.clearInterval(timer);
  }, [status.state]);

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
      setError(message(cause, `The ${providerName[provider]} key could not be saved.`));
    } finally {
      setSavingKey(false);
    }
  }

  async function start() {
    if (!consent || !keyAvailable[provider] || !hasAudio) return;
    setError(null);
    setActionBusy(true);
    setStatus((current) => ({ ...current, provider, state: "running", stage: "Preparing audio", error: null }));
    try {
      await invoke("transcribe_recent", { consent: true, provider });
      await refresh();
    } catch (cause) {
      setError(message(cause, "Transcription could not start."));
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
      setError(message(cause, "Transcription could not be cancelled."));
    } finally {
      setCancelling(false);
    }
  }

  const progress = status.totalChunks > 0 ? Math.min(100, Math.round(status.completedChunks / status.totalChunks * 100)) : null;
  const providerLocked = status.state === "running" || status.state === "completed";
  const selectedSegment = status.segments.find((segment) => segment.id === selectedSegmentId);
  const query = transcriptSearch.trim().toLocaleLowerCase();
  const visibleSegments = query ? status.segments.filter((segment) => segment.text.toLocaleLowerCase().includes(query) || segment.speaker?.toLocaleLowerCase().includes(query)) : status.segments;
  const transcriptGroups = groupSegments(visibleSegments);
  return <section className="transcript-workspace" aria-labelledby="transcript-title">
    <div className="section-title"><div><span>02</span><h2 id="transcript-title">Transcription</h2></div><small>Read, review, and correct the conversation</small></div>
    <div className="transcript-panel">
      <div className="transcript-panel-intro"><Captions size={22} /><div><h3>Make the conversation searchable</h3><p>Choose a transcription provider. The result is readable text with source timestamps and speaker labels when available.</p></div></div>
      {loading ? <p className="transcript-muted" role="status">Loading transcription…</p> : <>
        <fieldset className="transcript-provider-options" disabled={providerLocked}>
          <legend>Transcription provider</legend>
          <label><input type="radio" name="transcription-provider" value="gemini" checked={provider === "gemini"} onChange={() => { setProvider("gemini"); setConsent(false); setKey(""); setError(null); }} /> Gemini</label>
          <label><input type="radio" name="transcription-provider" value="elevenlabs" checked={provider === "elevenlabs"} onChange={() => { setProvider("elevenlabs"); setConsent(false); setKey(""); setError(null); }} /> ElevenLabs Scribe v2</label>
        </fieldset>
        {status.state === "running" && <p className="transcript-muted">Transcribing with {providerName[status.provider]}. Provider selection is unavailable while this job runs.</p>}
        {status.completedChunks > 0 && status.state !== "completed" && status.state !== "running" && <p className="transcript-muted">The {status.completedChunks} completed {status.completedChunks === 1 ? "chunk stays" : "chunks stay"} saved. Remaining chunks will use {providerName[provider]} after you review the audio sharing notice and start the job.</p>}
        {!keyAvailable[provider] && <form className="credential-form" onSubmit={saveKey}>
          <label htmlFor="transcription-key">{providerName[provider]} API key</label>
          <div><input id="transcription-key" type="password" autoComplete="off" value={key} onChange={(event) => setKey(event.target.value)} placeholder={`Enter your ${providerName[provider]} API key`} required /><button type="submit" className="replace-button" disabled={savingKey || !key.trim()}>{savingKey ? "Saving…" : "Save key"}</button></div>
          <p>The key is saved in your operating system credential store.</p>
        </form>}
        {keyAvailable[provider] && <p className="key-ready"><Check size={15} /> {providerName[provider]} key saved</p>}
        <div className="privacy-note"><strong>Before sending audio</strong><p>Cortelol extracts audio chunks of up to 15 minutes and sends them to {providerName[provider]} for transcription. Your source recording stays on this device. {provider === "elevenlabs" && <>ElevenLabs may retain uploaded audio and transcripts under its <a href="https://elevenlabs.io/privacy-policy" target="_blank" rel="noreferrer">privacy policy</a> and <a href="https://elevenlabs.io/speech-to-text-terms" target="_blank" rel="noreferrer">Speech to Text terms</a>. Cortelol does not enable <a href="https://elevenlabs.io/docs/eleven-api/resources/zero-retention-mode" target="_blank" rel="noreferrer">zero retention</a>. </>} If {providerName[provider]} returns invalid timestamps, Cortelol may send two shorter recovery chunks once. Provider charges may apply; an estimate is unavailable here.</p></div>
        <label className="consent-row"><input type="checkbox" checked={consent} onChange={(event) => setConsent(event.target.checked)} /> I agree to send extracted audio chunks to {providerName[provider]} for this transcription.</label>
        {!hasAudio && <p className="error" role="alert">This recording has no audio stream to transcribe.</p>}
        <div className="transcript-controls">
          {status.state === "running" ? <button type="button" className="replace-button" onClick={cancel} disabled={cancelling}><StopCircle size={15} /> {cancelling ? "Cancelling…" : "Cancel transcription"}</button> : status.state !== "completed" && <button type="button" className="primary-button" onClick={start} disabled={!consent || !keyAvailable[provider] || !hasAudio || actionBusy}>{status.state === "failed" || status.state === "cancelled" ? <RotateCcw size={15} /> : <Captions size={15} />}{status.state === "failed" || status.state === "cancelled" ? "Retry transcription" : "Transcribe"}</button>}
          <span className="transcript-state" role="status">{status.state === "idle" ? "Ready when you are" : status.state === "running" ? (status.stage || "Transcribing") : status.state === "completed" ? "Transcription complete" : status.state === "cancelled" ? "Transcription cancelled" : "Transcription failed"}</span>
        </div>
        {status.state !== "idle" && status.totalChunks > 0 && <div className="transcript-progress"><progress value={status.completedChunks} max={status.totalChunks} aria-label="Transcription progress" /><span>{`${status.completedChunks} of ${status.totalChunks} chunks · ${progress}%`}</span></div>}
        {status.error && <p className="error" role="alert">{status.error}</p>}
        {error && <p className="error" role="alert">{error}</p>}
      </>}
    </div>
    {status.segments.length > 0 ? <div className="transcript-results"><div className="transcript-results-heading"><div><h3>Transcript</h3><span>{status.segments.length} timed phrases · Select a phrase to correct it</span></div><label className="transcript-search">Search transcript<input type="search" value={transcriptSearch} onChange={(event) => setTranscriptSearch(event.target.value)} placeholder="Find spoken text or speaker" /></label></div>
      {selectedSegment && <div className="transcript-selected"><div className="transcript-selected-heading"><strong>Correct selected phrase</strong><button type="button" className="replace-button" onClick={() => setSelectedSegmentId(null)}>Close editor</button></div><SegmentEditor key={`${mediaIdentity}-${selectedSegment.id}`} segment={selectedSegment} onSaved={refresh} /></div>}
      <div className="transcript-reading" aria-label="Transcript text">{transcriptGroups.map((group) => <div className="transcript-paragraph" key={group[0].id}><div className="transcript-paragraph-heading"><span>{group[0].speaker || "Speaker unknown"}</span><time>{time(group[0].startSeconds)}</time></div><p>{group.map((segment) => <button key={segment.id} type="button" className={`transcript-phrase${segment.id === selectedSegmentId ? " selected" : ""}${segment.corrected ? " corrected" : ""}`} onClick={() => setSelectedSegmentId(segment.id)} aria-label={`Edit phrase at ${time(segment.startSeconds)}: ${segment.text}`} title={`${time(segment.startSeconds)}–${time(segment.endSeconds)}`}>{segment.text}</button>)}</p></div>)}</div>
      {query && visibleSegments.length === 0 && <p className="transcript-empty">No matching transcript text.</p>}
    </div> : status.state === "completed" ? <p className="transcript-empty">No spoken segments were detected in this recording.</p> : <p className="transcript-empty">The editable transcript will appear here after transcription.</p>}
  </section>;
}
