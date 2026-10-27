import { useEffect, useRef, useState } from "react";
import { useTranslation } from "../../i18n";
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
  const centiseconds = Math.round(Math.max(0, seconds) * 100);
  const minutes = Math.floor(centiseconds / 6000);
  return `${String(minutes).padStart(2, "0")}:${((centiseconds % 6000) / 100).toFixed(2).padStart(5, "0")}`;
}

function durationLabel(seconds: number) {
  const whole = Math.round(seconds);
  return `${Math.floor(whole / 60)}m ${String(whole % 60).padStart(2, "0")}s`;
}

function SuggestionCard({ suggestion, onReview, onPreview, onUseSuggestion, busy, featured }: {
  suggestion: Suggestion;
  onReview: (id: number, decision: ReviewDecision, startSeconds: number, endSeconds: number) => Promise<void>;
  onPreview: (startSeconds: number, endSeconds: number) => Promise<void>;
  onUseSuggestion?: (range: { startSeconds: number; endSeconds: number }) => void;
  busy: boolean;
  featured: boolean;
}) {
  const { t } = useTranslation();
  const [start, setStart] = useState(String(suggestion.startSeconds));
  const [end, setEnd] = useState(String(suggestion.endSeconds));
  const changed = Number(start) !== suggestion.startSeconds || Number(end) !== suggestion.endSeconds;
  const valid = start.trim() !== "" && end.trim() !== "" && Number.isFinite(Number(start)) && Number.isFinite(Number(end)) && Number(start) >= 0 && Number(end) > Number(start);
  const duration = Math.max(0, suggestion.endSeconds - suggestion.startSeconds);

  return <article className={`discovery-suggestion ${suggestion.reviewDecision}${featured ? " featured" : ""}`}>
    {featured && <span className="discovery-featured-label"><Sparkles size={12} /> {t("discovery.suggested.moment")}</span>}
    <div className="discovery-suggestion-head">
      <span className="discovery-range">{clock(suggestion.startSeconds)}–{clock(suggestion.endSeconds)} · {durationLabel(duration)}</span>
      {suggestion.reviewDecision !== "pending" && <span className="discovery-decision">{suggestion.reviewDecision === "accepted" ? <Check size={13} /> : <X size={13} />}{suggestion.reviewDecision === "accepted" ?t("discovery.accepted") :t("discovery.rejected")}</span>}
    </div>
    <h4>{suggestion.hook}</h4>
    <blockquote>“{suggestion.sourceQuote}”</blockquote>
    <p>{suggestion.rationale}</p>
    {suggestion.score !== null && <small>{t("discovery.editorial.score")} {Math.round(suggestion.score * 100)} {t("discovery.100.this.is.a.subjective")}</small>}
    <div className="discovery-range-editor">
      <label>{t("discovery.start.seconds")}<input type="number" min="0" step="0.01" value={start} onChange={(event) => setStart(event.target.value)} /></label>
      <label>{t("discovery.end.seconds")}<input type="number" min="0" step="0.01" value={end} onChange={(event) => setEnd(event.target.value)} /></label>
    </div>
    <div className="discovery-suggestion-actions">
      <button type="button" className="replace-button" disabled={!valid} onClick={() => onPreview(Number(start), Number(end))}><CirclePlay size={14} /> {t("discovery.preview.in.source")}</button>
      {suggestion.reviewDecision === "accepted" && onUseSuggestion && <button type="button" className="replace-button" disabled={changed} title={changed ?t("discovery.save.the.edited.range.before") : undefined} onClick={() => onUseSuggestion({ startSeconds: suggestion.startSeconds, endSeconds: suggestion.endSeconds })}>{t("discovery.use.in.editor")}</button>}
      <button type="button" className="replace-button" disabled={busy} onClick={() => onReview(suggestion.id, "rejected", suggestion.startSeconds, suggestion.endSeconds)}><X size={14} /> {t("discovery.reject")}</button>
      <button type="button" className="primary-button" disabled={busy || !valid} onClick={() => onReview(suggestion.id, "accepted", Number(start), Number(end))}><Check size={14} /> {changed ?t("discovery.save.range.and.accept") :t("discovery.accept.moment")}</button>
    </div>
  </article>;
}

