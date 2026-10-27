import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { CircleCheck, ExternalLink, RefreshCw, Send, ShieldCheck, Upload } from "lucide-react";
import { useEffect, useState } from "react";
import { useTranslation } from "../../i18n";
import "./PublishWorkspace.css";

type TikTokPrivacy = "PUBLIC_TO_EVERYONE" | "MUTUAL_FOLLOW_FRIENDS" | "FOLLOWER_OF_CREATOR" | "SELF_ONLY";
type TikTokCapabilities = {
  platform: "tiktok";
  account_id: string;
  display_name: string;
  app_audited: boolean;
  allowed_privacy: TikTokPrivacy[];
  max_duration_ms: number;
  comments_disabled: boolean;
  duet_disabled: boolean;
  stitch_disabled: boolean;
};
type Eligibility = { available: boolean; reason: string | null; capabilities: TikTokCapabilities | null };
type PublishState = { stage: "draft" | "reviewed" | "uploading" | "cancelled" }
  | { stage: "processing"; provider_reference: string }
  | { stage: "published"; provider_reference: string; post_id: string | null }
  | { stage: "failed"; message: string };
type PublishJob = {
  export_path: string;
  state: PublishState;
  metadata: {
    account_id: string;
    caption: string;
    destination: { tiktok?: {
      privacy: TikTokPrivacy;
      settings: {
        allow_comments: boolean;
        allow_duet: boolean;
        allow_stitch: boolean;
        promotes_own_brand: boolean;
        branded_content: boolean;
      };
    } };
  };
};
type Props = { exportPath: string };

function describeError(cause: unknown, fallback: string) {
  return typeof cause === "string" ? cause : cause instanceof Error ? cause.message : fallback;
}

function exportFileName(path: string, fallback: string) {
  return path.split(/[\\/]/).pop() || fallback;
}

const privacyKeys: Record<TikTokPrivacy, string> = {
  PUBLIC_TO_EVERYONE: "publishing.privacy.everyone",
  MUTUAL_FOLLOW_FRIENDS: "publishing.privacy.friends",
  FOLLOWER_OF_CREATOR: "publishing.privacy.followers",
  SELF_ONLY: "publishing.privacy.onlyMe",
};

