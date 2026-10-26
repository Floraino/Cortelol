import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { save } from "@tauri-apps/plugin-dialog";
import { Captions, CirclePlay, FileDown, Pause, Plus, RotateCcw, Scissors, Trash2 } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import "./ExportWorkspace.css";

type Media = { path: string; fileName: string; durationSeconds: number; hasVideo: boolean };
type StoredSegment = { id: number; startSeconds: number; endSeconds: number; text: string; speaker: string | null };
type TranscriptStatus = { segments: StoredSegment[] };
type Caption = { startMs: number; endMs: number; text: string };
type FramingKeyframe = { timestampMs: number; mode: "crop" | "fit"; horizontalPercent: number };
type ClipDraft = { startMs: number; endMs: number; framingMode: "crop" | "fit"; horizontalPercent: number; framingKeyframes: FramingKeyframe[]; captions: Caption[]; reviewed: boolean };
type FramingAnalysisMode = "local" | "gemini";
type FramingSuggestion = { horizontalPercent: number; keyframes: FramingKeyframe[]; confidence: "none" | "low" | "medium" | "high"; reason: "noFace" | "stableFace" | "movingFace" | "multipleFaces" | "visualSubject"; faceSamples: number; sampleCount: number };
type ExportStatus = {
  state: "idle" | "running" | "completed" | "cancelled" | "failed";
  stage: string;
  error: string | null;
  outputPath: string | null;
};
type Props = { media: Media; selectedRange?: { startSeconds: number; endSeconds: number; version: number }; onCompleted?: (outputPath: string | null) => void };

function time(seconds: number) {
  const whole = Math.floor(Math.max(0, seconds));
  return `${Math.floor(whole / 60)}:${String(whole % 60).padStart(2, "0")}`;
}

function errorText(cause: unknown) {
  return typeof cause === "string" ? cause : "The export could not be completed.";
}

function framingAt(keyframes: FramingKeyframe[], timestampMs: number, mode: FramingKeyframe["mode"], horizontalPercent: number) {
  return keyframes.reduce<Pick<FramingKeyframe, "mode" | "horizontalPercent">>(
    (active, keyframe) => keyframe.timestampMs <= timestampMs ? keyframe : active,
    { mode, horizontalPercent },
  );
}

function rebaseKeyframes(keyframes: FramingKeyframe[], startMs: number, endMs: number, mode: FramingKeyframe["mode"], horizontalPercent: number) {
  if (!keyframes.length || endMs <= startMs) return keyframes;
  const first = { timestampMs: startMs, ...framingAt(keyframes, startMs, mode, horizontalPercent) };
  return [first, ...keyframes.filter((keyframe) => keyframe.timestampMs > startMs && keyframe.timestampMs < endMs)];
}