export function DiscoveryWorkspace({ mediaIdentity, mediaDurationSeconds, hasVideo, active, onUseSuggestion }: { mediaIdentity: string; mediaDurationSeconds: number; hasVideo: boolean; active: boolean; onUseSuggestion?: (range: { startSeconds: number; endSeconds: number }) => void }) {
  const { t } = useTranslation();
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

  useEffect(() => { if (!active) player.current?.pause(); }, [active]);

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
    }).catch((cause) => { if (active) setError(message(cause,t("discovery.moment.discovery.could.not.be"))); })
      .finally(() => { if (active) setLoading(false); });
    return () => { active = false; };
  }, [mediaIdentity]);

  useEffect(() => {
    if (loading) return;
    const interval = window.setInterval(() => {
      refresh().catch((cause) => setError(message(cause,t("discovery.discovery.status.could.not.be"))));
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
    } catch (cause) { setError(message(cause,t("discovery.the.api.key.could.not"))); }
    finally { setSavingKey(false); }
  }

  async function start() {
    if (!consent || !keys[provider] || !model.trim() || !status.transcriptReady) return;
    setBusy(true);
    setError(null);
    setStatus((current) => current.job ? { ...current, job: { ...current.job, state: "running", stage:t("discovery.preparing.transcript") } } : current);
    try {
      setStatus(await invoke<DiscoveryStatus>("discover_recent", { provider, model: model.trim(), consent: true }));
    } catch (cause) {
      setError(message(cause,t("discovery.moment.discovery.failed")));
      try { await refresh(); } catch { /* Preserve the actionable provider error above. */ }
    } finally { setBusy(false); }
  }

  async function cancel() {
    setCancelling(true);
    setError(null);
    try { await invoke("cancel_discovery"); await refresh(); }
    catch (cause) { setError(message(cause,t("discovery.discovery.could.not.be.cancelled"))); }
    finally { setCancelling(false); }
  }

  async function review(id: number, decision: ReviewDecision, startSeconds: number, endSeconds: number) {
    if (endSeconds > mediaDurationSeconds) { setError(t("discovery.the.range.ends.after.the")); return; }
    setReviewing(id);
    setError(null);
    try { setStatus(await invoke<DiscoveryStatus>("review_suggestion", { id, decision, startSeconds, endSeconds })); }
    catch (cause) { setError(message(cause,t("discovery.this.review.decision.could.not"))); }
    finally { setReviewing(null); }
  }

  async function preview(startSeconds: number, endSeconds: number) {
    if (!Number.isFinite(startSeconds) || !Number.isFinite(endSeconds) || startSeconds < 0 || endSeconds <= startSeconds || endSeconds > mediaDurationSeconds) {
      setError(t("discovery.choose.a.valid.source.range"));
      return;
    }
    setError(null);
    setPreviewError(false);
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
  return <section className={`discovery-workspace${job?.suggestions.length && !stale ? " has-suggestions" : ""}`} aria-labelledby="discovery-title">
    <div className="section-title"><div><span>03</span><h2 id="discovery-title">{t("discovery.find.moments")}</h2></div><small>{t("discovery.review.every.ai.suggestion")}</small></div>
    <div className="discovery-panel">
      <div className="discovery-intro"><Sparkles size={22} /><div><h3>{t("discovery.find.developed.2.5.minute")}</h3><p>{t("discovery.choose.an.ai.provider.to")}</p></div></div>
      {loading ? <p className="discovery-muted" role="status">{t("discovery.loading.discovery")}</p> : <>
        {!status.transcriptReady && <p className="discovery-muted">{t("discovery.finish.transcription.before.searching.for")}</p>}
        <div className="discovery-fields">
          <label htmlFor="discovery-provider">{t("discovery.provider")}<select id="discovery-provider" value={provider} onChange={(event) => {
            const nextProvider = event.target.value as Provider;
            modelDrafts.current[provider] = model;
            setProvider(nextProvider);
            setModel(modelDrafts.current[nextProvider] ?? (nextProvider === "deepseek" ? "deepseek-flash" : ""));
            setConsent(false);
            setKey("");
          }}><option value="claude">{t("discovery.claude")}</option><option value="openai">{t("discovery.openai")}</option><option value="deepseek">{t("discovery.deepseek")}</option></select></label>
          <label htmlFor="discovery-model">{t("discovery.model.id")}<input id="discovery-model" type="text" autoComplete="off" maxLength={100} value={model} onChange={(event) => { setModel(event.target.value); modelDrafts.current[provider] = event.target.value; }} placeholder={t("discovery.modelPrompt", { provider: providerNames[provider] })} /></label>
        </div>
        {!keys[provider] ? <form className="discovery-key-form" onSubmit={saveKey}><label htmlFor="discovery-key">{t("discovery.providerKey", { provider: providerNames[provider] })}</label><div><input id="discovery-key" type="password" autoComplete="off" value={key} onChange={(event) => setKey(event.target.value)} required /><button type="submit" className="replace-button" disabled={savingKey || !key.trim()}>{savingKey ?t("discovery.saving") :t("discovery.save.key")}</button></div><p>{t("discovery.the.key.is.kept.in")}</p></form> : <p className="discovery-key-ready"><Check size={15} /> {t("discovery.providerKeySaved", { provider: providerNames[provider] })}</p>}
        <div className="discovery-privacy"><strong>{t("discovery.before.sending.text")}</strong><p>{t("discovery.textDisclosure", { provider: providerNames[provider] })}</p></div>
        <label className="discovery-consent"><input type="checkbox" checked={consent} onChange={(event) => setConsent(event.target.checked)} /> {t("discovery.textConsent", { provider: providerNames[provider] })}</label>
        <div className="discovery-controls">
          {job?.state === "running" || busy ? <button type="button" className="replace-button" onClick={cancel} disabled={cancelling}><StopCircle size={15} /> {cancelling ?t("discovery.cancelling") :t("discovery.cancel.discovery")}</button> : <button type="button" className="primary-button" onClick={start} disabled={!status.transcriptReady || !consent || !keys[provider] || !model.trim() || sameCompleted}>{job ? <RotateCcw size={15} /> : <Sparkles size={15} />}{sameCompleted ?t("discovery.suggestions.saved") : job ?t("discovery.find.moments.again") :t("discovery.find.moments")}</button>}
          <span role="status">{job?.state === "running" || job?.state === "completed" ? job.stage : job?.state === "failed" ?t("discovery.discovery.failed") : job?.state === "cancelled" ?t("discovery.discovery.cancelled") :t("discovery.ready.when.the.transcript.is")}</span>
        </div>
        {job?.state === "running" && <div className="discovery-progress"><progress value={job.completedWindows} max={Math.max(1, job.totalWindows)} aria-label={t("discovery.discovery.progress")} /><span>{t("discovery.windowProgress", { completed: job.completedWindows, total: job.totalWindows })}</span></div>}
        {stale && <p className="discovery-stale">{t("discovery.the.transcript.or.editorial.criteria")}</p>}
        {job?.error && <p className="error" role="alert">{job.error}</p>}
        {error && error !== job?.error && <p className="error" role="alert">{error}</p>}
      </>}
    </div>
    {job && job.suggestions.length > 0 && !stale ? <div className="discovery-results"><div className="discovery-results-head"><h3>{t("discovery.suggested.moments.strongest.first")}</h3><span>{t("discovery.resultsCount", { count: job.suggestions.length, accepted: job.suggestions.filter((item) => item.reviewDecision === "accepted").length })}</span></div><div className="discovery-source-player"><div>{hasVideo ? <video ref={(element) => { player.current = element; }} src={convertFileSrc(mediaIdentity)} controls preload="metadata" aria-label={t("discovery.discovery.source.preview")} onTimeUpdate={(event) => keepWithinPreview(event.currentTarget)} onError={() => setPreviewError(true)} /> : <audio ref={(element) => { player.current = element; }} src={convertFileSrc(mediaIdentity)} controls preload="metadata" aria-label={t("discovery.discovery.source.preview")} onTimeUpdate={(event) => keepWithinPreview(event.currentTarget)} onError={() => setPreviewError(true)} />}</div><p>{previewRange ? t("discovery.previewRange", { start: clock(previewRange.start), end: clock(previewRange.end) }) :t("discovery.select.preview.in.source.on")}</p>{previewError && <p className="error" role="alert">{t("discovery.this.source.codec.cannot.be")}</p>}</div><div className="discovery-list">{[...job.suggestions].sort((a, b) => (b.score ?? -1) - (a.score ?? -1) || a.startSeconds - b.startSeconds).map((suggestion, index) => <SuggestionCard key={`${mediaIdentity}-${suggestion.id}-${suggestion.startSeconds}-${suggestion.endSeconds}`} suggestion={suggestion} featured={index === 0} busy={reviewing === suggestion.id} onReview={review} onPreview={preview} onUseSuggestion={onUseSuggestion} />)}</div></div> : job?.state === "completed" && !stale ? <p className="discovery-empty">{t("discovery.no.source.aligned.moments.were")}</p> : null}
  </section>;
}
