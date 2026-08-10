//! Encrypted, explicitly unlocked credential vault.
//!
//! The vault is deliberately separate from the account profiles. OAuth/device-code credentials
//! stay in their existing profile locations; this module only stores values the user explicitly
//! enters (for example an email, password, and TOTP seed bought from a supplier).

use crate::models::{
    ToolId, VaultCredential, VaultCredentialInput, VaultCredentialSummary, VaultStatus,
};
use crate::store::Store;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::pbkdf2;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use zeroize::{Zeroize, Zeroizing};

const ENVELOPE_VERSION: u32 = 1;
const KDF_NAME: &str = "PBKDF2-HMAC-SHA256";
const PBKDF2_ITERATIONS: u32 = 600_000;
const KEY_LEN: usize = 32;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MIN_PASSKEY_BYTES: usize = 8;
const AAD: &[u8] = b"ai-account-switcher/credentials-vault/v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct VaultEnvelope {
    version: u32,
    kdf: String,
    iterations: u32,
    salt: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct VaultDocument {
    version: u32,
    records: BTreeMap<String, VaultRecord>,
}

impl VaultDocument {
    fn empty() -> Self {
        Self {
            version: ENVELOPE_VERSION,
            records: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct VaultRecord {
    tool_id: ToolId,
    account_id: String,
    username: String,
    password: String,
    totp_secret: String,
    source: String,
    notes: String,
    created_at: String,
    updated_at: String,
}

struct VaultSession {
    key: Zeroizing<[u8; KEY_LEN]>,
    salt: Zeroizing<[u8; SALT_LEN]>,
    document: VaultDocument,
    last_activity: Instant,
}

impl Drop for VaultSession {
    fn drop(&mut self) {
        for record in self.document.records.values_mut() {
            record.account_id.zeroize();
            record.username.zeroize();
            record.password.zeroize();
            record.totp_secret.zeroize();
            record.source.zeroize();
            record.notes.zeroize();
            record.created_at.zeroize();
            record.updated_at.zeroize();
        }
        self.document.records.clear();
        self.key.zeroize();
        self.salt.zeroize();
    }
}

/// Owns the encrypted vault file and the short-lived in-memory unlock session.
pub struct VaultService {
    path: PathBuf,
    session: Mutex<Option<VaultSession>>,
}

impl VaultService {
    pub fn new(store: &Store) -> Self {
        Self {
            path: store.vault_path(),
            session: Mutex::new(None),
        }
    }

    pub fn status(&self) -> Result<VaultStatus> {
        let mut guard = self.lock_session()?;
        self.expire_session(&mut guard);
        Ok(VaultStatus {
            configured: self.path.exists(),
            unlocked: guard.is_some(),
            record_count: guard
                .as_ref()
                .map(|session| session.document.records.len())
                .unwrap_or(0),
        })
    }

    pub fn initialize(&self, passkey: &str) -> Result<VaultStatus> {
        validate_passkey(passkey)?;
        if self.path.exists() {
            bail!("Secure vault is already configured");
        }

        let document = VaultDocument::empty();
        let salt = Zeroizing::new(random_bytes::<SALT_LEN>()?);
        let key = derive_key(passkey, &*salt)?;
        persist_document(&self.path, &key, &*salt, &document)?;

        let mut guard = self.lock_session()?;
        *guard = Some(VaultSession {
            key,
            salt,
            document,
            last_activity: Instant::now(),
        });
        Ok(status_for(&self.path, guard.as_ref()))
    }

    pub fn unlock(&self, passkey: &str) -> Result<VaultStatus> {
        validate_passkey(passkey)?;
        let envelope = read_envelope(&self.path)?;
        let salt = decode_exact(&envelope.salt, SALT_LEN)?;
        let key = derive_key(passkey, &salt)?;
        let document = decrypt_document(&key, &envelope)?;

        let mut guard = self.lock_session()?;
        *guard = Some(VaultSession {
            key,
            salt: Zeroizing::new(salt.try_into().expect("salt length validated")),
            document,
            last_activity: Instant::now(),
        });
        Ok(status_for(&self.path, guard.as_ref()))
    }

    pub fn lock(&self) {
        if let Ok(mut guard) = self.session.lock() {
            guard.take();
        }
    }

    pub fn list(&self) -> Result<Vec<VaultCredentialSummary>> {
        self.with_session(|session| {
            let mut records: Vec<_> = session.document.records.values().map(summary).collect();
            records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            Ok(records)
        })
    }

    pub fn get(&self, tool_id: &ToolId, account_id: &str) -> Result<Option<VaultCredential>> {
        validate_component(account_id, "account id")?;
        let key = record_key(tool_id, account_id);
        self.with_session(|session| {
            Ok(session
                .document
                .records
                .get(&key)
                .map(|record| VaultCredential {
                    tool_id: record.tool_id.clone(),
                    account_id: record.account_id.clone(),
                    username: record.username.clone(),
                    password: record.password.clone(),
                    totp_secret: record.totp_secret.clone(),
                    source: record.source.clone(),
                    notes: record.notes.clone(),
                    updated_at: record.updated_at.clone(),
                }))
        })
    }

    pub fn save(&self, mut input: VaultCredentialInput) -> Result<VaultCredentialSummary> {
        if let Err(error) = validate_input(&input) {
            input.password.zeroize();
            input.totp_secret.zeroize();
            return Err(error);
        }
        let key = record_key(&input.tool_id, &input.account_id);
        let now = chrono::Utc::now().to_rfc3339();

        let result = self.with_session(|session| {
            let previous = session.document.records.get(&key).cloned();
            let record = VaultRecord {
                tool_id: input.tool_id.clone(),
                account_id: input.account_id.clone(),
                username: input.username.trim().to_string(),
                password: input.password.clone(),
                totp_secret: input.totp_secret.trim().to_string(),
                source: input.source.trim().to_string(),
                notes: input.notes.trim().to_string(),
                created_at: previous
                    .as_ref()
                    .map(|item| item.created_at.clone())
                    .unwrap_or_else(|| now.clone()),
                updated_at: now.clone(),
            };
            session.document.records.insert(key.clone(), record);
            if let Err(error) =
                persist_document(&self.path, &session.key, &*session.salt, &session.document)
            {
                if let Some(previous) = previous {
                    session.document.records.insert(key.clone(), previous);
                } else {
                    session.document.records.remove(&key);
                }
                return Err(error);
            }
            Ok(summary(
                session
                    .document
                    .records
                    .get(&key)
                    .expect("record just inserted"),
            ))
        });
        input.password.zeroize();
        input.totp_secret.zeroize();
        result
    }

    pub fn delete(&self, tool_id: &ToolId, account_id: &str) -> Result<()> {
        validate_component(account_id, "account id")?;
        let key = record_key(tool_id, account_id);
        self.with_session(|session| {
            let Some(previous) = session.document.records.remove(&key) else {
                return Ok(());
            };
            if let Err(error) =
                persist_document(&self.path, &session.key, &*session.salt, &session.document)
            {
                session.document.records.insert(key.clone(), previous);
                return Err(error);
            }
            Ok(())
        })
    }

    /// Account deletion must not leave an encrypted orphan behind. A configured vault therefore
    /// requires an unlocked session before the account profile can be removed.
    pub fn ensure_unlocked_for_account_delete(&self) -> Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        self.with_session(|_| Ok(()))
    }

    fn lock_session(&self) -> Result<MutexGuard<'_, Option<VaultSession>>> {
        self.session
            .lock()
            .map_err(|_| anyhow::anyhow!("Secure vault state is unavailable"))
    }

    fn expire_session(&self, guard: &mut Option<VaultSession>) {
        if guard
            .as_ref()
            .is_some_and(|session| session.last_activity.elapsed() >= SESSION_IDLE_TIMEOUT)
        {
            guard.take();
        }
    }

    fn with_session<T>(&self, operation: impl FnOnce(&mut VaultSession) -> Result<T>) -> Result<T> {
        let mut guard = self.lock_session()?;
        self.expire_session(&mut guard);
        let session = guard
            .as_mut()
            .context("Secure vault is locked — unlock it first")?;
        session.last_activity = Instant::now();
        operation(session)
    }
}

fn status_for(path: &Path, session: Option<&VaultSession>) -> VaultStatus {
    VaultStatus {
        configured: path.exists(),
        unlocked: session.is_some(),
        record_count: session
            .map(|active| active.document.records.len())
            .unwrap_or(0),
    }
}

fn validate_passkey(passkey: &str) -> Result<()> {
    if passkey.as_bytes().len() < MIN_PASSKEY_BYTES {
        bail!("Vault passkey must be at least {MIN_PASSKEY_BYTES} characters");
    }
    if passkey.as_bytes().contains(&0) {
        bail!("Vault passkey contains an invalid character");
    }
    Ok(())
}

fn validate_component(value: &str, label: &str) -> Result<()> {
    if value.is_empty() || value.len() > 128 || value.chars().any(|ch| ch.is_control()) {
        bail!("Invalid {label}");
    }
    if value.contains('/') || value.contains('\\') {
        bail!("Invalid {label}");
    }
    Ok(())
}

fn validate_input(input: &VaultCredentialInput) -> Result<()> {
    validate_component(&input.account_id, "account id")?;
    if input.username.trim().is_empty() || input.username.len() > 320 {
        bail!("Username is required");
    }
    if input.password.is_empty() || input.password.len() > 4096 {
        bail!("Password is required");
    }
    if input.totp_secret.len() > 1024 {
        bail!("2FA secret is too long");
    }
    if input.source.len() > 200 || input.notes.len() > 2000 {
        bail!("Vault note is too long");
    }
    Ok(())
}

fn record_key(tool_id: &ToolId, account_id: &str) -> String {
    format!("{}:{account_id}", tool_id.as_str())
}

fn summary(record: &VaultRecord) -> VaultCredentialSummary {
    VaultCredentialSummary {
        tool_id: record.tool_id.clone(),
        account_id: record.account_id.clone(),
        username: record.username.clone(),
        source: record.source.clone(),
        notes: record.notes.clone(),
        updated_at: record.updated_at.clone(),
    }
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Secure random generation failed"))?;
    Ok(bytes)
}

fn derive_key(passkey: &str, salt: &[u8]) -> Result<Zeroizing<[u8; KEY_LEN]>> {
    let iterations = std::num::NonZeroU32::new(PBKDF2_ITERATIONS).expect("constant is nonzero");
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        salt,
        passkey.as_bytes(),
        &mut *key,
    );
    Ok(key)
}

