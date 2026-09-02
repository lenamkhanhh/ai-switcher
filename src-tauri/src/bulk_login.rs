#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_bounded_email_password_totp_batch_without_returning_secrets() {
        let raw = "# local only\nfirst@example.com|secret-one|JBSWY3DPEHPK3PXP\nsecond@example.com|secret-two|\n";
        let count = validate_credentials_file(raw).expect("valid credentials file");
        assert_eq!(count, 2);
    }

    #[test]
    fn rejects_duplicate_email_without_echoing_password() {
        let raw = "first@example.com|super-secret|JBSWY3DPEHPK3PXP\nFIRST@example.com|another-secret|JBSWY3DPEHPK3PXP\n";
        let error = validate_credentials_file(raw).expect_err("duplicate must fail");
        let message = error.to_string();
        assert!(message.contains("duplicate"));
        assert!(!message.contains("super-secret"));
        assert!(!message.contains("another-secret"));
    }

    #[test]
    fn rejects_ambiguous_extra_fields() {
        let error = validate_credentials_file("user@example.com|password|totp|extra\n")
            .expect_err("extra field must fail");
        assert!(error.to_string().contains("exactly email|password|2fa"));
    }

    #[test]
    fn rejects_invalid_worker_limits() {
        assert!(validate_worker_limit(0).is_err());
        assert!(validate_worker_limit(4).is_err());
        assert!(validate_worker_limit(3).is_ok());
    }

    #[test]
    fn bridge_args_never_include_credentials() {
        let args = bridge_args(
            std::path::Path::new("D:/9router/auto_login.py"),
            std::path::Path::new("C:/Temp/accounts.txt"),
            std::path::Path::new("C:/Temp/oauth.json"),
            2,
            true,
        );
        let rendered = args
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert!(rendered.iter().any(|arg| arg == "--worker"));
        assert!(rendered.iter().any(|arg| arg == "--headed"));
        assert!(!rendered.iter().any(|arg| arg.contains("password")));
        assert!(!rendered.iter().any(|arg| arg.contains("totp")));
    }
}
