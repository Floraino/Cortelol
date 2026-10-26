import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { CircleCheck, ExternalLink, RefreshCw, Send, ShieldCheck, Upload } from "lucide-react";
import { useEffect, useState } from "react";
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

function describeError(cause: unknown) {
  return typeof cause === "string" ? cause : cause instanceof Error ? cause.message : "The publishing request failed.";
}

const privacyLabels: Record<TikTokPrivacy, string> = {
  PUBLIC_TO_EVERYONE: "Everyone",
  MUTUAL_FOLLOW_FRIENDS: "Friends",
  FOLLOWER_OF_CREATOR: "Followers",
  SELF_ONLY: "Only me",
};

export function PublishWorkspace({ exportPath }: Props) {
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
        setWatched(false);
        setReviewedCaption(false);
        setReviewedDestination(false);
        setTransferConsent(false);
        setTerms(false);
      }
    }).catch((cause) => { if (active) setError(describeError(cause)); });
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
        if (active) setError(describeError(cause));
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

  async function refreshEligibility() {
    setError(null);
    setBusy(true);
    try { setEligibility(await invoke<Eligibility>("get_publish_capabilities", { platform })); }
    catch (cause) { setError(describeError(cause)); }
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
    } catch (cause) { setError(describeError(cause)); }
    finally { setBusy(false); }
  }

  async function publish() {
    setError(null);
    setBusy(true);
    try { setJob(await invoke<PublishJob>("confirm_publish")); }
    catch (cause) { setError(describeError(cause)); }
    finally { setBusy(false); }
  }

  async function cancel() {
    setError(null);
    try { await invoke("cancel_publish"); setJob(await invoke<PublishJob | null>("get_publish_status")); }
    catch (cause) { setError(describeError(cause)); }
  }

  if (eligibility && !eligibility.available) {
    return <section className="publish-workspace" aria-label="Manual publishing handoff">
      <div className="section-title"><div><span>05</span><h2>Share your clip</h2></div><small>Upload the reviewed file manually</small></div>
      <div className="publish-grid">
        <div className="publish-preview-panel"><video className="publish-preview" src={convertFileSrc(exportPath)} controls playsInline aria-label="Final exported clip" /><p className="publish-path" title={exportPath}>Local export: {exportPath}</p></div>
        <div className="publish-review-panel"><h3><Upload size={17} /> Manual upload</h3><label className="publish-field">Platform<select value={platform} onChange={(event) => setPlatform(event.target.value as "tiktok" | "instagram")}><option value="tiktok">TikTok</option><option value="instagram">Instagram Reels</option></select></label><p className="publish-unavailable" role="status">{eligibility.reason}</p><p className="publish-hint">Your MP4 and SRT remain on your device. Review the final video and upload it with the platform's own app.</p><button type="button" className="replace-button" onClick={refreshEligibility} disabled={busy}><RefreshCw size={14} /> Check eligibility again</button></div>
      </div>
    </section>;
  }

  return <section className="publish-workspace" aria-label="Review and publish">
    <div className="section-title"><div><span>05</span><h2>Publish your clip</h2></div><small>Your reviewed MP4 stays on this device</small></div>
    <div className="publish-grid">
      <div className="publish-preview-panel">
        <video className="publish-preview" src={convertFileSrc(exportPath)} controls playsInline aria-label="Final exported clip" onError={() => { setPreviewError(true); setWatched(false); }} />
        {previewError && <p className="publish-error" role="alert">The final MP4 cannot be previewed here. Review the local file before preparing a post.</p>}
        <p className="publish-path" title={exportPath}>Local export: {exportPath}</p>
        <p className="publish-hint">You can upload this MP4 manually using the platform app whenever direct posting is unavailable.</p>
      </div>
      <div className="publish-review-panel">
        <h3><Upload size={17} /> Destination</h3>
        <label className="publish-field">Platform<select value={platform} onChange={(event) => { setPlatform(event.target.value as "tiktok" | "instagram"); setJob(null); }}><option value="tiktok">TikTok</option><option value="instagram">Instagram Reels</option></select></label>
        {eligibility === null ? <p className="publish-hint" role="status">Checking account eligibility…</p> : eligibility.available && caps ? <p className="publish-account"><CircleCheck size={15} /> {caps.display_name} (@{caps.account_id}) · {caps.app_audited ? "Audited app" : "Private posting only until app audit"}</p> : <p className="publish-unavailable" role="status">{eligibility.reason}</p>}
        <button type="button" className="replace-button" onClick={refreshEligibility} disabled={busy}><RefreshCw size={14} /> Check eligibility again</button>
        {platform === "tiktok" && <>
          <label className="publish-field">Caption and hashtags<textarea value={caption} maxLength={2200} onChange={(event) => { setCaption(event.target.value); setJob(null); setReviewedCaption(false); }} placeholder="Write the final caption in your own words" /></label>
          <p className="publish-hint">{[...caption].length} characters. TikTok counts UTF-16 units; the app validates the final value.</p>
          <label className="publish-field">Visibility<select value={privacy} onChange={(event) => { setPrivacy(event.target.value as TikTokPrivacy | ""); setJob(null); setReviewedDestination(false); }} disabled={!privacyOptions?.length}><option value="">Choose visibility</option>{privacyOptions?.map((choice) => <option key={choice} value={choice}>{privacyLabels[choice]}</option>)}</select></label>
          <div className="publish-options" aria-label="TikTok interaction settings">
            <label><input type="checkbox" checked={allowComments} disabled={caps?.comments_disabled} onChange={(event) => { setAllowComments(event.target.checked); setJob(null); setReviewedDestination(false); }} /> Allow comments</label>
            <label><input type="checkbox" checked={allowDuet} disabled={caps?.duet_disabled} onChange={(event) => { setAllowDuet(event.target.checked); setJob(null); setReviewedDestination(false); }} /> Allow Duet</label>
            <label><input type="checkbox" checked={allowStitch} disabled={caps?.stitch_disabled} onChange={(event) => { setAllowStitch(event.target.checked); setJob(null); setReviewedDestination(false); }} /> Allow Stitch</label>
            <label><input type="checkbox" checked={commercialDisclosure} onChange={(event) => { setCommercialDisclosure(event.target.checked); if (!event.target.checked) { setOwnBrand(false); setBrandedContent(false); } setJob(null); setReviewedDestination(false); }} /> This clip promotes a brand, product, or service</label>
            {commercialDisclosure && <>
              <label><input type="checkbox" checked={ownBrand} onChange={(event) => { setOwnBrand(event.target.checked); setJob(null); setReviewedDestination(false); }} /> My brand or business</label>
              <label><input type="checkbox" checked={brandedContent} disabled={privacy === "SELF_ONLY"} onChange={(event) => { setBrandedContent(event.target.checked); setJob(null); setReviewedDestination(false); }} /> Paid partnership with another brand</label>
              {privacy === "SELF_ONLY" && <p className="publish-hint">Paid partnerships cannot use private visibility.{brandedContent ? " Clear this choice to continue." : ""}</p>}
              {!ownBrand && !brandedContent && <p className="publish-hint">Choose at least one commercial content category.</p>}
              {brandedContent && <p className="publish-hint">TikTok will label this clip “Paid partnership”.</p>}
              {ownBrand && !brandedContent && <p className="publish-hint">TikTok will label this clip “Promotional content”.</p>}
            </>}
          </div>
        </>}
      </div>
    </div>
    <div className="publish-checklist">
      <h3><ShieldCheck size={17} /> Final review</h3>
      <label><input type="checkbox" checked={watched} disabled={previewError} onChange={(event) => setWatched(event.target.checked)} /> I watched the final exported video.</label>
      <label><input type="checkbox" checked={reviewedCaption} onChange={(event) => setReviewedCaption(event.target.checked)} /> I reviewed the caption, hashtags, and claims.</label>
      <label><input type="checkbox" checked={reviewedDestination} onChange={(event) => setReviewedDestination(event.target.checked)} /> I reviewed the account, visibility, and interaction settings.</label>
      <label><input type="checkbox" checked={transferConsent} onChange={(event) => setTransferConsent(event.target.checked)} /> I agree to transfer this exported MP4 and caption to TikTok for publishing.</label>
      <label><input type="checkbox" checked={terms} onChange={(event) => setTerms(event.target.checked)} /> By posting, I agree to TikTok's <a href="https://www.tiktok.com/legal/page/global/music-usage-confirmation/en" target="_blank" rel="noreferrer">Music Usage Confirmation <ExternalLink size={12} /></a>{brandedContent ? <> and <a href="https://www.tiktok.com/legal/page/global/bc-policy/en" target="_blank" rel="noreferrer">Branded Content Policy <ExternalLink size={12} /></a></> : null}.</label>
      <div className="publish-actions">
        {job?.state.stage === "reviewed" ? <button type="button" className="primary-button" disabled={busy || platform !== "tiktok" || !allReviewed || previewError} onClick={publish}><Send size={15} /> Publish to TikTok</button> : <button type="button" className="replace-button" disabled={!canPrepare} onClick={prepare}>Confirm review</button>}
        {(job?.state.stage === "draft" || job?.state.stage === "reviewed" || job?.state.stage === "uploading") && <button type="button" className="replace-button" onClick={cancel}>Cancel</button>}
      </div>
      {job && <p className="publish-state" role="status">{job.state.stage === "uploading" ? "Uploading your reviewed MP4 to TikTok…" : job.state.stage === "processing" ? "TikTok is processing the video. Checking status every few seconds…" : job.state.stage === "published" ? `TikTok confirmed publication${job.state.post_id ? ` · Post ${job.state.post_id}` : " · Private post ID is not returned"}.` : job.state.stage === "failed" ? job.state.message : `Publication ${job.state.stage}.`}</p>}
      {error && <p className="publish-error" role="alert">{error}</p>}
    </div>
  </section>;
}