fn encrypt_document(
    key: &[u8; KEY_LEN],
    salt: &[u8],
    document: &VaultDocument,
) -> Result<VaultEnvelope> {
    let mut ciphertext = Zeroizing::new(serde_json::to_vec(document)?);
    let nonce = random_bytes::<NONCE_LEN>()?;
    let unbound = UnboundKey::new(&aead::CHACHA20_POLY1305, key)
        .map_err(|_| anyhow::anyhow!("Vault encryption failed"))?;
    let less_safe = LessSafeKey::new(unbound);
    less_safe
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(AAD),
            &mut *ciphertext,
        )
        .map_err(|_| anyhow::anyhow!("Vault encryption failed"))?;
    Ok(VaultEnvelope {
        version: ENVELOPE_VERSION,
        kdf: KDF_NAME.to_string(),
        iterations: PBKDF2_ITERATIONS,
        salt: URL_SAFE_NO_PAD.encode(salt),
        nonce: URL_SAFE_NO_PAD.encode(nonce),
        ciphertext: URL_SAFE_NO_PAD.encode(&*ciphertext),
    })
}

fn persist_document(
    path: &Path,
    key: &[u8; KEY_LEN],
    salt: &[u8],
    document: &VaultDocument,
) -> Result<()> {
    let envelope = encrypt_document(key, salt, document)?;
    let encoded = serde_json::to_vec_pretty(&envelope)?;
    atomic_write(path, &encoded)
}

