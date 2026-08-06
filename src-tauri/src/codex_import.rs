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
        let auth: Value = serde_json::from_slice(&codex_auth_json(&session)).expect("auth JSON");

        assert_eq!(auth.get("auth_mode").and_then(Value::as_str), Some("chatgpt"));
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
        export = export.replace("]\n        }", "]\n        }, {\n          \"platform\": \"openai\",\n          \"type\": \"oauth\",\n          \"credentials\": {\n            \"access_token\": \"a\",\n            \"refresh_token\": \"r\",\n            \"id_token\": \"i\",\n            \"chatgpt_account_id\": \"22222222-2222-4222-8222-222222222222\"\n          }\n        }]\n        }");

        let error = parse_k12_export(&export).expect_err("multiple accounts must be rejected");
        assert!(error.to_string().contains("exactly one"));
    }

    #[test]
    fn rejects_non_openai_or_missing_refresh_token() {
        let invalid_platform = sample_export().replace("\"openai\"", "\"anthropic\"");
        let error = parse_k12_export(&invalid_platform).expect_err("platform must be rejected");
        assert!(error.to_string().contains("OpenAI"));

        let missing_refresh = sample_export().replace("\"refresh_token\": \"refresh-token\",", "");
        let error = parse_k12_export(&missing_refresh).expect_err("refresh token is required");
        assert!(error.to_string().contains("refresh_token"));
    }
}
