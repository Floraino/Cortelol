import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Check, KeyRound, ShieldCheck, Trash2 } from "lucide-react";
import "./SettingsWorkspace.css";

type Provider = "gemini" | "elevenlabs" | "claude" | "openai" | "deepseek";

const providers: { id: Provider; name: string; purpose: string }[] = [
  { id: "gemini", name: "Gemini", purpose: "Transcribe recordings" },
  { id: "elevenlabs", name: "ElevenLabs", purpose: "Transcribe recordings with Scribe v2" },
  { id: "claude", name: "Claude", purpose: "Find moments in the transcript" },
  { id: "openai", name: "OpenAI", purpose: "Find moments in the transcript" },
  { id: "deepseek", name: "DeepSeek", purpose: "Find moments in the transcript" },
];

function message(cause: unknown, fallback: string): string {
  return typeof cause === "string" ? cause : cause instanceof Error ? cause.message : fallback;
}

function ProviderKey({ id, name, purpose }: { id: Provider; name: string; purpose: string }) {
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
      .catch((cause) => { if (active) setError(message(cause, `Could not check the ${name} key.`)); })
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
      setNotice(`${name} key ${available ? "replaced" : "saved"}.`);
    } catch (cause) {
      setError(message(cause, `Could not save the ${name} key.`));
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
      setNotice(`${name} key removed from the credential store.`);
    } catch (cause) {
      setError(message(cause, `Could not remove the ${name} key.`));
    } finally {
      setBusy(false);
    }
  }

  return <section className="settings-provider" aria-labelledby={`settings-${id}-title`}>
    <div className="settings-provider-heading">
      <div>
        <h3 id={`settings-${id}-title`}>{name}</h3>
        <p>{purpose}</p>
      </div>
      <span className={available ? "settings-key-status saved" : "settings-key-status"}>
        {available ? <><Check size={14} aria-hidden="true" /> Key saved</> : loading ? "Checking…" : "No key saved"}
      </span>
    </div>

    <form className="settings-key-form" onSubmit={save}>
      <label htmlFor={`settings-${id}-key`}>{available ? `Replace ${name} API key` : `${name} API key`}</label>
      <div className="settings-key-controls">
        <input
          id={`settings-${id}-key`}
          type="password"
          autoComplete="off"
          spellCheck={false}
          value={key}
          onChange={(event) => { setKey(event.target.value); setNotice(null); }}
          placeholder={available ? "Enter a new key" : "Paste your API key"}
          disabled={busy || loading}
        />
        <button type="submit" className="primary-button" disabled={busy || loading || !key.trim()}>
          {busy ? "Saving…" : available ? "Replace key" : "Save key"}
        </button>
      </div>
    </form>

    {available && <div className="settings-remove">
      {confirmDelete ? <div className="settings-remove-confirm">
        <span>Remove the {name} key from this device?</span>
        <button type="button" className="settings-delete-button" onClick={remove} disabled={busy}>Yes, remove key</button>
        <button type="button" className="replace-button" onClick={() => setConfirmDelete(false)} disabled={busy}>Keep key</button>
      </div> : <button type="button" className="settings-remove-trigger" onClick={() => { setConfirmDelete(true); setError(null); setNotice(null); }} disabled={busy}>
        <Trash2 size={14} aria-hidden="true" /> Remove saved key
      </button>}
    </div>}
    {error && <p className="settings-message error" role="alert">{error}</p>}
    {notice && <p className="settings-message success" role="status">{notice}</p>}
  </section>;
}

export function SettingsWorkspace() {
  return <section className="settings-workspace" aria-labelledby="settings-title">
    <div className="settings-header">
      <div className="settings-header-icon"><KeyRound size={22} aria-hidden="true" /></div>
      <div>
        <span className="settings-eyebrow">PREFERENCES</span>
        <h1 id="settings-title">Settings</h1>
        <p>Manage the AI keys used by this device. Add them before importing a recording, or update them at any time.</p>
      </div>
    </div>

    <div className="settings-section-heading"><h2>AI providers</h2><span>Bring your own keys</span></div>
    <div className="settings-provider-list">
      {providers.map((provider) => <ProviderKey key={provider.id} {...provider} />)}
    </div>

    <aside className="settings-privacy">
      <ShieldCheck size={20} aria-hidden="true" />
      <div>
        <h2>Privacy and storage</h2>
        <p>Keys are stored in the operating system credential store. They are never shown again in the app. Recordings and exports stay on your device; AI processing sends only the data described at each job's consent step. Publishing is a manual upload from your exported file.</p>
      </div>
    </aside>
  </section>;
}