fn decrypt_document(key: &[u8; KEY_LEN], envelope: &VaultEnvelope) -> Result<VaultDocument> {
    if envelope.version != ENVELOPE_VERSION
        || envelope.kdf != KDF_NAME
        || envelope.iterations != PBKDF2_ITERATIONS
    {
        bail!("Vault could not be unlocked");
    }
    let nonce = decode_exact(&envelope.nonce, NONCE_LEN)?;
    let mut ciphertext = Zeroizing::new(decode_at_least(&envelope.ciphertext, 16)?);
    let unbound = UnboundKey::new(&aead::CHACHA20_POLY1305, key)
        .map_err(|_| anyhow::anyhow!("Vault could not be unlocked"))?;
    let less_safe = LessSafeKey::new(unbound);
    let plaintext = less_safe
        .open_in_place(
            Nonce::try_assume_unique_for_key(&nonce)
                .map_err(|_| anyhow::anyhow!("Vault could not be unlocked"))?,
            Aad::from(AAD),
            &mut *ciphertext,
        )
        .map_err(|_| anyhow::anyhow!("Vault could not be unlocked"))?;
    let document: VaultDocument = serde_json::from_slice(plaintext)
        .map_err(|_| anyhow::anyhow!("Vault could not be unlocked"))?;
    if document.version != ENVELOPE_VERSION {
        bail!("Vault could not be unlocked");
    }
    Ok(document)
}

fn read_envelope(path: &Path) -> Result<VaultEnvelope> {
    let text =
        fs::read_to_string(path).map_err(|_| anyhow::anyhow!("Secure vault is unavailable"))?;
    serde_json::from_str(&text).map_err(|_| anyhow::anyhow!("Vault could not be unlocked"))
}

fn decode_exact(value: &str, expected_len: usize) -> Result<Vec<u8>> {
    let decoded = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| anyhow::anyhow!("Vault could not be unlocked"))?;
    if decoded.len() != expected_len {
        bail!("Vault could not be unlocked");
    }
    Ok(decoded)
}

fn decode_at_least(value: &str, minimum_len: usize) -> Result<Vec<u8>> {
    let decoded = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| anyhow::anyhow!("Vault could not be unlocked"))?;
    if decoded.len() < minimum_len {
        bail!("Vault could not be unlocked");
    }
    Ok(decoded)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("Secure vault location is unavailable")?;
    fs::create_dir_all(parent).map_err(|_| anyhow::anyhow!("Secure vault is unavailable"))?;
    let tmp = path.with_extension(format!("vault.tmp{}", std::process::id()));
    fs::write(&tmp, bytes).map_err(|_| anyhow::anyhow!("Secure vault could not be saved"))?;
    set_owner_only_permissions(&tmp)?;
    fs::rename(&tmp, path).map_err(|_| anyhow::anyhow!("Secure vault could not be saved"))?;
    set_owner_only_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| anyhow::anyhow!("Secure vault permissions could not be set"))
}

#[cfg(not(unix))]
fn set_owner_only_permissions(path: &Path) -> Result<()> {
    crate::store::set_owner_only_permissions(path)
}

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
