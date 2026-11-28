//! Secure storage and validation for ElevenLabs API credentials.
//!
//! The API key is stored in the platform OS keyring via `keyring_core`, with a
//! fallback to the `ELEVENLABS_API_KEY` environment variable. It is never
//! written to `settings.json` or emitted to application logs.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use keyring_core::api::CredentialStoreApi;
use zeroize::Zeroizing;

const KEYRING_SERVICE: &str = "rocks.zapfast.ZapFast";
const KEYRING_USER: &str = "elevenlabs_api_key";
const ENV_VAR: &str = "ELEVENLABS_API_KEY";
const SIGNED_URL_ENDPOINT: &str =
    "https://api.elevenlabs.io/v1/convai/conversation/get-signed-url";

/// Where the active ElevenLabs API key was loaded from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiKeySource {
    Keyring,
    EnvVar,
}

fn platform_store() -> Result<Arc<dyn CredentialStoreApi>> {
    #[cfg(target_os = "linux")]
    let store = zbus_secret_service_keyring_store::Store::new();
    #[cfg(target_os = "macos")]
    let store = apple_native_keyring_store::keychain::Store::new();
    #[cfg(windows)]
    let store = windows_native_keyring_store::Store::new();
    Ok(store
        .map_err(|error| anyhow::anyhow!("{error}"))
        .context("Could not open the OS keyring")? as Arc<dyn CredentialStoreApi>)
}

fn normalize_key(raw: &str) -> Option<Zeroizing<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(Zeroizing::new(trimmed.to_owned()))
    }
}

fn load_api_key_with(
    store: &dyn CredentialStoreApi,
    env_fallback: Option<&str>,
) -> Result<Option<(Zeroizing<String>, ApiKeySource)>> {
    let entry = store
        .build(KEYRING_SERVICE, KEYRING_USER, None)
        .map_err(|error| anyhow::anyhow!("{error}"))
        .context("Could not open ZapFast's ElevenLabs keyring entry")?;
    match entry.get_secret() {
        Ok(bytes) => {
            let text = Zeroizing::new(
                String::from_utf8(bytes)
                    .context("Stored ElevenLabs API key in OS keyring is not valid UTF-8")?,
            );
            if let Some(key) = normalize_key(&text) {
                return Ok(Some((key, ApiKeySource::Keyring)));
            }
        }
        Err(keyring_core::Error::NoEntry) => {}
        Err(error) => {
            return Err(anyhow::anyhow!("{error}"))
                .context("Could not read ElevenLabs API key from OS keyring");
        }
    }

    Ok(env_fallback
        .and_then(normalize_key)
        .map(|key| (key, ApiKeySource::EnvVar)))
}

/// Loads the ElevenLabs API key from the OS keyring, falling back to
/// `ELEVENLABS_API_KEY`.
pub fn load_api_key() -> Result<Option<(Zeroizing<String>, ApiKeySource)>> {
    let env_val = std::env::var(ENV_VAR).ok();
    match platform_store() {
        Ok(store) => load_api_key_with(&*store, env_val.as_deref()),
        Err(_) => Ok(env_val
            .as_deref()
            .and_then(normalize_key)
            .map(|key| (key, ApiKeySource::EnvVar))),
    }
}

/// Saves the ElevenLabs API key to the OS keyring. An empty/whitespace key
/// clears any stored entry.
pub fn save_api_key(key: &str) -> Result<()> {
    let Some(trimmed) = normalize_key(key) else {
        return clear_api_key();
    };
    let store = platform_store()?;
    let entry = store
        .build(KEYRING_SERVICE, KEYRING_USER, None)
        .map_err(|error| anyhow::anyhow!("{error}"))
        .context("Could not open ZapFast's ElevenLabs keyring entry")?;
    entry
        .set_secret(trimmed.as_bytes())
        .map_err(|error| anyhow::anyhow!("{error}"))
        .context("Could not save ElevenLabs API key in OS keyring")?;
    Ok(())
}

/// Removes any stored ElevenLabs API key from the OS keyring.
pub fn clear_api_key() -> Result<()> {
    let store = platform_store()?;
    let entry = store
        .build(KEYRING_SERVICE, KEYRING_USER, None)
        .map_err(|error| anyhow::anyhow!("{error}"))
        .context("Could not open ZapFast's ElevenLabs keyring entry")?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(error) => Err(anyhow::anyhow!("{error}"))
            .context("Could not remove ElevenLabs API key from OS keyring"),
    }
}

/// Validates an ElevenLabs API key and Agent ID by requesting a signed
/// Conversational AI WebSocket URL. Never logs the API key or returned URL.
pub fn test_connection(api_key: &str, agent_id: &str) -> Result<String> {
    let Some(api_key) = normalize_key(api_key) else {
        bail!("Enter an ElevenLabs API key first.");
    };
    let agent_id = agent_id.trim();
    if agent_id.is_empty() {
        bail!("Enter an ElevenLabs Agent ID first.");
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("Could not create HTTP client")?;

    let response = client
        .get(SIGNED_URL_ENDPOINT)
        .query(&[("agent_id", agent_id)])
        .header("xi-api-key", api_key.as_str())
        .send()
        .context("Could not reach ElevenLabs API")?;

    let status = response.status();
    if !status.is_success() {
        bail!("ElevenLabs rejected the request (HTTP {status}). Check your API key and Agent ID.");
    }

    let raw = response
        .text()
        .context("Could not read ElevenLabs response body")?;
    let body: serde_json::Value =
        serde_json::from_str(&raw).context("ElevenLabs returned an unexpected response")?;
    let has_url = body
        .get("signed_url")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|url| url.starts_with("wss://"));
    if !has_url {
        bail!("ElevenLabs response did not contain a valid signed WebSocket URL.");
    }

    Ok("Connected to ElevenLabs Agent.".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_key_trims_and_rejects_blank() {
        assert_eq!(normalize_key("  xi_test  ").as_deref().map(String::as_str), Some("xi_test"));
        assert!(normalize_key("   ").is_none());
        assert!(normalize_key("").is_none());
    }

    #[test]
    fn test_connection_rejects_missing_inputs_without_network() {
        let err = test_connection("", "agent_123").unwrap_err();
        assert!(err.to_string().contains("API key"));

        let err = test_connection("xi_test", "  ").unwrap_err();
        assert!(err.to_string().contains("Agent ID"));
    }

    #[test]
    fn mock_store_prefers_keyring_and_falls_back_to_env() {
        let store = keyring_core::mock::Store::new().unwrap();
        let none = load_api_key_with(&*store, None).unwrap();
        assert!(none.is_none());

        let from_env = load_api_key_with(&*store, Some("  env_key  ")).unwrap();
        assert_eq!(
            from_env.as_ref().map(|(k, s)| (k.as_str(), *s)),
            Some(("env_key", ApiKeySource::EnvVar))
        );

        let entry = store.build(KEYRING_SERVICE, KEYRING_USER, None).unwrap();
        entry.set_secret(b"keyring_key").unwrap();

        let from_keyring = load_api_key_with(&*store, Some("env_key")).unwrap();
        assert_eq!(
            from_keyring.as_ref().map(|(k, s)| (k.as_str(), *s)),
            Some(("keyring_key", ApiKeySource::Keyring))
        );
    }
}
