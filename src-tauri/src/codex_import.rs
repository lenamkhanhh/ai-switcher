use anyhow::{bail, Result};
use serde::Deserialize;
use serde_json::{Map, Value};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

/// Maximum size accepted for a K12 export. The supported export is a small JSON wrapper; keeping
/// this bounded prevents an accidental multi-gigabyte paste from becoming an IPC/memory issue.
pub(crate) const MAX_IMPORT_BYTES: usize = 5 * 1024 * 1024;

/// The only credential fields Codex needs. Secrets are wrapped in `Zeroizing` so the parsed
/// session does not linger in memory after the import operation returns.
pub(crate) struct ImportedCodexSession {
    pub(crate) account_id: String,
    pub(crate) email: Option<String>,
    pub(crate) access_token: Zeroizing<String>,
    pub(crate) refresh_token: Zeroizing<String>,
    pub(crate) id_token: Zeroizing<String>,
}

#[derive(Deserialize)]
struct K12Export {
    accounts: Vec<K12Account>,
}

#[derive(Deserialize)]
struct K12Account {
    platform: String,
    #[serde(rename = "type")]
    account_type: String,
    credentials: K12Credentials,
}

#[derive(Deserialize)]
struct K12Credentials {
    access_token: String,
    refresh_token: String,
    id_token: String,
    chatgpt_account_id: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    expires_at: Option<i64>,
}

impl Drop for K12Credentials {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
        self.id_token.zeroize();
        self.chatgpt_account_id.zeroize();
        self.email.zeroize();
    }
}

pub(crate) fn parse_k12_export(raw: &str) -> Result<ImportedCodexSession> {
    if raw.len() > MAX_IMPORT_BYTES {
        bail!("Session file is too large (maximum is 5 MB)");
    }

    let raw = raw.trim_start_matches('\u{feff}');
    let export: K12Export = serde_json::from_str(raw).map_err(|error| {
        anyhow::anyhow!("Session file does not match the K12 export format: {error}")
    })?;
    if export.accounts.len() != 1 {
        bail!("K12 export must contain exactly one account");
    }

    let account = &export.accounts[0];
    if !account.platform.eq_ignore_ascii_case("openai")
        && !account.platform.eq_ignore_ascii_case("codex")
    {
        bail!("Session file is not an OpenAI Codex export");
    }
    if !account.account_type.eq_ignore_ascii_case("oauth") {
        bail!("Only OAuth session exports can be imported");
    }

    let credentials = &account.credentials;
    let account_id = credentials.chatgpt_account_id.trim();
    if Uuid::parse_str(account_id).is_err() {
        bail!("The session is missing a valid ChatGPT account id");
    }

    // `expires_at` is intentionally not a hard rejection: an expired access token can still be
    // recoverable with a valid refresh token. The Codex quota reader performs the normal refresh.
    if credentials.expires_at.is_some_and(|value| value < 0) {
        bail!("The session expiry value is invalid");
    }

    let email = credentials
        .email
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    if email
        .as_ref()
        .is_some_and(|value| value.len() > 320 || !value.contains('@'))
    {
        bail!("The session email is invalid");
    }

    Ok(ImportedCodexSession {
        account_id: account_id.to_string(),
        email,
        access_token: required_secret(&credentials.access_token, "access_token")?,
        refresh_token: required_secret(&credentials.refresh_token, "refresh_token")?,
        id_token: required_secret(&credentials.id_token, "id_token")?,
    })
}

pub(crate) fn codex_auth_json(session: &ImportedCodexSession) -> Result<Zeroizing<Vec<u8>>> {
    let mut tokens = Map::new();
    tokens.insert(
        "id_token".to_string(),
        Value::String(session.id_token.to_string()),
    );
    tokens.insert(
        "access_token".to_string(),
        Value::String(session.access_token.to_string()),
    );
    tokens.insert(
        "refresh_token".to_string(),
        Value::String(session.refresh_token.to_string()),
    );
    tokens.insert(
        "account_id".to_string(),
        Value::String(session.account_id.clone()),
    );

    let mut auth = Map::new();
    auth.insert(
        "auth_mode".to_string(),
        Value::String("chatgpt".to_string()),
    );
    auth.insert("tokens".to_string(), Value::Object(tokens));

    serde_json::to_vec_pretty(&Value::Object(auth))
        .map(Zeroizing::new)
        .map_err(|_| anyhow::anyhow!("Couldn't encode Codex credentials"))
}

fn required_secret(value: &str, field: &str) -> Result<Zeroizing<String>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("The session is missing {field}");
    }
    if trimmed.len() > 16 * 1024 || trimmed.chars().any(char::is_control) {
        bail!("The session contains an invalid {field}");
    }
    Ok(Zeroizing::new(trimmed.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{codex_auth_json, parse_k12_export};
    use serde_json::Value;

    fn sample_export() -> String {
        r#"
        {
          "accounts": [{
            "platform": "openai",
            "type": "oauth",
            "credentials": {
              "access_token": "access-token",
              "refresh_token": "refresh-token",
              "id_token": "id-token",
              "chatgpt_account_id": "11111111-1111-4111-8111-111111111111",
              "email": "k12@example.test",
              "expires_at": 1786876906
            }
          }]
        }
        "#
        .to_string()
    }

    #[test]
    fn parses_single_openai_oauth_account() {
        let session = parse_k12_export(&sample_export()).expect("valid K12 export");

        assert_eq!(session.account_id, "11111111-1111-4111-8111-111111111111");
        assert_eq!(session.email.as_deref(), Some("k12@example.test"));
        assert_eq!(&*session.access_token, "access-token");
        assert_eq!(&*session.refresh_token, "refresh-token");
    }

    #[test]
    fn maps_only_codex_auth_fields() {
        let session = parse_k12_export(&sample_export()).expect("valid K12 export");
        let auth: Value = serde_json::from_slice(&codex_auth_json(&session).expect("auth JSON"))
            .expect("auth JSON");

        assert_eq!(
            auth.get("auth_mode").and_then(Value::as_str),
            Some("chatgpt")
        );
        assert_eq!(
            auth.get("tokens")
                .and_then(|tokens| tokens.get("account_id"))
                .and_then(Value::as_str),
            Some("11111111-1111-4111-8111-111111111111")
        );
        assert!(auth.get("OPENAI_API_KEY").is_none());
        assert!(auth.get("model_mapping").is_none());
    }

    #[test]
    fn rejects_multiple_accounts() {
        let mut export = sample_export();
        export = export.replace(
            "          }]\n        }",
            "          }, {\n            \"platform\": \"openai\",\n            \"type\": \"oauth\",\n            \"credentials\": {\n              \"access_token\": \"a\",\n              \"refresh_token\": \"r\",\n              \"id_token\": \"i\",\n              \"chatgpt_account_id\": \"22222222-2222-4222-8222-222222222222\"\n            }\n          }]\n        }",
        );

        let error = match parse_k12_export(&export) {
            Ok(_) => panic!("multiple accounts must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("exactly one"));
    }

    #[test]
    fn rejects_non_openai_or_missing_refresh_token() {
        let invalid_platform = sample_export().replace("\"openai\"", "\"anthropic\"");
        let error = match parse_k12_export(&invalid_platform) {
            Ok(_) => panic!("platform must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("OpenAI"));

        let missing_refresh = sample_export().replace("\"refresh_token\": \"refresh-token\",", "");
        let error = match parse_k12_export(&missing_refresh) {
            Ok(_) => panic!("refresh token is required"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("refresh_token"));
    }
}
