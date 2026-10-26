import { useEffect, useRef, useState } from "react";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { Check, CirclePlay, RotateCcw, Sparkles, StopCircle, X } from "lucide-react";
import "./DiscoveryWorkspace.css";

type Provider = "claude" | "openai" | "deepseek";

const providerNames: Record<Provider, string> = {
  claude: "Claude",
  openai: "OpenAI",
  deepseek: "DeepSeek",
};
type ReviewDecision = "pending" | "accepted" | "rejected";

type Suggestion = {
  id: number;
  startSeconds: number;
  endSeconds: number;
  originalStartSeconds: number;
  originalEndSeconds: number;
  hook: string;
  rationale: string;
  sourceQuote: string;
  score: number | null;
  reviewDecision: ReviewDecision;
};

type DiscoveryJob = {
  state: "idle" | "running" | "completed" | "cancelled" | "failed";
  stage: string;
  error: string | null;
  provider: Provider;
  model: string;
  transcriptRevision: string;
  completedWindows: number;
  totalWindows: number;
  suggestions: Suggestion[];
};

type DiscoveryStatus = {
  transcriptReady: boolean;
  transcriptCurrent: boolean;
  job: DiscoveryJob | null;
};

const emptyStatus: DiscoveryStatus = { transcriptReady: false, transcriptCurrent: false, job: null };

function message(cause: unknown, fallback: string) {
  return typeof cause === "string" ? cause : cause instanceof Error ? cause.message : fallback;
}

function clock(seconds: number) {
  const minutes = Math.floor(seconds / 60);
  return `${String(minutes).padStart(2, "0")}:${(seconds % 60).toFixed(2).padStart(5, "0")}`;
}

function durationLabel(seconds: number) {
  const whole = Math.round(seconds);
  return `${Math.floor(whole / 60)}m ${String(whole % 60).padStart(2, "0")}s`;
}

function SuggestionCard({ suggestion, onReview, onPreview, onUseSuggestion, busy }: {
  suggestion: Suggestion;
  onReview: (id: number, decision: ReviewDecision, startSeconds: number, endSeconds: number) => Promise<void>;
  onPreview: (startSeconds: number, endSeconds: number) => Promise<void>;
  onUseSuggestion?: (range: { startSeconds: number; endSeconds: number }) => void;
  busy: boolean;
}) {
  const [start, setStart] = useState(String(suggestion.startSeconds));
  const [end, setEnd] = useState(String(suggestion.endSeconds));
  const changed = Number(start) !== suggestion.startSeconds || Number(end) !== suggestion.endSeconds;
  const valid = start.trim() !== "" && end.trim() !== "" && Number.isFinite(Number(start)) && Number.isFinite(Number(end)) && Number(start) >= 0 && Number(end) > Number(start);
  const duration = Math.max(0, suggestion.endSeconds - suggestion.startSeconds);

  return <article className={`discovery-suggestion ${suggestion.reviewDecision}`}>
    <div className="discovery-suggestion-head">
      <span className="discovery-range">{clock(suggestion.startSeconds)}–{clock(suggestion.endSeconds)} · {durationLabel(duration)}</span>
      {suggestion.reviewDecision !== "pending" && <span className="discovery-decision">{suggestion.reviewDecision === "accepted" ? <Check size={13} /> : <X size={13} />}{suggestion.reviewDecision === "accepted" ? "Accepted" : "Rejected"}</span>}
    </div>
    <h4>{suggestion.hook}</h4>
    <blockquote>“{suggestion.sourceQuote}”</blockquote>
    <p>{suggestion.rationale}</p>
    {suggestion.score !== null && <small>Editorial score: {Math.round(suggestion.score * 100)} / 100. This is a subjective hint.</small>}
    <div className="discovery-range-editor">
      <label>Start (seconds)<input type="number" min="0" step="0.01" value={start} onChange={(event) => setStart(event.target.value)} /></label>
      <label>End (seconds)<input type="number" min="0" step="0.01" value={end} onChange={(event) => setEnd(event.target.value)} /></label>
    </div>
    <div className="discovery-suggestion-actions">
      <button type="button" className="replace-button" disabled={!valid} onClick={() => onPreview(Number(start), Number(end))}><CirclePlay size={14} /> Preview in source</button>
      {suggestion.reviewDecision === "accepted" && onUseSuggestion && <button type="button" className="replace-button" onClick={() => onUseSuggestion({ startSeconds: suggestion.startSeconds, endSeconds: suggestion.endSeconds })}>Use in editor</button>}
      <button type="button" className="replace-button" disabled={busy} onClick={() => onReview(suggestion.id, "rejected", suggestion.startSeconds, suggestion.endSeconds)}><X size={14} /> Reject</button>
      <button type="button" className="primary-button" disabled={busy || !valid} onClick={() => onReview(suggestion.id, "accepted", Number(start), Number(end))}><Check size={14} /> {changed ? "Save range and accept" : "Accept moment"}</button>
    </div>
  </article>;
}

