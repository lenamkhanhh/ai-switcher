//! Encrypted, explicitly unlocked credential vault.
//!
//! The tests in this module define the security contract before the implementation is added:
//! records must round-trip only after unlocking, ciphertext must not contain the supplied
//! secrets, edits/deletes must be account-scoped, and malformed/tampered data must fail closed.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ToolId;
    use crate::store::Store;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_store(name: &str) -> Store {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Store::for_test(std::env::temp_dir().join(format!("aisw-vault-{name}-{unique}"))).unwrap()
    }

    fn sample(account_id: &str) -> VaultCredentialInput {
        VaultCredentialInput {
            tool_id: ToolId::Codex,
            account_id: account_id.to_string(),
            username: "test@example.invalid".to_string(),
            password: "fixture-password-only".to_string(),
            totp_secret: "JBSWY3DPEHPK3PXP".to_string(),
            source: "fixture supplier".to_string(),
            notes: "test fixture; not a real account".to_string(),
        }
    }

    #[test]
    fn initializes_unlocks_and_keeps_secrets_out_of_ciphertext() {
        let store = temp_store("roundtrip");
        let vault = VaultService::new(&store);

        let status = vault.initialize("fixture-passkey-123").unwrap();
        assert!(status.configured);
        assert!(status.unlocked);
        vault.save(sample("account-1")).unwrap();
        vault.lock();

        let ciphertext = std::fs::read(store.vault_path()).unwrap();
        assert!(!String::from_utf8_lossy(&ciphertext).contains("fixture-password-only"));
        assert!(!String::from_utf8_lossy(&ciphertext).contains("JBSWY3DPEHPK3PXP"));

        assert!(vault.unlock("wrong-passkey").is_err());
        assert!(vault.get(&ToolId::Codex, "account-1").is_err());
        vault.unlock("fixture-passkey-123").unwrap();
        let record = vault.get(&ToolId::Codex, "account-1").unwrap().unwrap();
        assert_eq!(record.password, "fixture-password-only");
        assert_eq!(record.totp_secret, "JBSWY3DPEHPK3PXP");

        let _ = std::fs::remove_dir_all(store.root_dir());
    }

    #[test]
    fn updates_are_account_scoped_and_delete_removes_the_record() {
        let store = temp_store("delete");
        let vault = VaultService::new(&store);
        vault.initialize("fixture-passkey-123").unwrap();
        vault.save(sample("account-1")).unwrap();
        vault.save(sample("account-2")).unwrap();

        let mut edited = sample("account-1");
        edited.password = "updated-fixture-password".to_string();
        vault.save(edited).unwrap();
        assert_eq!(vault.list().unwrap().len(), 2);
        assert_eq!(
            vault
                .get(&ToolId::Codex, "account-1")
                .unwrap()
                .unwrap()
                .password,
            "updated-fixture-password"
        );

        vault.delete(&ToolId::Codex, "account-1").unwrap();
        assert!(vault.get(&ToolId::Codex, "account-1").unwrap().is_none());
        assert!(vault.get(&ToolId::Codex, "account-2").unwrap().is_some());

        let _ = std::fs::remove_dir_all(store.root_dir());
    }

    #[test]
    fn rejects_weak_passkeys_invalid_ids_and_tampering() {
        let store = temp_store("validation");
        let vault = VaultService::new(&store);
        assert!(vault.initialize("short").is_err());
        vault.initialize("fixture-passkey-123").unwrap();

        let mut invalid = sample("../escape");
        assert!(vault.save(invalid.clone()).is_err());
        invalid.account_id = "".to_string();
        assert!(vault.save(invalid).is_err());

        let path = store.vault_path();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(b'x');
        std::fs::write(&path, bytes).unwrap();
        vault.lock();
        assert!(vault.unlock("fixture-passkey-123").is_err());

        let _ = std::fs::remove_dir_all(store.root_dir());
    }
}
