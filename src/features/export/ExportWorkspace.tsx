import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { save } from "@tauri-apps/plugin-dialog";
import { Captions, CirclePlay, FileDown, Pause, Plus, RotateCcw, Scissors, Trash2 } from "lucide-react";
import { useEffect, useRef, useState, type CSSProperties } from "react";
import { useTranslation } from "../../i18n";
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

function preciseTime(seconds: number) {
  const ms = Math.round(Math.max(0, seconds) * 1000);
  return `${Math.floor(ms / 60000)}:${String(Math.floor(ms / 1000) % 60).padStart(2, "0")}.${String(ms % 1000).padStart(3, "0")}`;
}

function errorText(cause: unknown, fallback: string) {
  return typeof cause === "string" ? cause : fallback;
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
  const { t } = useTranslation();
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
  const [saveState, setSaveState] = useState<"loading" | "saved" | "pending" | "saving" | "failed" | "invalid">("loading");
  const saveRevision = useRef(0);
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
      .catch((cause) => { if (live) setError(errorText(cause, t("export.the.export.could.not.be"))); });
    return () => { live = false; };
  }, [media.path]);

  useEffect(() => {
    let live = true;
    invoke<ClipDraft | null>("get_export_draft")
      .then((draft) => {
        if (!live || !draft || selectedRange) return;
        setStart(String(draft.startMs / 1000));
        setEnd(String(draft.endMs / 1000));
        setPosition(draft.startMs / 1000);
        if (player.current) player.current.currentTime = draft.startMs / 1000;
        setFramingMode(draft.framingMode);
        setHorizontalPercent(draft.horizontalPercent);
        setFramingKeyframes(draft.framingKeyframes ?? []);
        setCaptions(draft.captions);
        setCaptionsTouched(true);
      })
      .catch((cause) => { if (live) setError(errorText(cause, t("export.the.export.could.not.be"))); })
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
        if (live) setError(errorText(cause, t("export.the.export.could.not.be")));
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
  const timelineStart = validRange ? startSeconds : 0;
  const timelineEnd = validRange ? endSeconds : media.durationSeconds;
  const timelineProgress = Math.max(0, Math.min(100, ((position - timelineStart) / Math.max(0.001, timelineEnd - timelineStart)) * 100));

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
    if (!draftLoaded) return;
    const revision = ++saveRevision.current;
    if (!validRange || !validKeyframes || visibleCaptions.length > 500 ||
      visibleCaptions.some(({ caption }) => caption.endMs <= caption.startMs || caption.text.trim().length === 0)) { setSaveState("invalid"); return; }
    if (saveTimer.current !== null) window.clearTimeout(saveTimer.current);
    setSaveState("pending");
    const draft = currentDraft();
    saveTimer.current = window.setTimeout(() => {
      saveTimer.current = null;
      if (saveRevision.current === revision) setSaveState("saving");
      void enqueueSave(draft)
        .then(() => { if (saveRevision.current === revision) setSaveState("saved"); })
        .catch((cause) => { if (saveRevision.current === revision) { setSaveState("failed"); setError(errorText(cause, t("export.the.export.could.not.be"))); } });
    }, 650);
    return () => { if (saveTimer.current !== null) window.clearTimeout(saveTimer.current); };
  }, [draftLoaded, start, end, framingMode, horizontalPercent, framingKeyframes, captions]);

  async function setReview(checked: boolean) {
    if (!checked) { setReviewed(false); void enqueueSave(currentDraft()).catch((cause) => setError(errorText(cause, t("export.the.export.could.not.be")))); return; }
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
    } catch (cause) { setReviewed(false); setError(errorText(cause, t("export.the.export.could.not.be"))); }
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
        setError(t("export.complete.ai.transcription.before.generating"));
        return;
      }
      setCaptions(captionsFromTranscript(transcript.segments));
      setCaptionsTouched(true);
      setReviewed(false);
      editVersion.current += 1;
      userEditedSinceExport.current = true;
      setDirtySinceExport(true);
      reportOutput(null);
    } catch (cause) { setError(errorText(cause, t("export.the.export.could.not.be"))); }
    finally { setGeneratingCaptions(false); }
  }

  function seek(seconds: number) {
    const target = Math.max(timelineStart, Math.min(timelineEnd, seconds));
    if (player.current) {
      const relativeTarget = proxyPath ? target - proxyStartSeconds : target;
      const duration = player.current.duration;
      player.current.currentTime = Number.isFinite(duration) ? Math.max(0, Math.min(duration, relativeTarget)) : Math.max(0, relativeTarget);
    }
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
    } catch (cause) { setError(errorText(cause, t("export.the.export.could.not.be"))); }
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
    } catch (cause) { setError(errorText(cause, t("export.the.export.could.not.be"))); }
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
    } catch (cause) { setError(errorText(cause, t("export.the.export.could.not.be"))); }
    finally { setPreparingPreview(false); }
  }

  async function cancelPreview() {
    try { await invoke("cancel_export_preview"); } catch (cause) { setError(errorText(cause, t("export.the.export.could.not.be"))); }
  }

  async function exportClip() {
    if (!validRange || !reviewed || !validCaptions || !validKeyframes || exporting) return;
    const versionAtExport = editVersion.current;
    const draft = currentDraft();
    setExporting(true);
    setError(null);
    try {
      const destinationPath = await save({
        title:t("export.save.reviewed.clip"),
        defaultPath: `${media.fileName.replace(/\.[^.]+$/, "")}-clip.mp4`,
        filters: [{ name:t("export.mp4.video"), extensions: ["mp4"] }],
      });
      if (!destinationPath) return;
      await saveQueue.current;
      if (editVersion.current !== versionAtExport) {
        setReviewed(false);
        throw new Error(t("export.the.clip.changed.while.choosing"));
      }
      const saved = await invoke<ClipDraft | null>("get_export_draft");
      if (!saved?.reviewed || saved.startMs !== draft.startMs || saved.endMs !== draft.endMs
        || saved.framingMode !== draft.framingMode || saved.horizontalPercent !== draft.horizontalPercent
        || JSON.stringify(saved.framingKeyframes ?? []) !== JSON.stringify(draft.framingKeyframes)
        || JSON.stringify(saved.captions) !== JSON.stringify(draft.captions)) {
        setReviewed(false);
        throw new Error(t("export.the.saved.clip.changed.since"));
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
      setJob({ state: "running", stage:t("export.rendering.video.and.captions"), error: null, outputPath: null });
      await invoke("start_export_reviewed", { request });
      const status = await invoke<ExportStatus>("get_export_status");
      setJob(status);
      const matchesCurrentDraft = editVersion.current === reviewedEditVersion;
      if (status.state === "completed" && matchesCurrentDraft) { setDirtySinceExport(false); userEditedSinceExport.current = false; }
      reportOutput(status.state === "completed" && matchesCurrentDraft ? status.outputPath : null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : errorText(cause, t("export.the.export.could.not.be")));
      reportOutput(null);
      try { setJob(await invoke<ExportStatus>("get_export_status")); } catch { /* retain the direct error */ }
    } finally { setExporting(false); }
  }

  async function cancel() {
    try { await invoke("cancel_export"); } catch (cause) { setError(errorText(cause, t("export.the.export.could.not.be"))); }
  }

  return <section className="export-workspace" aria-label={t("export.clip.editor.and.export")}>
    <div className="section-title"><div><span>04</span><h2>{t("export.edit.and.export")}</h2></div><small>{t("export.review.the.source.cut.and")} {saveState === "loading" ?t("export.loading.saved.edits") : saveState === "saved" ?t("export.edits.saved") : saveState === "pending" || saveState === "saving" ?t("export.saving.edits") : saveState === "failed" ?t("export.save.failed") :t("export.not.saved.until.the.cut")}</small></div>
    <div className="export-layout">
      <div className="export-preview-panel">
        <p className="export-preview-note">{t("export.source.playback.draft.vertical.frame")}</p>
        <div className="export-preview-frame" aria-label={t("export.source.playback.with.draft.vertical")}>
          {(media.hasVideo || proxyPath) ? <video
            ref={(element) => { player.current = element; }}
            src={convertFileSrc(proxyPath || media.path)}
            style={{ objectFit: activeFraming.mode === "crop" ? "cover" : "contain", objectPosition: `${activeFraming.horizontalPercent}% center` }}
            playsInline
             onLoadedMetadata={() => seek(position)}
            onTimeUpdate={(event) => { const now = event.currentTarget.currentTime + (proxyPath ? proxyStartSeconds : 0); setPosition(now); if (validRange && now >= endSeconds) { event.currentTarget.pause(); seek(startSeconds); } }}
            onPlay={() => setPlaying(true)} onPause={() => setPlaying(false)} onError={() => { setPreviewError(true); if (!proxyPath) setNativeUnsupported(true); }}
          /> : <div className="export-audio-preview"><Captions size={40} /><span>{t("export.audio.only.vertical.background")}</span><audio
            ref={(element) => { player.current = element; }}
            src={convertFileSrc(media.path)}
             onLoadedMetadata={() => seek(position)}
            onTimeUpdate={(event) => { const now = event.currentTarget.currentTime; setPosition(now); if (validRange && now >= endSeconds) { event.currentTarget.pause(); seek(startSeconds); } }}
            onPlay={() => setPlaying(true)} onPause={() => setPlaying(false)} onError={() => { setPreviewError(true); setNativeUnsupported(true); }}
          /></div>}
          {activeCaption && <div className="export-preview-caption">{activeCaption.text}</div>}
        </div>
        {previewError && <div className="export-preview-fallback"><p className="export-error" role="alert">{t(proxyPath ? "export.previewProxyFailed" : "export.previewNeedsProxy")}</p>{!proxyPath && <>{validRange && endSeconds - startSeconds > 300 && <p className="export-help">{t("export.temporary.preview.supports.cuts.up")}</p>}<button type="button" className="replace-button" disabled={preparingPreview || !validRange || endSeconds - startSeconds > 300} onClick={preparePreview}>{preparingPreview ?t("export.preparing.preview") :t("export.prepare.local.preview")}</button>{preparingPreview && <button type="button" className="replace-button" onClick={cancelPreview}>{t("export.cancel.preview")}</button>}</>}</div>}
        <div className="export-timeline" aria-label={t("export.selected.clip.timeline")}>
          <span className="export-timeline-ratio">9:16</span>
          <span className="export-timeline-time">{preciseTime(timelineStart)}</span>
          <div className="export-timeline-track" style={{ "--timeline-progress": `${timelineProgress}%` } as CSSProperties}>
            <input type="range" min={timelineStart} max={timelineEnd} step="0.001" value={Math.max(timelineStart, Math.min(timelineEnd, position))} disabled={!validRange || previewError} aria-label={t("export.seek.within.selected.clip")} aria-valuetext={preciseTime(position)} onChange={(event) => seek(Number(event.target.value))} />
          </div>
          <span className="export-timeline-time">{preciseTime(position)} / {preciseTime(timelineEnd)}</span>
        </div>
        <div className="export-player-controls">
          <button type="button" className="replace-button" onClick={togglePlayback} disabled={!validRange || previewError}>{playing ? <Pause size={15} /> : <CirclePlay size={15} />}{playing ?t("export.pause") :t("export.play.cut")}</button>
          <button type="button" className="replace-button" onClick={() => seek(position - 0.1)} disabled={!validRange || previewError} aria-label={t("export.step.back.0.1.seconds")}>{t("export.0.1.s")}</button>
          <button type="button" className="replace-button" onClick={() => seek(position + 0.1)} disabled={!validRange || previewError} aria-label={t("export.step.forward.0.1.seconds")}>{t("export.0.1.s.1")}</button>
          <span>{preciseTime(position)} / {time(media.durationSeconds)}</span>
          <span className="export-framing-readout" role="status">{activeFraming.mode === "crop" ? t("export.frameAtPlayheadCrop", { position: activeFraming.horizontalPercent }) : t("export.frameAtPlayheadFit")}</span>
        </div>
      </div>
      <div className="export-edit-panel">
        <h3><Scissors size={18} /> {t("export.cut.and.frame")}</h3>
        <div className="export-time-fields">
          <label>{t("export.in.seconds")}<input type="number" min="0" max={media.durationSeconds} step="0.001" value={start} disabled={editingDisabled} onChange={(event) => { const next = event.target.value; setStart(next); if (Number.isFinite(Number(next)) && Number(next) < endSeconds) setFramingKeyframes((current) => rebaseKeyframes(current, Math.round(Number(next) * 1000), endMs, framingMode, horizontalPercent)); invalidatePreview(); }} /></label>
          <label>{t("export.out.seconds")}<input type="number" min="0" max={media.durationSeconds} step="0.001" value={end} disabled={editingDisabled} onChange={(event) => { const next = event.target.value; setEnd(next); if (Number.isFinite(Number(next)) && Number(next) > startSeconds) setFramingKeyframes((current) => rebaseKeyframes(current, startMs, Math.round(Number(next) * 1000), framingMode, horizontalPercent)); invalidatePreview(); }} /></label>
        </div>
        {!validRange && <p className="export-error" role="alert">{t("export.choose.an.in.point.before")}</p>}
        <label className="export-field-label">{t("export.default.vertical.framing")}<select value={framingMode} disabled={editingDisabled} onChange={(event) => { setFramingMode(event.target.value as "crop" | "fit"); editVersion.current += 1; userEditedSinceExport.current = true; setReviewed(false); setDirtySinceExport(true); reportOutput(null); }}><option value="crop">{t("export.fill.9.16.frame")}</option><option value="fit">{t("export.fit.with.dark.background")}</option></select></label>
        {framingMode === "crop" && <label className="export-field-label">{t("export.default.horizontal.position")} {horizontalPercent}%<input type="range" min="0" max="100" value={horizontalPercent} disabled={editingDisabled} onChange={(event) => { setHorizontalPercent(Number(event.target.value)); editVersion.current += 1; userEditedSinceExport.current = true; setReviewed(false); setDirtySinceExport(true); reportOutput(null); }} /></label>}
        {media.hasVideo && <div className="export-keyframes">
          <div className="export-keyframes-heading"><div><h4>{t("export.frame.changes")}</h4><p>{t("export.each.frame.setting.applies.until")}</p></div><button type="button" className="replace-button" disabled={editingDisabled || !validRange || framingKeyframes.length >= 150} onClick={addKeyframe}><Plus size={15} /> {t("export.add.at")} {time(position)}</button></div>
          {framingKeyframes.map((keyframe, index) => <div className="export-keyframe-row" key={index}>
            <label>{t("export.at.seconds")}<input type="number" min={startSeconds} max={endSeconds} step="0.001" value={keyframe.timestampMs / 1000} disabled={editingDisabled || index === 0} onChange={(event) => { const timestampMs = Math.round(Number(event.target.value) * 1000); if (Number.isSafeInteger(timestampMs) && timestampMs > startMs && timestampMs < endMs && !framingKeyframes.some((other, otherIndex) => otherIndex !== index && other.timestampMs === timestampMs)) editKeyframe(index, { timestampMs }); }} /></label>
            <label>{t("export.frame")}<select value={keyframe.mode} disabled={editingDisabled} onChange={(event) => editKeyframe(index, { mode: event.target.value as FramingKeyframe["mode"] })}><option value="crop">{t("export.fill.9.16")}</option><option value="fit">{t("export.fit.all")}</option></select></label>
            <label>{t("export.position")} {keyframe.horizontalPercent}%<input type="range" min="0" max="100" value={keyframe.horizontalPercent} disabled={editingDisabled || keyframe.mode === "fit"} onChange={(event) => editKeyframe(index, { horizontalPercent: Number(event.target.value) })} /></label>
            <button type="button" className="replace-button" aria-label={t("export.goToFrameChange", { number: index + 1 })} onClick={() => seek(keyframe.timestampMs / 1000)}>{t("export.go.to")}</button>
            <button type="button" className="export-remove-cue" aria-label={t("export.removeFrameChange", { number: index + 1 })} title={t("export.remove.frame.change")} disabled={editingDisabled} onClick={() => { const remaining = framingKeyframes.filter((_, currentIndex) => currentIndex !== index); changeKeyframes(remaining.length && index === 0 ? rebaseKeyframes(remaining, startMs, endMs, framingMode, horizontalPercent) : remaining); }}><Trash2 size={17} /></button>
          </div>)}
          {framingKeyframes.length === 0 && <p className="export-help">{t("export.one.frame.setting.applies.to")}</p>}
          {!validKeyframes && <p className="export-error" role="alert">{t("export.frame.changes.must.start.at")}</p>}
        </div>}
        {media.hasVideo && <div className="export-framing-assistant">
          <label className="export-field-label">{t("export.suggest.crop.using")}<select value={analysisMode} disabled={editingDisabled || suggestingFraming} onChange={(event) => { void changeAnalysisMode(event.target.value as FramingAnalysisMode); }}><option value="local">{t("export.local.face.analysis")}</option><option value="gemini">{t("export.gemini.visual.analysis")}</option></select></label>
          {analysisMode === "gemini" && <label className="export-cloud-consent"><input type="checkbox" checked={cloudFrameConsent} onChange={(event) => setCloudFrameConsent(event.target.checked)} /> {t("export.send.a.small.sample.of")}</label>}
          <div className="export-framing-actions"><button type="button" className="replace-button" disabled={editingDisabled || suggestingFraming || !validRange || endSeconds - startSeconds > 300 || (analysisMode === "gemini" && !cloudFrameConsent)} onClick={() => { void suggestFraming(); }}>{suggestingFraming ?t("export.analyzing.frames") :t("export.suggest.9.16.center")}</button>{suggestingFraming && <button type="button" className="replace-button" onClick={() => { void invoke(analysisMode === "local" ? "cancel_local_framing" : "cancel_gemini_framing"); }}>{t("export.cancel")}</button>}</div>
          {framingSuggestion && <div className="export-framing-result" role="status"><p>{t("export.suggestedPosition", { position: framingSuggestion.horizontalPercent, count: framingSuggestion.keyframes?.length ?? 0, confidence: t(`export.confidence.${framingSuggestion.confidence}`) })}</p><p>{framingSuggestion.reason === "noFace" ?t("export.no.clear.face.was.found") : framingSuggestion.reason === "multipleFaces" ?t("export.several.faces.compete.for.the") : framingSuggestion.reason === "movingFace" ?t("export.the.subject.moves.during.the") : framingSuggestion.reason === "visualSubject" ?t("export.gemini.identified.the.main.visual") : t("export.stableFace")}</p><button type="button" className="replace-button" disabled={editingDisabled} onClick={applyFramingSuggestion}>{t("export.apply.suggestion")}</button></div>}
        </div>}
        <p className="export-help">{t("export.this.source.playback.approximates.the")}</p>
      </div>
    </div>
    <div className="export-caption-panel">
      <div className="export-caption-heading"><div><h3><Captions size={18} /> {t("export.ai.transcript.captions")}</h3><p>{t("export.timed.subtitles.come.from.the")}</p></div><div className="export-caption-actions"><button type="button" className="replace-button" disabled={editingDisabled || generatingCaptions} onClick={() => { void regenerateCaptions(); }} title={t("export.replace.current.caption.edits.with")}><RotateCcw size={15} /> {generatingCaptions ?t("export.generating") :t("export.regenerate.from.transcript")}</button><button type="button" className="replace-button" disabled={editingDisabled} onClick={() => { const startMs = Math.round(Math.max(0, validRange ? startSeconds : 0, position) * 1000); setCaptions((current) => [...current, { startMs, endMs: Math.min(Math.round(media.durationSeconds * 1000), startMs + 2000), text: "" }]); setCaptionsTouched(true); userEditedSinceExport.current = true; setReviewed(false); reportOutput(null); }}><Plus size={15} /> {t("export.add.cue")}</button></div></div>
      <p className="export-caption-replace-note">{t("export.regenerating.replaces.current.caption.edits")}</p>
      {visibleCaptions.length ? <div className="export-caption-list">{visibleCaptions.map(({ caption, index }) => <div className="export-caption-row" key={index}>
        <label>{t("export.start")}<input type="number" min="0" step="0.001" value={caption.startMs / 1000} disabled={editingDisabled} onChange={(event) => editCaption(index, { startMs: Math.round(Number(event.target.value) * 1000) })} /></label>
        <label>{t("export.end")}<input type="number" min="0" step="0.001" value={caption.endMs / 1000} disabled={editingDisabled} onChange={(event) => editCaption(index, { endMs: Math.round(Number(event.target.value) * 1000) })} /></label>
        <label className="export-caption-text">{t("export.text")}<input value={caption.text} maxLength={240} disabled={editingDisabled} onChange={(event) => editCaption(index, { text: event.target.value })} /></label>
        <button type="button" className="export-remove-cue" aria-label={t("export.removeCaption", { number: index + 1 })} title={t("export.remove.caption")} disabled={editingDisabled} onClick={() => { setCaptions((current) => current.filter((_, currentIndex) => currentIndex !== index)); setCaptionsTouched(true); userEditedSinceExport.current = true; setReviewed(false); reportOutput(null); }}><Trash2 size={17} /></button>
      </div>)}</div> : <p className="export-help">{t("export.no.captions.touch.this.cut")}</p>}
      {visibleCaptions.length > 500 && <p className="export-error" role="alert">{t("export.captionLimit", { count: visibleCaptions.length })}</p>}
      {visibleCaptions.length > 0 && visibleCaptions.length <= 500 && !validCaptions && <p className="export-error" role="alert">{t("export.each.caption.needs.text.and")}</p>}
    </div>
    <div className="export-final-review">
      <label><input type="checkbox" checked={reviewed} disabled={!draftLoaded || reviewing || exporting || !validRange || !validCaptions || !validKeyframes || previewError || job?.state === "running"} onChange={(event) => { void setReview(event.target.checked); }} /> {t("export.i.reviewed.the.cut.framing")}</label>
      <div className="export-actions"><button type="button" className="primary-button" disabled={!reviewed || reviewing || exporting || !validRange || !validCaptions || !validKeyframes || previewError || job?.state === "running"} onClick={exportClip}><FileDown size={16} /> {exporting ?t("export.exporting") :t("export.export.mp4.srt")}</button>{job?.state === "running" && <button type="button" className="replace-button" onClick={cancel}>{t("export.cancel.export")}</button>}</div>
      {job && job.state !== "idle" && <p className="export-job" role="status">{job.stage}{job.state === "running" ? "…" : ""}{job.outputPath && <> {t("export.saved.to")} <strong>{job.outputPath}</strong></>}</p>}
      {(error || job?.error) && <p className="export-error" role="alert">{error || job?.error}</p>}
    </div>
  </section>;
}