export function DiscoveryWorkspace({ mediaIdentity, mediaDurationSeconds, hasVideo, onUseSuggestion }: { mediaIdentity: string; mediaDurationSeconds: number; hasVideo: boolean; onUseSuggestion?: (range: { startSeconds: number; endSeconds: number }) => void }) {
  const [status, setStatus] = useState<DiscoveryStatus>(emptyStatus);
  const [loading, setLoading] = useState(true);
  const [provider, setProvider] = useState<Provider>("claude");
  const [model, setModel] = useState("");
  const [keys, setKeys] = useState<Record<Provider, boolean>>({ claude: false, openai: false, deepseek: false });
  const [key, setKey] = useState("");
  const [savingKey, setSavingKey] = useState(false);
  const [consent, setConsent] = useState(false);
  const [busy, setBusy] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [reviewing, setReviewing] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [previewRange, setPreviewRange] = useState<{ start: number; end: number } | null>(null);
  const [previewError, setPreviewError] = useState(false);
  const player = useRef<HTMLMediaElement | null>(null);
  const modelDrafts = useRef<Partial<Record<Provider, string>>>({});

  async function refresh() {
    setStatus(await invoke<DiscoveryStatus>("get_discovery_status"));
  }

  useEffect(() => {
    let active = true;
    setLoading(true);
    Promise.all([
      invoke<DiscoveryStatus>("get_discovery_status"),
      invoke<boolean>("has_api_key", { provider: "claude" }),
      invoke<boolean>("has_api_key", { provider: "openai" }),
      invoke<boolean>("has_api_key", { provider: "deepseek" }),
    ]).then(([current, claude, openai, deepseek]) => {
      if (active) {
        setStatus(current);
        setKeys({ claude, openai, deepseek });
        if (current.job) {
          setProvider(current.job.provider);
          setModel(current.job.model);
          modelDrafts.current[current.job.provider] = current.job.model;
        }
      }
    }).catch((cause) => { if (active) setError(message(cause, "Moment discovery could not be loaded.")); })
      .finally(() => { if (active) setLoading(false); });
    return () => { active = false; };
  }, [mediaIdentity]);

  useEffect(() => {
    if (loading) return;
    const interval = window.setInterval(() => {
      refresh().catch((cause) => setError(message(cause, "Discovery status could not be refreshed.")));
    }, status.job?.state === "running" || busy ? 1500 : 5000);
    return () => window.clearInterval(interval);
  }, [loading, status.job?.state, busy]);

  async function saveKey(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!key.trim()) return;
    setSavingKey(true);
    setError(null);
    try {
      await invoke("save_api_key", { provider, key: key.trim() });
      setKey("");
      setKeys((current) => ({ ...current, [provider]: true }));
    } catch (cause) { setError(message(cause, "The API key could not be saved.")); }
    finally { setSavingKey(false); }
  }

  async function start() {
    if (!consent || !keys[provider] || !model.trim() || !status.transcriptReady) return;
    setBusy(true);
    setError(null);
    setStatus((current) => current.job ? { ...current, job: { ...current.job, state: "running", stage: "Preparing transcript" } } : current);
    try {
      setStatus(await invoke<DiscoveryStatus>("discover_recent", { provider, model: model.trim(), consent: true }));
    } catch (cause) {
      setError(message(cause, "Moment discovery failed."));
      try { await refresh(); } catch { /* Preserve the actionable provider error above. */ }
    } finally { setBusy(false); }
  }

  async function cancel() {
    setCancelling(true);
    setError(null);
    try { await invoke("cancel_discovery"); await refresh(); }
    catch (cause) { setError(message(cause, "Discovery could not be cancelled.")); }
    finally { setCancelling(false); }
  }

  async function review(id: number, decision: ReviewDecision, startSeconds: number, endSeconds: number) {
    if (endSeconds > mediaDurationSeconds) { setError("The range ends after the recording."); return; }
    setReviewing(id);
    setError(null);
    try { setStatus(await invoke<DiscoveryStatus>("review_suggestion", { id, decision, startSeconds, endSeconds })); }
    catch (cause) { setError(message(cause, "This review decision could not be saved.")); }
    finally { setReviewing(null); }
  }

  async function preview(startSeconds: number, endSeconds: number) {
    if (!Number.isFinite(startSeconds) || !Number.isFinite(endSeconds) || startSeconds < 0 || endSeconds <= startSeconds || endSeconds > mediaDurationSeconds) {
      setError("Choose a valid source range to preview.");
      return;
    }
    setError(null);
    const element = player.current;
    if (!element) return;
    element.pause();
    element.currentTime = startSeconds;
    setPreviewRange({ start: startSeconds, end: endSeconds });
    try { await element.play(); } catch { setPreviewError(true); }
  }

  function keepWithinPreview(element: HTMLMediaElement) {
    if (previewRange && element.currentTime >= previewRange.end) element.pause();
  }

  const job = status.job;
  const stale = job && !status.transcriptCurrent;
  const sameCompleted = job?.state === "completed" && !stale && job.provider === provider && job.model === model.trim();
  return <section className="discovery-workspace" aria-labelledby="discovery-title">
    <div className="section-title"><div><span>03</span><h2 id="discovery-title">Find moments</h2></div><small>Review every AI suggestion</small></div>
    <div className="discovery-panel">
      <div className="discovery-intro"><Sparkles size={22} /><div><h3>Find developed 2–5 minute clips</h3><p>Choose an AI provider to find excerpts with a setup, developed idea, and usable ending. Each quote anchors a proposal to the transcript; review the full cut before accepting it.</p></div></div>
      {loading ? <p className="discovery-muted" role="status">Loading discovery…</p> : <>
        {!status.transcriptReady && <p className="discovery-muted">Finish transcription before searching for moments.</p>}
        <div className="discovery-fields">
          <label htmlFor="discovery-provider">Provider<select id="discovery-provider" value={provider} onChange={(event) => {
            const nextProvider = event.target.value as Provider;
            modelDrafts.current[provider] = model;
            setProvider(nextProvider);
            setModel(modelDrafts.current[nextProvider] ?? (nextProvider === "deepseek" ? "deepseek-flash" : ""));
            setConsent(false);
            setKey("");
          }}><option value="claude">Claude</option><option value="openai">OpenAI</option><option value="deepseek">DeepSeek</option></select></label>
          <label htmlFor="discovery-model">Model ID<input id="discovery-model" type="text" autoComplete="off" maxLength={100} value={model} onChange={(event) => { setModel(event.target.value); modelDrafts.current[provider] = event.target.value; }} placeholder={`Enter a ${providerNames[provider]} model ID`} /></label>
        </div>
        {!keys[provider] ? <form className="discovery-key-form" onSubmit={saveKey}><label htmlFor="discovery-key">{providerNames[provider]} API key</label><div><input id="discovery-key" type="password" autoComplete="off" value={key} onChange={(event) => setKey(event.target.value)} required /><button type="submit" className="replace-button" disabled={savingKey || !key.trim()}>{savingKey ? "Saving…" : "Save key"}</button></div><p>The key is kept in your operating system credential store.</p></form> : <p className="discovery-key-ready"><Check size={15} /> {providerNames[provider]} key saved</p>}
        <div className="discovery-privacy"><strong>Before sending text</strong><p>Cortelol sends bounded excerpts of your corrected transcript with timestamps and speaker labels to {providerNames[provider]}. No source recording is sent during discovery. Provider charges may apply; an estimate is unavailable here.</p></div>
        <label className="discovery-consent"><input type="checkbox" checked={consent} onChange={(event) => setConsent(event.target.checked)} /> I agree to send transcript excerpts to {providerNames[provider]} for moment discovery.</label>
        <div className="discovery-controls">
          {job?.state === "running" || busy ? <button type="button" className="replace-button" onClick={cancel} disabled={cancelling}><StopCircle size={15} /> {cancelling ? "Cancelling…" : "Cancel discovery"}</button> : <button type="button" className="primary-button" onClick={start} disabled={!status.transcriptReady || !consent || !keys[provider] || !model.trim() || sameCompleted}>{job ? <RotateCcw size={15} /> : <Sparkles size={15} />}{sameCompleted ? "Suggestions saved" : job ? "Find moments again" : "Find moments"}</button>}
          <span role="status">{job?.state === "running" || job?.state === "completed" ? job.stage : job?.state === "failed" ? "Discovery failed" : job?.state === "cancelled" ? "Discovery cancelled" : "Ready when the transcript is"}</span>
        </div>
        {job?.state === "running" && <div className="discovery-progress"><progress value={job.completedWindows} max={Math.max(1, job.totalWindows)} aria-label="Discovery progress" /><span>{job.completedWindows} of {job.totalWindows} transcript windows</span></div>}
        {stale && <p className="discovery-stale">The transcript or editorial criteria changed since these suggestions were generated. Run discovery again before reviewing them.</p>}
        {job?.error && <p className="error" role="alert">{job.error}</p>}
        {error && error !== job?.error && <p className="error" role="alert">{error}</p>}
      </>}
    </div>
    {job && job.suggestions.length > 0 && !stale ? <div className="discovery-results"><div className="discovery-results-head"><h3>Suggested moments · strongest first</h3><span>{job.suggestions.length} suggestions · {job.suggestions.filter((item) => item.reviewDecision === "accepted").length} accepted</span></div><div className="discovery-source-player"><div>{hasVideo ? <video ref={(element) => { player.current = element; }} src={convertFileSrc(mediaIdentity)} controls preload="metadata" onTimeUpdate={(event) => keepWithinPreview(event.currentTarget)} onError={() => setPreviewError(true)} /> : <audio ref={(element) => { player.current = element; }} src={convertFileSrc(mediaIdentity)} controls preload="metadata" onTimeUpdate={(event) => keepWithinPreview(event.currentTarget)} onError={() => setPreviewError(true)} />}</div><p>{previewRange ? `Previewing source ${clock(previewRange.start)}–${clock(previewRange.end)}. Listen to the quote before accepting.` : "Select Preview in source on a suggestion to seek to its proposed range."}</p>{previewError && <p className="error" role="alert">This source codec cannot be previewed in the desktop player.</p>}</div><div className="discovery-list">{[...job.suggestions].sort((a, b) => (b.score ?? -1) - (a.score ?? -1) || a.startSeconds - b.startSeconds).map((suggestion) => <SuggestionCard key={`${mediaIdentity}-${suggestion.id}-${suggestion.startSeconds}-${suggestion.endSeconds}`} suggestion={suggestion} busy={reviewing === suggestion.id} onReview={review} onPreview={preview} onUseSuggestion={onUseSuggestion} />)}</div></div> : job?.state === "completed" && !stale ? <p className="discovery-empty">No source-aligned moments were suggested. You can still create a clip manually.</p> : null}
  </section>;
}