export function PublishWorkspace({ exportPath }: Props) {
  const { t } = useTranslation();
  const [platform, setPlatform] = useState<"tiktok" | "instagram">("tiktok");
  const [eligibility, setEligibility] = useState<Eligibility | null>(null);
  const [job, setJob] = useState<PublishJob | null>(null);
  const [caption, setCaption] = useState("");
  const [privacy, setPrivacy] = useState<TikTokPrivacy | "">("");
  const [allowComments, setAllowComments] = useState(false);
  const [allowDuet, setAllowDuet] = useState(false);
  const [allowStitch, setAllowStitch] = useState(false);
  const [ownBrand, setOwnBrand] = useState(false);
  const [brandedContent, setBrandedContent] = useState(false);
  const [commercialDisclosure, setCommercialDisclosure] = useState(false);
  const [watched, setWatched] = useState(false);
  const [reviewedCaption, setReviewedCaption] = useState(false);
  const [reviewedDestination, setReviewedDestination] = useState(false);
  const [transferConsent, setTransferConsent] = useState(false);
  const [terms, setTerms] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [previewError, setPreviewError] = useState(false);

  useEffect(() => {
    let active = true;
    setEligibility(null);
    setJob(null);
    setError(null);
    setPreviewError(false);
    setWatched(false);
    setReviewedCaption(false);
    setReviewedDestination(false);
    setTransferConsent(false);
    setTerms(false);
    setCaption("");
    setPrivacy("");
    setAllowComments(false);
    setAllowDuet(false);
    setAllowStitch(false);
    setOwnBrand(false);
    setBrandedContent(false);
    setCommercialDisclosure(false);
    void Promise.all([
      invoke<Eligibility>("get_publish_capabilities", { platform }),
      invoke<PublishJob | null>("get_publish_status"),
    ]).then(([nextEligibility, nextJob]) => {
      if (!active) return;
      setEligibility(nextEligibility);
      const restored = platform === "tiktok" && nextJob?.export_path === exportPath ? nextJob : null;
      setJob(restored);
      const tiktok = restored?.metadata.destination.tiktok;
      if (tiktok) {
        setCaption(restored.metadata.caption);
        setPrivacy(tiktok.privacy);
        setAllowComments(tiktok.settings.allow_comments);
        setAllowDuet(tiktok.settings.allow_duet);
        setAllowStitch(tiktok.settings.allow_stitch);
        setOwnBrand(tiktok.settings.promotes_own_brand);
        setBrandedContent(tiktok.settings.branded_content);
        setCommercialDisclosure(tiktok.settings.promotes_own_brand || tiktok.settings.branded_content);
      }
    }).catch((cause) => { if (active) setError(describeError(cause, t("publishing.the.publishing.request.failed"))); });
    return () => { active = false; };
  }, [platform, exportPath]);

  useEffect(() => {
    if (job?.state.stage !== "processing") return;
    let active = true;
    const poll = async () => {
      try {
        const updated = await invoke<PublishJob | null>("refresh_publish_status");
        if (active) setJob(updated);
      } catch (cause) {
        if (active) setError(describeError(cause, t("publishing.the.publishing.request.failed")));
      }
    };
    const timer = window.setInterval(poll, 6000);
    return () => { active = false; window.clearInterval(timer); };
  }, [job?.state.stage]);

  const caps = platform === "tiktok" ? eligibility?.capabilities : null;
  const privacyOptions = caps?.app_audited ? caps.allowed_privacy : caps?.allowed_privacy.filter((choice) => choice === "SELF_ONLY");
  const allReviewed = watched && reviewedCaption && reviewedDestination && transferConsent && terms;
  const canPrepare = platform === "tiktok" && !!eligibility?.available && !!caps && !!privacy
    && !!privacyOptions?.includes(privacy) && !(brandedContent && privacy === "SELF_ONLY")
    && (!commercialDisclosure || ownBrand || brandedContent) && !previewError && allReviewed && !busy
    && job?.state.stage !== "uploading" && job?.state.stage !== "processing";
  const eligibilityReason = platform === "instagram" ? t("publishing.eligibilityInstagram")
    : eligibility?.reason?.startsWith("Connect TikTok through an approved app") ? t("publishing.eligibilityTikTok")
    : eligibility?.reason;

  async function refreshEligibility() {
    setError(null);
    setBusy(true);
    try { setEligibility(await invoke<Eligibility>("get_publish_capabilities", { platform })); }
    catch (cause) { setError(describeError(cause, t("publishing.the.publishing.request.failed"))); }
    finally { setBusy(false); }
  }

  async function prepare() {
    if (!canPrepare || !caps) return;
    setError(null);
    setBusy(true);
    try {
      const metadata = {
        account_id: caps.account_id,
        caption,
        destination: { tiktok: { privacy, settings: {
          allow_comments: allowComments, allow_duet: allowDuet, allow_stitch: allowStitch,
          promotes_own_brand: ownBrand, branded_content: brandedContent,
        } } },
      };
      const confirmation = {
        watched_export: watched, reviewed_caption: reviewedCaption,
        reviewed_destination: reviewedDestination, consent_to_transfer: transferConsent,
        accepted_platform_terms: terms,
      };
      setJob(await invoke<PublishJob>("prepare_publish", { exportPath, metadata, confirmation }));
    } catch (cause) { setError(describeError(cause, t("publishing.the.publishing.request.failed"))); }
    finally { setBusy(false); }
  }

  async function publish() {
    setError(null);
    setBusy(true);
    try { setJob(await invoke<PublishJob>("confirm_publish")); }
    catch (cause) { setError(describeError(cause, t("publishing.the.publishing.request.failed"))); }
    finally { setBusy(false); }
  }

  async function cancel() {
    setError(null);
    try { await invoke("cancel_publish"); setJob(await invoke<PublishJob | null>("get_publish_status")); }
    catch (cause) { setError(describeError(cause, t("publishing.the.publishing.request.failed"))); }
  }

  if (eligibility && !eligibility.available) {
    return <section className="publish-workspace" aria-label={t("publishing.manual.publishing.handoff")}>
      <div className="section-title"><div><span>05</span><h2>{t("publishing.share.your.clip")}</h2></div><small>{t("publishing.upload.the.reviewed.file.manually")}</small></div>
      <div className="publish-grid">
        <div className="publish-preview-panel"><div className="publish-preview-body"><div className="publish-player-surface"><video className="publish-preview" src={convertFileSrc(exportPath)} controls playsInline aria-label={t("publishing.final.exported.clip")} /></div><div className="publish-artifact-summary"><span className="publish-ready-label"><CircleCheck size={14} /> {t("publishing.ready.for.review")}</span><h3>{exportFileName(exportPath, t("publishing.exported.clip.mp4"))}</h3><p>{t("publishing.mp4.saved.on.this.computer")}</p><p className="publish-path" title={exportPath}>{exportPath}</p></div></div></div>
        <div className="publish-review-panel"><h3><Upload size={17} /> {t("publishing.manual.upload")}</h3><label className="publish-field">{t("publishing.platform")}<select value={platform} onChange={(event) => setPlatform(event.target.value as "tiktok" | "instagram")}><option value="tiktok">{t("publishing.tiktok")}</option><option value="instagram">{t("publishing.instagram.reels")}</option></select></label><p className="publish-unavailable" role="status">{eligibilityReason}</p><p className="publish-hint">{t("publishing.your.mp4.and.srt.remain")}</p><button type="button" className="replace-button" onClick={refreshEligibility} disabled={busy}><RefreshCw size={14} /> {t("publishing.check.eligibility.again")}</button></div>
      </div>
    </section>;
  }

  return <section className="publish-workspace" aria-label={t("publishing.review.and.publish")}>
    <div className="section-title"><div><span>05</span><h2>{t("publishing.publish.your.clip")}</h2></div><small>{t("publishing.your.reviewed.mp4.stays.on")}</small></div>
    <div className="publish-grid">
      <div className="publish-preview-panel">
        <div className="publish-preview-body"><div className="publish-player-surface"><video className="publish-preview" src={convertFileSrc(exportPath)} controls playsInline aria-label={t("publishing.final.exported.clip")} onError={() => { setPreviewError(true); setWatched(false); }} /></div><div className="publish-artifact-summary"><span className="publish-ready-label"><CircleCheck size={14} /> {t("publishing.ready.for.review")}</span><h3>{exportFileName(exportPath, t("publishing.exported.clip.mp4"))}</h3><p>{t("publishing.mp4.saved.on.this.computer")}</p><p className="publish-path" title={exportPath}>{exportPath}</p></div></div>
        {previewError && <p className="publish-error" role="alert">{t("publishing.the.final.mp4.cannot.be")}</p>}
        <p className="publish-hint">{t("publishing.you.can.upload.this.mp4")}</p>
      </div>
      <div className="publish-review-panel">
        <h3><Upload size={17} /> {t("publishing.destination")}</h3>
        <label className="publish-field">{t("publishing.platform")}<select value={platform} onChange={(event) => { setPlatform(event.target.value as "tiktok" | "instagram"); setJob(null); }}><option value="tiktok">{t("publishing.tiktok")}</option><option value="instagram">{t("publishing.instagram.reels")}</option></select></label>
        {eligibility === null ? <p className="publish-hint" role="status">{t("publishing.checking.account.eligibility")}</p> : eligibility.available && caps ? <p className="publish-account"><CircleCheck size={15} /> {caps.display_name} (@{caps.account_id}) · {caps.app_audited ?t("publishing.audited.app") :t("publishing.private.posting.only.until.app")}</p> : <p className="publish-unavailable" role="status">{eligibilityReason}</p>}
        <button type="button" className="replace-button" onClick={refreshEligibility} disabled={busy}><RefreshCw size={14} /> {t("publishing.check.eligibility.again")}</button>
        {platform === "tiktok" && <>
          <label className="publish-field">{t("publishing.caption.and.hashtags")}<textarea value={caption} maxLength={2200} onChange={(event) => { setCaption(event.target.value); setJob(null); setReviewedCaption(false); }} placeholder={t("publishing.write.the.final.caption.in")} /></label>
          <p className="publish-hint">{t("publishing.captionLength", { count: [...caption].length })}</p>
          <label className="publish-field">{t("publishing.visibility")}<select value={privacy} onChange={(event) => { setPrivacy(event.target.value as TikTokPrivacy | ""); setJob(null); setReviewedDestination(false); }} disabled={!privacyOptions?.length}><option value="">{t("publishing.choose.visibility")}</option>{privacyOptions?.map((choice) => <option key={choice} value={choice}>{t(privacyKeys[choice])}</option>)}</select></label>
          <div className="publish-options" aria-label={t("publishing.tiktok.interaction.settings")}>
            <label><input type="checkbox" checked={allowComments} disabled={caps?.comments_disabled} onChange={(event) => { setAllowComments(event.target.checked); setJob(null); setReviewedDestination(false); }} /> {t("publishing.allow.comments")}</label>
            <label><input type="checkbox" checked={allowDuet} disabled={caps?.duet_disabled} onChange={(event) => { setAllowDuet(event.target.checked); setJob(null); setReviewedDestination(false); }} /> {t("publishing.allow.duet")}</label>
            <label><input type="checkbox" checked={allowStitch} disabled={caps?.stitch_disabled} onChange={(event) => { setAllowStitch(event.target.checked); setJob(null); setReviewedDestination(false); }} /> {t("publishing.allow.stitch")}</label>
            <label><input type="checkbox" checked={commercialDisclosure} onChange={(event) => { setCommercialDisclosure(event.target.checked); if (!event.target.checked) { setOwnBrand(false); setBrandedContent(false); } setJob(null); setReviewedDestination(false); }} /> {t("publishing.this.clip.promotes.a.brand")}</label>
            {commercialDisclosure && <>
              <label><input type="checkbox" checked={ownBrand} onChange={(event) => { setOwnBrand(event.target.checked); setJob(null); setReviewedDestination(false); }} /> {t("publishing.my.brand.or.business")}</label>
              <label><input type="checkbox" checked={brandedContent} disabled={privacy === "SELF_ONLY"} onChange={(event) => { setBrandedContent(event.target.checked); setJob(null); setReviewedDestination(false); }} /> {t("publishing.paid.partnership.with.another.brand")}</label>
              {privacy === "SELF_ONLY" && <p className="publish-hint">{t(brandedContent ? "publishing.clearPrivatePartnership" : "publishing.paid.partnerships.cannot.use.private")}</p>}
              {!ownBrand && !brandedContent && <p className="publish-hint">{t("publishing.choose.at.least.one.commercial")}</p>}
              {brandedContent && <p className="publish-hint">{t("publishing.tiktok.will.label.this.clip")}</p>}
              {ownBrand && !brandedContent && <p className="publish-hint">{t("publishing.tiktok.will.label.this.clip.1")}</p>}
            </>}
          </div>
        </>}
      </div>
    </div>
    <div className="publish-checklist">
      <h3><ShieldCheck size={17} /> {t("publishing.final.review")}</h3>
      <label><input type="checkbox" checked={watched} disabled={previewError} onChange={(event) => setWatched(event.target.checked)} /> {t("publishing.i.watched.the.final.exported")}</label>
      <label><input type="checkbox" checked={reviewedCaption} onChange={(event) => setReviewedCaption(event.target.checked)} /> {t("publishing.i.reviewed.the.caption.hashtags")}</label>
      <label><input type="checkbox" checked={reviewedDestination} onChange={(event) => setReviewedDestination(event.target.checked)} /> {t("publishing.i.reviewed.the.account.visibility")}</label>
      <label><input type="checkbox" checked={transferConsent} onChange={(event) => setTransferConsent(event.target.checked)} /> {t("publishing.i.agree.to.transfer.this")}</label>
      <label><input type="checkbox" checked={terms} onChange={(event) => setTerms(event.target.checked)} /> {t("publishing.by.posting.i.agree.to")} <a href="https://www.tiktok.com/legal/page/global/music-usage-confirmation/en" target="_blank" rel="noreferrer">{t("publishing.music.usage.confirmation")} <ExternalLink size={12} /></a>{brandedContent ? <> {t("publishing.and")} <a href="https://www.tiktok.com/legal/page/global/bc-policy/en" target="_blank" rel="noreferrer">{t("publishing.branded.content.policy")} <ExternalLink size={12} /></a></> : null}.</label>
      <div className="publish-actions">
        {job?.state.stage === "reviewed" ? <button type="button" className="primary-button" disabled={busy || platform !== "tiktok" || !allReviewed || previewError} onClick={publish}><Send size={15} /> {t("publishing.publish.to.tiktok")}</button> : <button type="button" className="replace-button" disabled={!canPrepare} onClick={prepare}>{t("publishing.confirm.review")}</button>}
        {(job?.state.stage === "draft" || job?.state.stage === "reviewed" || job?.state.stage === "uploading") && <button type="button" className="replace-button" onClick={cancel}>{t("publishing.cancel")}</button>}
      </div>
      {job && <p className="publish-state" role="status">{job.state.stage === "uploading" ?t("publishing.uploading.your.reviewed.mp4.to") : job.state.stage === "processing" ?t("publishing.tiktok.is.processing.the.video") : job.state.stage === "published" ? t("publishing.postConfirmed", { post: job.state.post_id ? t("publishing.postReference", { id: job.state.post_id }) : t("publishing.postIdUnavailable") }) : job.state.stage === "failed" ? job.state.message : t("publishing.publicationStage", { stage: t(`publishing.stage.${job.state.stage}`) })}</p>}
      {error && <p className="publish-error" role="alert">{error}</p>}
    </div>
  </section>;
}
