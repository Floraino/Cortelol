import { useEffect, useState } from "react";
import { useTranslation } from "../../i18n";
import { invoke } from "@tauri-apps/api/core";
import { Check, KeyRound, ShieldCheck, Trash2 } from "lucide-react";
import "./SettingsWorkspace.css";

type Provider = "gemini" | "elevenlabs" | "claude" | "openai" | "deepseek";

const providers: { id: Provider; name: string }[] = [
  { id: "gemini", name: "Gemini" },
  { id: "elevenlabs", name: "ElevenLabs" },
  { id: "claude", name: "Claude" },
  { id: "openai", name: "OpenAI" },
  { id: "deepseek", name: "DeepSeek" },
];

function message(cause: unknown, fallback: string): string {
  return typeof cause === "string" ? cause : cause instanceof Error ? cause.message : fallback;
}

function ProviderKey({ id, name }: { id: Provider; name: string }) {
  const { t } = useTranslation();
  const [loading, setLoading] = useState(true);
  const [available, setAvailable] = useState(false);
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    invoke<boolean>("has_api_key", { provider: id })
      .then((stored) => { if (active) setAvailable(stored); })
      .catch((cause) => { if (active) setError(message(cause, t("settings.checkKeyError", { provider: name }))); })
      .finally(() => { if (active) setLoading(false); });
    return () => { active = false; };
  }, [id, name]);

  async function save(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!key.trim() || busy) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    setConfirmDelete(false);
    try {
      await invoke("save_api_key", { provider: id, key: key.trim() });
      setKey("");
      setAvailable(true);
      setNotice(t(available ? "settings.keyReplaced" : "settings.keySavedNotice", { provider: name }));
    } catch (cause) {
      setError(message(cause, t("settings.saveKeyError", { provider: name })));
    } finally {
      setBusy(false);
    }
  }

  async function remove() {
    if (busy) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await invoke("delete_api_key", { provider: id });
      setAvailable(false);
      setConfirmDelete(false);
      setKey("");
      setNotice(t("settings.keyRemoved", { provider: name }));
    } catch (cause) {
      setError(message(cause, t("settings.removeKeyError", { provider: name })));
    } finally {
      setBusy(false);
    }
  }

  return <section className="settings-provider" aria-labelledby={`settings-${id}-title`}>
    <div className="settings-provider-heading">
      <div>
        <h3 id={`settings-${id}-title`}>{name}</h3>
        <p>{t(`settings.purpose.${id}`)}</p>
      </div>
      <span className={available ? "settings-key-status saved" : "settings-key-status"}>
        {available ? <><Check size={14} aria-hidden="true" /> {t("settings.key.saved")}</> : loading ?t("settings.checking") :t("settings.no.key.saved")}
      </span>
    </div>

    <form className="settings-key-form" onSubmit={save}>
      <label htmlFor={`settings-${id}-key`}>{t(available ? "settings.replaceProviderKey" : "settings.providerKey", { provider: name })}</label>
      <div className="settings-key-controls">
        <input
          id={`settings-${id}-key`}
          type="password"
          autoComplete="off"
          spellCheck={false}
          value={key}
          onChange={(event) => { setKey(event.target.value); setNotice(null); }}
          placeholder={available ?t("settings.enter.a.new.key") :t("settings.paste.your.api.key")}
          disabled={busy || loading}
        />
        <button type="submit" className="primary-button" disabled={busy || loading || !key.trim()}>
          {busy ?t("settings.saving") : available ?t("settings.replace.key") :t("settings.save.key")}
        </button>
      </div>
    </form>

    {available && <div className="settings-remove">
      {confirmDelete ? <div className="settings-remove-confirm">
        <span>{t("settings.removeProviderKey", { provider: name })}</span>
        <button type="button" className="settings-delete-button" onClick={remove} disabled={busy}>{t("settings.yes.remove.key")}</button>
        <button type="button" className="replace-button" onClick={() => setConfirmDelete(false)} disabled={busy}>{t("settings.keep.key")}</button>
      </div> : <button type="button" className="settings-remove-trigger" onClick={() => { setConfirmDelete(true); setError(null); setNotice(null); }} disabled={busy}>
        <Trash2 size={14} aria-hidden="true" /> {t("settings.remove.saved.key")}
      </button>}
    </div>}
    {error && <p className="settings-message error" role="alert">{error}</p>}
    {notice && <p className="settings-message success" role="status">{notice}</p>}
  </section>;
}

export function SettingsWorkspace() {
  const { t } = useTranslation();
  return <section className="settings-workspace" aria-labelledby="settings-title">
    <div className="settings-header">
      <div className="settings-header-icon"><KeyRound size={22} aria-hidden="true" /></div>
      <div>
        <span className="settings-eyebrow">{t("settings.preferences")}</span>
        <h1 id="settings-title">{t("settings.settings")}</h1>
        <p>{t("settings.manage.the.ai.keys.used")}</p>
      </div>
    </div>

    <div className="settings-section-heading"><h2>{t("settings.ai.providers")}</h2><span>{t("settings.bring.your.own.keys")}</span></div>
    <div className="settings-provider-list">
      {providers.map((provider) => <ProviderKey key={provider.id} {...provider} />)}
    </div>

    <aside className="settings-privacy">
      <ShieldCheck size={20} aria-hidden="true" />
      <div>
        <h2>{t("settings.privacy.and.storage")}</h2>
        <p>{t("settings.keys.are.stored.in.the")}</p>
      </div>
    </aside>
  </section>;
}