// Older projects may contain one transcript segment per word. Build caption phrases
// only from those segments; existing timed phrases and saved draft cues stay intact.
function captionsFromTranscript(segments: StoredSegment[]): Caption[] {
  const cues: Caption[] = [];
  let active: { cue: Caption; speaker: string | null; words: number } | null = null;
  const flush = () => {
    if (active) cues.push(active.cue);
    active = null;
  };

  for (const segment of segments) {
    const cue = {
      startMs: Math.round(segment.startSeconds * 1000),
      endMs: Math.round(segment.endSeconds * 1000),
      text: segment.text,
    };
    const word = segment.text.trim();
    if (!/^\S+$/u.test(word) || cue.endMs <= cue.startMs) {
      flush();
      cues.push(cue);
      continue;
    }
    const previous = active?.cue;
    const gapMs = previous ? cue.startMs - previous.endMs : 0;
    if (!active || segment.speaker !== active.speaker || gapMs > 700 || gapMs < -150
      || active.words >= 8 || cue.endMs - active.cue.startMs > 4500
      || active.cue.text.length + word.length + 1 > 72
      || /[.!?…]["'”’)]?$/u.test(active.cue.text)) {
      flush();
      active = { cue: { ...cue, text: word }, speaker: segment.speaker, words: 1 };
    } else {
      active.cue.endMs = Math.max(active.cue.endMs, cue.endMs);
      active.cue.text += /^[,.;:!?…]/u.test(word) ? word : ` ${word}`;
      active.words += 1;
    }
  }
  flush();
  return cues;
}

export function ExportWorkspace({ media, selectedRange, onCompleted }: Props) {
  const [start, setStart] = useState("0");
  const [end, setEnd] = useState(String(Math.min(180, Math.floor(media.durationSeconds))));
  const [framingMode, setFramingMode] = useState<"crop" | "fit">("crop");
  const [horizontalPercent, setHorizontalPercent] = useState(50);
  const [framingKeyframes, setFramingKeyframes] = useState<FramingKeyframe[]>([]);
  const [analysisMode, setAnalysisMode] = useState<FramingAnalysisMode>("local");
  const [framingSuggestion, setFramingSuggestion] = useState<FramingSuggestion | null>(null);
  const [suggestingFraming, setSuggestingFraming] = useState(false);
  const [cloudFrameConsent, setCloudFrameConsent] = useState(false);
  const [captions, setCaptions] = useState<Caption[]>([]);
  const [captionsTouched, setCaptionsTouched] = useState(false);
  const [generatingCaptions, setGeneratingCaptions] = useState(false);
  const [draftLoaded, setDraftLoaded] = useState(false);
  const [reviewing, setReviewing] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [reviewed, setReviewed] = useState(false);
  const [dirtySinceExport, setDirtySinceExport] = useState(true);
  const [position, setPosition] = useState(0);
  const [playing, setPlaying] = useState(false);
  const [previewError, setPreviewError] = useState(false);
  const [nativeUnsupported, setNativeUnsupported] = useState(false);
  const [proxyPath, setProxyPath] = useState<string | null>(null);
  const [proxyStartSeconds, setProxyStartSeconds] = useState(0);
  const [preparingPreview, setPreparingPreview] = useState(false);
  const [job, setJob] = useState<ExportStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const player = useRef<HTMLMediaElement | null>(null);
  const lastAppliedRangeVersion = useRef<number | undefined>(undefined);
  const completedCallback = useRef(onCompleted);
  const reportedOutput = useRef<string | null | undefined>(undefined);
  const saveTimer = useRef<number | null>(null);
  const saveQueue = useRef<Promise<void>>(Promise.resolve());
  const editVersion = useRef(0);
  const userEditedSinceExport = useRef(false);

  useEffect(() => {
    let live = true;
    invoke<FramingAnalysisMode>("get_framing_analysis_mode")
      .then((mode) => { if (live) setAnalysisMode(mode); })
      .catch((cause) => { if (live) setError(errorText(cause)); });
    return () => { live = false; };
  }, [media.path]);

  useEffect(() => {
    let live = true;
    invoke<ClipDraft | null>("get_export_draft")
      .then((draft) => {
        if (!live || !draft || selectedRange) return;
        setStart(String(draft.startMs / 1000));
        setEnd(String(draft.endMs / 1000));
        setFramingMode(draft.framingMode);
        setHorizontalPercent(draft.horizontalPercent);
        setFramingKeyframes(draft.framingKeyframes ?? []);
        setCaptions(draft.captions);
        setCaptionsTouched(true);
      })
      .catch((cause) => { if (live) setError(errorText(cause)); })
      .finally(() => { if (live) setDraftLoaded(true); });
    return () => { live = false; };
  }, [media.path]);

  useEffect(() => { completedCallback.current = onCompleted; }, [onCompleted]);
  useEffect(() => () => {
    void invoke("discard_export_preview", { sourcePath: media.path }).catch(() => { /* process exit also removes temporary previews */ });
  }, [media.path]);

  function reportOutput(path: string | null) {
    if (reportedOutput.current === path) return;
    reportedOutput.current = path;
    completedCallback.current?.(path);
  }

  useEffect(() => { editVersion.current += 1; setReviewed(false); setDirtySinceExport(true); reportOutput(null); }, [captions]);
  useEffect(() => { reportOutput(null); setDirtySinceExport(true); }, [media.path]);

  useEffect(() => {
    if (!selectedRange || selectedRange.version === lastAppliedRangeVersion.current) return;
    if (job?.state === "running" || reviewing || exporting) return;
    if (!Number.isFinite(selectedRange.startSeconds) || !Number.isFinite(selectedRange.endSeconds)
      || selectedRange.endSeconds <= selectedRange.startSeconds) return;
    lastAppliedRangeVersion.current = selectedRange.version;
    const inPoint = Math.max(0, Math.min(media.durationSeconds, selectedRange.startSeconds));
    const outPoint = Math.max(inPoint, Math.min(media.durationSeconds, selectedRange.endSeconds));
    setStart(String(inPoint));
    setEnd(String(outPoint));
    setFramingKeyframes((current) => rebaseKeyframes(current, Math.round(inPoint * 1000), Math.round(outPoint * 1000), framingMode, horizontalPercent));
    setPosition(inPoint);
    setReviewed(false);
    editVersion.current += 1;
    userEditedSinceExport.current = true;
    setDirtySinceExport(true);
    reportOutput(null);
    setProxyPath(null);
    setPreviewError(nativeUnsupported);
    if (player.current) {
      player.current.pause();
      player.current.currentTime = inPoint;
    }
  }, [media.durationSeconds, nativeUnsupported, selectedRange, job?.state, reviewing, exporting]);

  useEffect(() => {
    let live = true;
    async function refresh() {
      try {
        const [transcript, status] = await Promise.all([
          invoke<TranscriptStatus>("get_transcription_status"),
          invoke<ExportStatus>("get_export_status"),
        ]);
        if (!live) return;
        setJob(status);
        reportOutput(!userEditedSinceExport.current && status.state === "completed" ? status.outputPath : null);
        if (draftLoaded && !captionsTouched) {
          const nextCaptions = captionsFromTranscript(transcript.segments);
          setCaptions((current) => current.length === nextCaptions.length
            && current.every((cue, index) => cue.startMs === nextCaptions[index].startMs
              && cue.endMs === nextCaptions[index].endMs && cue.text === nextCaptions[index].text)
            ? current : nextCaptions);
        }
      } catch (cause) {
        if (live) setError(errorText(cause));
      }
    }
    void refresh();
    const timer = window.setInterval(refresh, 2500);
    return () => { live = false; window.clearInterval(timer); };
  }, [media.path, captionsTouched, dirtySinceExport, draftLoaded]);

  const startSeconds = Number(start);
  const endSeconds = Number(end);
  const validRange = Number.isFinite(startSeconds) && Number.isFinite(endSeconds)
    && startSeconds >= 0 && endSeconds > startSeconds && endSeconds <= media.durationSeconds;
  const visibleCaptions = validRange ? captions
    .map((caption, index) => ({ caption, index }))
    .filter(({ caption }) => caption.endMs > Math.round(startSeconds * 1000) && caption.startMs < Math.round(endSeconds * 1000)) : [];
  const validCaptions = visibleCaptions.length > 0 && visibleCaptions.length <= 500 && visibleCaptions.every(({ caption }) =>
    Number.isSafeInteger(caption.startMs) && Number.isSafeInteger(caption.endMs)
    && caption.startMs >= 0 && caption.endMs > caption.startMs
    && caption.text.trim().length > 0 && caption.text.length <= 240);
  const startMs = Math.round(startSeconds * 1000);
  const endMs = Math.round(endSeconds * 1000);
  const validKeyframes = framingKeyframes.length <= 150 && (framingKeyframes.length === 0
    || framingKeyframes[0].timestampMs === startMs)
    && framingKeyframes.every((keyframe, index) => Number.isSafeInteger(keyframe.timestampMs)
      && keyframe.timestampMs >= startMs && keyframe.timestampMs < endMs
      && Number.isInteger(keyframe.horizontalPercent) && keyframe.horizontalPercent >= 0
      && keyframe.horizontalPercent <= 100
      && (index === 0 || keyframe.timestampMs > framingKeyframes[index - 1].timestampMs));
  const activeFraming = framingAt(framingKeyframes, Math.round(position * 1000), framingMode, horizontalPercent);
  const activeCaption = visibleCaptions.find(({ caption }) => position * 1000 >= caption.startMs && position * 1000 < caption.endMs)?.caption;
  const editingDisabled = job?.state === "running" || reviewing || exporting;

  function currentDraft(): ClipDraft {
    return {
      startMs: Math.round(startSeconds * 1000),
      endMs: Math.round(endSeconds * 1000),
      framingMode,
      horizontalPercent,
      framingKeyframes,
      captions: visibleCaptions.map(({ caption }) => caption),
      reviewed: false,
    };
  }

  function enqueueSave(draft: ClipDraft): Promise<void> {
    const task = saveQueue.current.then(async () => { await invoke("save_export_draft", { draft }); });
    saveQueue.current = task.catch(() => { /* a later edit may still be saved */ });
    return task;
  }

  useEffect(() => {
    if (!draftLoaded || !validRange || !validKeyframes || visibleCaptions.length > 500 ||
      visibleCaptions.some(({ caption }) => caption.endMs <= caption.startMs || caption.text.trim().length === 0)) return;
    if (saveTimer.current !== null) window.clearTimeout(saveTimer.current);
    const draft = currentDraft();
    saveTimer.current = window.setTimeout(() => {
      saveTimer.current = null;
      void enqueueSave(draft).catch((cause) => setError(errorText(cause)));
    }, 650);
    return () => { if (saveTimer.current !== null) window.clearTimeout(saveTimer.current); };
  }, [draftLoaded, start, end, framingMode, horizontalPercent, framingKeyframes, captions]);

  async function setReview(checked: boolean) {
    if (!checked) { setReviewed(false); void enqueueSave(currentDraft()).catch((cause) => setError(errorText(cause))); return; }
    if (!validRange || !validCaptions || !validKeyframes || reviewing) return;
    if (saveTimer.current !== null) { window.clearTimeout(saveTimer.current); saveTimer.current = null; }
    const versionAtReview = editVersion.current;
    setReviewing(true);
    setError(null);
    try {
      await saveQueue.current;
      const draft = currentDraft();
      const saved = await invoke<ClipDraft>("review_export_draft", { draft });
      if (editVersion.current === versionAtReview && saved.reviewed) setReviewed(true);
      else setReviewed(false);
    } catch (cause) { setReviewed(false); setError(errorText(cause)); }
    finally { setReviewing(false); }
  }

  function editCaption(index: number, patch: Partial<Caption>) {
    setCaptions((current) => current.map((caption, currentIndex) => currentIndex === index ? { ...caption, ...patch } : caption));
    setCaptionsTouched(true);
    setReviewed(false);
    editVersion.current += 1;
    userEditedSinceExport.current = true;
    setDirtySinceExport(true);
    reportOutput(null);
  }

  async function regenerateCaptions() {
    setGeneratingCaptions(true);
    setError(null);
    try {
      const transcript = await invoke<TranscriptStatus>("get_transcription_status");
      if (!transcript.segments.length) {
        setError("Complete AI transcription before generating captions.");
        return;
      }
      setCaptions(captionsFromTranscript(transcript.segments));
      setCaptionsTouched(true);
      setReviewed(false);
      editVersion.current += 1;
      userEditedSinceExport.current = true;
      setDirtySinceExport(true);
      reportOutput(null);
    } catch (cause) { setError(errorText(cause)); }
    finally { setGeneratingCaptions(false); }
  }

  function seek(seconds: number) {
    const target = Math.max(0, Math.min(media.durationSeconds, seconds));
    if (player.current) player.current.currentTime = proxyPath ? Math.max(0, target - proxyStartSeconds) : target;
    setPosition(target);
  }

  async function togglePlayback() {
    const element = player.current;
    if (!element) return;
    if (!element.paused) {
      element.pause();
      return;
    }
    if (position < startSeconds || position >= endSeconds) seek(startSeconds);
    try { await element.play(); } catch { setPreviewError(true); }
  }

  function invalidatePreview() {
    setFramingSuggestion(null);
    setProxyPath(null);
    setPreviewError(nativeUnsupported);
    setReviewed(false);
    editVersion.current += 1;
    userEditedSinceExport.current = true;
    setDirtySinceExport(true);
    reportOutput(null);
  }

  function changeKeyframes(next: FramingKeyframe[]) {
    setFramingKeyframes(next.sort((left, right) => left.timestampMs - right.timestampMs));
    setReviewed(false);
    editVersion.current += 1;
    userEditedSinceExport.current = true;
    setDirtySinceExport(true);
    reportOutput(null);
  }

  function addKeyframe() {
    if (!validRange || !media.hasVideo) return;
    const timestampMs = Math.max(startMs, Math.min(endMs - 1, Math.round(position * 1000)));
    const first = framingKeyframes.length ? framingKeyframes : [{ timestampMs: startMs, mode: framingMode, horizontalPercent }];
    const active = framingAt(first, timestampMs, framingMode, horizontalPercent);
    changeKeyframes([...first.filter((keyframe) => keyframe.timestampMs !== timestampMs), { timestampMs, ...active }]);
  }

  function editKeyframe(index: number, patch: Partial<FramingKeyframe>) {
    const next = framingKeyframes.map((keyframe, currentIndex) => currentIndex === index ? { ...keyframe, ...patch } : keyframe);
    changeKeyframes(next);
  }

  async function changeAnalysisMode(mode: FramingAnalysisMode) {
    setError(null);
    try {
      await invoke("set_framing_analysis_mode", { mode });
      setAnalysisMode(mode);
      setFramingSuggestion(null);
      setCloudFrameConsent(false);
    } catch (cause) { setError(errorText(cause)); }
  }

  async function suggestFraming() {
    if (!validRange || !media.hasVideo || endSeconds - startSeconds > 300) return;
    setSuggestingFraming(true);
    setFramingSuggestion(null);
    setError(null);
    try {
      const command = analysisMode === "local" ? "suggest_local_framing" : "suggest_gemini_framing";
      const suggestion = await invoke<FramingSuggestion>(command, {
        startMs: Math.round(startSeconds * 1000),
        endMs: Math.round(endSeconds * 1000),
        ...(analysisMode === "gemini" ? { consent: cloudFrameConsent } : {}),
      });
      setFramingSuggestion(suggestion);
    } catch (cause) { setError(errorText(cause)); }
    finally { setSuggestingFraming(false); if (analysisMode === "gemini") setCloudFrameConsent(false); }
  }

  function applyFramingSuggestion() {
    if (!framingSuggestion || !Number.isInteger(framingSuggestion.horizontalPercent)
      || framingSuggestion.horizontalPercent < 0 || framingSuggestion.horizontalPercent > 100) return;
    setHorizontalPercent(framingSuggestion.horizontalPercent);
    setFramingKeyframes(rebaseKeyframes(framingSuggestion.keyframes ?? [], startMs, endMs, framingMode, framingSuggestion.horizontalPercent));
    editVersion.current += 1;
    userEditedSinceExport.current = true;
    setReviewed(false);
    setDirtySinceExport(true);
    reportOutput(null);
  }

  async function preparePreview() {
    if (!validRange || endSeconds - startSeconds > 300) return;
    setPreparingPreview(true);
    setError(null);
    try {
      const path = await invoke<string>("prepare_export_preview", {
        startMs: Math.round(startSeconds * 1000),
        endMs: Math.round(endSeconds * 1000),
      });
      setProxyStartSeconds(startSeconds);
      setProxyPath(path);
      setPosition(startSeconds);
      setPreviewError(false);
    } catch (cause) { setError(errorText(cause)); }
    finally { setPreparingPreview(false); }
  }

  async function cancelPreview() {
    try { await invoke("cancel_export_preview"); } catch (cause) { setError(errorText(cause)); }
  }

  async function exportClip() {
    if (!validRange || !reviewed || !validCaptions || !validKeyframes || exporting) return;
    const versionAtExport = editVersion.current;
    const draft = currentDraft();
    setExporting(true);
    setError(null);
    try {
      const destinationPath = await save({
        title: "Save reviewed clip",
        defaultPath: `${media.fileName.replace(/\.[^.]+$/, "")}-clip.mp4`,
        filters: [{ name: "MP4 video", extensions: ["mp4"] }],
      });
      if (!destinationPath) return;
      await saveQueue.current;
      if (editVersion.current !== versionAtExport) {
        setReviewed(false);
        throw new Error("The clip changed while choosing a destination. Review it again before export.");
      }
      const saved = await invoke<ClipDraft | null>("get_export_draft");
      if (!saved?.reviewed || saved.startMs !== draft.startMs || saved.endMs !== draft.endMs
        || saved.framingMode !== draft.framingMode || saved.horizontalPercent !== draft.horizontalPercent
        || JSON.stringify(saved.framingKeyframes ?? []) !== JSON.stringify(draft.framingKeyframes)
        || JSON.stringify(saved.captions) !== JSON.stringify(draft.captions)) {
        setReviewed(false);
        throw new Error("The saved clip changed since review. Review it again before export.");
      }
      const request = {
        startMs: saved.startMs,
        endMs: saved.endMs,
        destinationPath,
        captions: saved.captions,
        framing: { mode: saved.framingMode, horizontalPercent: saved.horizontalPercent, keyframes: saved.framingKeyframes ?? [] },
        reviewed: saved.reviewed,
      };
      const reviewedEditVersion = versionAtExport;
      reportOutput(null);
      setJob({ state: "running", stage: "Rendering video and captions", error: null, outputPath: null });
      await invoke("start_export_reviewed", { request });
      const status = await invoke<ExportStatus>("get_export_status");
      setJob(status);
      const matchesCurrentDraft = editVersion.current === reviewedEditVersion;
      if (status.state === "completed" && matchesCurrentDraft) { setDirtySinceExport(false); userEditedSinceExport.current = false; }
      reportOutput(status.state === "completed" && matchesCurrentDraft ? status.outputPath : null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : errorText(cause));
      reportOutput(null);
      try { setJob(await invoke<ExportStatus>("get_export_status")); } catch { /* retain the direct error */ }
    } finally { setExporting(false); }
  }

  async function cancel() {
    try { await invoke("cancel_export"); } catch (cause) { setError(errorText(cause)); }
  }

  return <section className="export-workspace" aria-label="Clip editor and export">
    <div className="section-title"><div><span>04</span><h2>Edit and export</h2></div><small>Review the source cut and captions first</small></div>
    <div className="export-layout">
      <div className="export-preview-panel">
        <p className="export-preview-note">Source playback · draft vertical frame and caption placement</p>
        <div className="export-preview-frame" aria-label="Source playback with draft vertical frame and captions">
          {(media.hasVideo || proxyPath) ? <video
            ref={(element) => { player.current = element; }}
            src={convertFileSrc(proxyPath || media.path)}
            style={{ objectFit: activeFraming.mode === "crop" ? "cover" : "contain", objectPosition: `${activeFraming.horizontalPercent}% center` }}
            playsInline
            onTimeUpdate={(event) => { const now = event.currentTarget.currentTime + (proxyPath ? proxyStartSeconds : 0); setPosition(now); if (validRange && now >= endSeconds) { event.currentTarget.pause(); seek(startSeconds); } }}
            onPlay={() => setPlaying(true)} onPause={() => setPlaying(false)} onError={() => { setPreviewError(true); if (!proxyPath) setNativeUnsupported(true); }}
          /> : <div className="export-audio-preview"><Captions size={40} /><span>Audio only · vertical background</span><audio
            ref={(element) => { player.current = element; }}
            src={convertFileSrc(media.path)}
            onTimeUpdate={(event) => { const now = event.currentTarget.currentTime; setPosition(now); if (validRange && now >= endSeconds) { event.currentTarget.pause(); seek(startSeconds); } }}
            onPlay={() => setPlaying(true)} onPause={() => setPlaying(false)} onError={() => { setPreviewError(true); setNativeUnsupported(true); }}
          /></div>}
          {activeCaption && <div className="export-preview-caption">{activeCaption.text}</div>}
        </div>
        {previewError && <div className="export-preview-fallback"><p className="export-error" role="alert">This recording's format cannot play directly in the desktop preview.{proxyPath ? " The temporary preview also failed." : " Prepare a local MP4 preview for this cut."}</p>{!proxyPath && <>{validRange && endSeconds - startSeconds > 300 && <p className="export-help">Temporary preview supports cuts up to five minutes.</p>}<button type="button" className="replace-button" disabled={preparingPreview || !validRange || endSeconds - startSeconds > 300} onClick={preparePreview}>{preparingPreview ? "Preparing preview…" : "Prepare local preview"}</button>{preparingPreview && <button type="button" className="replace-button" onClick={cancelPreview}>Cancel preview</button>}</>}</div>}
        <div className="export-player-controls">
          <button type="button" className="replace-button" onClick={togglePlayback} disabled={!validRange || previewError}>{playing ? <Pause size={15} /> : <CirclePlay size={15} />}{playing ? "Pause" : "Play cut"}</button>
          <button type="button" className="replace-button" onClick={() => seek(position - 0.1)} aria-label="Step back 0.1 seconds">−0.1 s</button>
          <button type="button" className="replace-button" onClick={() => seek(position + 0.1)} aria-label="Step forward 0.1 seconds">+0.1 s</button>
          <span>{time(position)} / {time(media.durationSeconds)}</span>
        </div>
      </div>
      <div className="export-edit-panel">
        <h3><Scissors size={18} /> Cut and frame</h3>
        <div className="export-time-fields">
          <label>In (seconds)<input type="number" min="0" max={media.durationSeconds} step="0.001" value={start} disabled={editingDisabled} onChange={(event) => { const next = event.target.value; setStart(next); if (Number.isFinite(Number(next)) && Number(next) < endSeconds) setFramingKeyframes((current) => rebaseKeyframes(current, Math.round(Number(next) * 1000), endMs, framingMode, horizontalPercent)); invalidatePreview(); }} /></label>
          <label>Out (seconds)<input type="number" min="0" max={media.durationSeconds} step="0.001" value={end} disabled={editingDisabled} onChange={(event) => { const next = event.target.value; setEnd(next); if (Number.isFinite(Number(next)) && Number(next) > startSeconds) setFramingKeyframes((current) => rebaseKeyframes(current, startMs, Math.round(Number(next) * 1000), framingMode, horizontalPercent)); invalidatePreview(); }} /></label>
        </div>
        {!validRange && <p className="export-error" role="alert">Choose an in point before the out point, within the recording.</p>}
        <label className="export-field-label">Default vertical framing<select value={framingMode} disabled={editingDisabled} onChange={(event) => { setFramingMode(event.target.value as "crop" | "fit"); editVersion.current += 1; userEditedSinceExport.current = true; setReviewed(false); setDirtySinceExport(true); reportOutput(null); }}><option value="crop">Fill 9:16 frame</option><option value="fit">Fit with dark background</option></select></label>
        {framingMode === "crop" && <label className="export-field-label">Default horizontal position · {horizontalPercent}%<input type="range" min="0" max="100" value={horizontalPercent} disabled={editingDisabled} onChange={(event) => { setHorizontalPercent(Number(event.target.value)); editVersion.current += 1; userEditedSinceExport.current = true; setReviewed(false); setDirtySinceExport(true); reportOutput(null); }} /></label>}
        {media.hasVideo && <div className="export-keyframes">
          <div className="export-keyframes-heading"><div><h4>Frame changes</h4><p>Each frame setting applies until the next point. The preview follows the playhead.</p></div><button type="button" className="replace-button" disabled={editingDisabled || !validRange || framingKeyframes.length >= 150} onClick={addKeyframe}><Plus size={15} /> Add at {time(position)}</button></div>
          {framingKeyframes.map((keyframe, index) => <div className="export-keyframe-row" key={`${keyframe.timestampMs}-${index}`}>
            <label>At (seconds)<input type="number" min={startSeconds} max={endSeconds} step="0.001" value={keyframe.timestampMs / 1000} disabled={editingDisabled || index === 0} onChange={(event) => { const timestampMs = Math.round(Number(event.target.value) * 1000); if (Number.isSafeInteger(timestampMs) && timestampMs > startMs && timestampMs < endMs && !framingKeyframes.some((other, otherIndex) => otherIndex !== index && other.timestampMs === timestampMs)) editKeyframe(index, { timestampMs }); }} /></label>
            <label>Frame<select value={keyframe.mode} disabled={editingDisabled} onChange={(event) => editKeyframe(index, { mode: event.target.value as FramingKeyframe["mode"] })}><option value="crop">Fill 9:16</option><option value="fit">Fit all</option></select></label>
            <label>Position · {keyframe.horizontalPercent}%<input type="range" min="0" max="100" value={keyframe.horizontalPercent} disabled={editingDisabled || keyframe.mode === "fit"} onChange={(event) => editKeyframe(index, { horizontalPercent: Number(event.target.value) })} /></label>
            <button type="button" className="export-remove-cue" aria-label={`Remove frame change ${index + 1}`} title="Remove frame change" disabled={editingDisabled} onClick={() => { const remaining = framingKeyframes.filter((_, currentIndex) => currentIndex !== index); changeKeyframes(remaining.length && index === 0 ? rebaseKeyframes(remaining, startMs, endMs, framingMode, horizontalPercent) : remaining); }}><Trash2 size={17} /></button>
          </div>)}
          {framingKeyframes.length === 0 && <p className="export-help">One frame setting applies to the whole cut. Add a change where the shot widens or the subject moves.</p>}
          {!validKeyframes && <p className="export-error" role="alert">Frame changes must start at the in point, occur in order within the cut, and stay under 150 points.</p>}
        </div>}
        {media.hasVideo && <div className="export-framing-assistant">
          <label className="export-field-label">Suggest crop using<select value={analysisMode} disabled={editingDisabled || suggestingFraming} onChange={(event) => { void changeAnalysisMode(event.target.value as FramingAnalysisMode); }}><option value="local">Local face analysis</option><option value="gemini">Gemini visual analysis</option></select></label>
          {analysisMode === "gemini" && <label className="export-cloud-consent"><input type="checkbox" checked={cloudFrameConsent} onChange={(event) => setCloudFrameConsent(event.target.checked)} /> Send a small sample of video frames to Gemini for this suggestion. No audio or full recording is sent.</label>}
          <div className="export-framing-actions"><button type="button" className="replace-button" disabled={editingDisabled || suggestingFraming || !validRange || endSeconds - startSeconds > 300 || (analysisMode === "gemini" && !cloudFrameConsent)} onClick={() => { void suggestFraming(); }}>{suggestingFraming ? "Analyzing frames…" : "Suggest 9:16 center"}</button>{suggestingFraming && <button type="button" className="replace-button" onClick={() => { void invoke(analysisMode === "local" ? "cancel_local_framing" : "cancel_gemini_framing"); }}>Cancel</button>}</div>
          {framingSuggestion && <div className="export-framing-result" role="status"><p>Suggested position: <strong>{framingSuggestion.horizontalPercent}%</strong> · {framingSuggestion.keyframes?.length ?? 0} frame changes · {framingSuggestion.confidence} confidence</p><p>{framingSuggestion.reason === "noFace" ? "No clear face was found; the center is a fallback." : framingSuggestion.reason === "multipleFaces" ? "Several faces compete for the frame; check the preview through the cut." : framingSuggestion.reason === "movingFace" ? "The subject moves during the cut; check the suggested crop through playback." : framingSuggestion.reason === "visualSubject" ? "Gemini identified the main visual subject. Check the crop through playback." : "A stable face was found in sampled frames."}</p><button type="button" className="replace-button" disabled={editingDisabled} onClick={applyFramingSuggestion}>Apply suggestion</button></div>}
        </div>}
        <p className="export-help">This source playback approximates the final crop and captions. Review the rendered MP4 before publishing.</p>
      </div>
    </div>
    <div className="export-caption-panel">
      <div className="export-caption-heading"><div><h3><Captions size={18} /> AI transcript captions</h3><p>Timed subtitles come from the AI transcription and your corrections. Review the wording and timing against the source.</p></div><div className="export-caption-actions"><button type="button" className="replace-button" disabled={editingDisabled || generatingCaptions} onClick={() => { void regenerateCaptions(); }} title="Replace current caption edits with subtitles from the corrected AI transcript"><RotateCcw size={15} /> {generatingCaptions ? "Generating…" : "Regenerate from transcript"}</button><button type="button" className="replace-button" disabled={editingDisabled} onClick={() => { const startMs = Math.round(Math.max(0, validRange ? startSeconds : 0, position) * 1000); setCaptions((current) => [...current, { startMs, endMs: Math.min(Math.round(media.durationSeconds * 1000), startMs + 2000), text: "" }]); setCaptionsTouched(true); userEditedSinceExport.current = true; setReviewed(false); reportOutput(null); }}><Plus size={15} /> Add cue</button></div></div>
      <p className="export-caption-replace-note">Regenerating replaces current caption edits. It does not send another provider request.</p>
      {visibleCaptions.length ? <div className="export-caption-list">{visibleCaptions.map(({ caption, index }) => <div className="export-caption-row" key={index}>
        <label>Start<input type="number" min="0" step="0.001" value={caption.startMs / 1000} disabled={editingDisabled} onChange={(event) => editCaption(index, { startMs: Math.round(Number(event.target.value) * 1000) })} /></label>
        <label>End<input type="number" min="0" step="0.001" value={caption.endMs / 1000} disabled={editingDisabled} onChange={(event) => editCaption(index, { endMs: Math.round(Number(event.target.value) * 1000) })} /></label>
        <label className="export-caption-text">Text<input value={caption.text} maxLength={240} disabled={editingDisabled} onChange={(event) => editCaption(index, { text: event.target.value })} /></label>
        <button type="button" className="export-remove-cue" aria-label={`Remove caption ${index + 1}`} title="Remove caption" disabled={editingDisabled} onClick={() => { setCaptions((current) => current.filter((_, currentIndex) => currentIndex !== index)); setCaptionsTouched(true); userEditedSinceExport.current = true; setReviewed(false); reportOutput(null); }}><Trash2 size={17} /></button>
      </div>)}</div> : <p className="export-help">No captions touch this cut yet. Transcribe the recording or add a cue manually.</p>}
      {visibleCaptions.length > 500 && <p className="export-error" role="alert">This cut has {visibleCaptions.length} caption cues; the export limit is 500. Shorten the cut or combine adjacent cues.</p>}
      {visibleCaptions.length > 0 && visibleCaptions.length <= 500 && !validCaptions && <p className="export-error" role="alert">Each caption needs text and an end time after its start time.</p>}
    </div>
    <div className="export-final-review">
      <label><input type="checkbox" checked={reviewed} disabled={!draftLoaded || reviewing || exporting || !validRange || !validCaptions || !validKeyframes || previewError || job?.state === "running"} onChange={(event) => { void setReview(event.target.checked); }} /> I reviewed the cut, framing, and caption text against the recording.</label>
      <div className="export-actions"><button type="button" className="primary-button" disabled={!reviewed || reviewing || exporting || !validRange || !validCaptions || !validKeyframes || previewError || job?.state === "running"} onClick={exportClip}><FileDown size={16} /> {exporting ? "Exporting…" : "Export MP4 + SRT"}</button>{job?.state === "running" && <button type="button" className="replace-button" onClick={cancel}>Cancel export</button>}</div>
      {job && job.state !== "idle" && <p className="export-job" role="status">{job.stage}{job.state === "running" ? "…" : ""}{job.outputPath && <> · Saved to <strong>{job.outputPath}</strong></>}</p>}
      {(error || job?.error) && <p className="export-error" role="alert">{error || job?.error}</p>}
    </div>
  </section>;
}
