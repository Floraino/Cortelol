use keyring::{Entry, Error};
use serde::{Deserialize, Serialize};

const SERVICE: &str = "app.cortelol.desktop";

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Gemini,
    Elevenlabs,
    Claude,
    Openai,
    Deepseek,
}

#[derive(Debug, Clone, Copy)]
pub enum SocialPlatform {
    TikTok,
    Instagram,
}

impl SocialPlatform {
    fn account(self) -> &'static str {
        match self {
            Self::TikTok => "tiktok-access-token",
            Self::Instagram => "instagram-access-token",
        }
    }
}

impl Provider {
    fn account(self) -> &'static str {
        match self {
            Self::Gemini => "gemini-api-key",
            Self::Elevenlabs => "elevenlabs-api-key",
            Self::Claude => "claude-api-key",
            Self::Openai => "openai-api-key",
            Self::Deepseek => "deepseek-api-key",
        }
    }
}

fn entry(provider: Provider) -> Result<Entry, String> {
    Entry::new(SERVICE, provider.account())
        .map_err(|_| "The operating system credential store is unavailable.".to_string())
}

fn social_entry(platform: SocialPlatform) -> Result<Entry, String> {
    Entry::new(SERVICE, platform.account())
        .map_err(|_| "The operating system credential store is unavailable.".to_string())
}

fn validate_key(key: &str) -> Result<&str, String> {
    let key = key.trim();
    if key.is_empty() || key.len() > 4096 || key.chars().any(char::is_control) {
        return Err("Enter a valid API key.".to_string());
    }
    Ok(key)
}

pub fn get_api_key(provider: Provider) -> Result<Option<String>, String> {
    match entry(provider)?.get_password() {
        Ok(key) => Ok(Some(key)),
        Err(Error::NoEntry) => Ok(None),
        Err(_) => Err("The API key could not be read from the credential store.".to_string()),
    }
}

pub fn get_social_token(platform: SocialPlatform) -> Result<Option<String>, String> {
    match social_entry(platform)?.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(Error::NoEntry) => Ok(None),
        Err(_) => Err("The social account credential could not be read.".to_string()),
    }
}

/// Called only after an official OAuth flow has exchanged its code in Rust.
#[allow(dead_code)]
pub fn save_social_token(platform: SocialPlatform, token: &str) -> Result<(), String> {
    let token = validate_key(token)?;
    social_entry(platform)?
        .set_password(token)
        .map_err(|_| "The social account credential could not be saved.".to_string())
}

#[tauri::command]
pub fn save_api_key(provider: Provider, key: String) -> Result<(), String> {
    let key = validate_key(&key)?;
    entry(provider)?
        .set_password(key)
        .map_err(|_| "The API key could not be saved to the credential store.".to_string())
}

#[tauri::command]
pub fn has_api_key(provider: Provider) -> Result<bool, String> {
    Ok(get_api_key(provider)?.is_some())
}

#[tauri::command]
pub fn delete_api_key(provider: Provider) -> Result<(), String> {
    match entry(provider)?.delete_credential() {
        Ok(()) | Err(Error::NoEntry) => Ok(()),
        Err(_) => Err("The API key could not be removed from the credential store.".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Provider, validate_key};

    #[test]
    fn rejects_empty_and_multiline_keys() {
        assert!(validate_key("   ").is_err());
        assert!(validate_key("abc\nsecret").is_err());
        assert!(validate_key("abc123").is_ok());
    }

    #[test]
    fn rejects_unknown_provider() {
        assert!(serde_json::from_str::<Provider>("\"unknown\"").is_err());
    }
}
