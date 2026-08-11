use crate::models::{
    Account, AccountState, ApiGatewayConfig, AutoPingSetting, AutoPrimeSetting, AutoSwitchSetting,
    ResumeJob, ToolId, ToolSetup,
};
use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredState {
    pub disclaimer_accepted: bool,
    pub accounts: Vec<Account>,
    /// Auto-switch the account in use (plain command) when it hits the quota threshold.
    #[serde(default)]
    pub auto_switch: bool,
    /// The % used that triggers auto-switch (default 100 = fully exhausted).
    #[serde(default = "default_threshold")]
    pub auto_switch_threshold: f64,
    /// Per-tool auto-switch settings. Claude/Codex are independent; Antigravity is not supported.
    #[serde(default)]
    pub auto_switch_settings: BTreeMap<String, AutoSwitchSetting>,
    /// Per-account "auto session prime" schedules, keyed by account id. Only subscription
    /// (OAuth) Claude/Codex accounts are eligible — API-proxy accounts have no 5h window.
    #[serde(default)]
    pub auto_prime: BTreeMap<String, AutoPrimeSetting>,
    #[serde(default)]
    pub auto_ping_enabled: bool,
    #[serde(default)]
    pub launch_with_windows: bool,
    #[serde(default)]
    pub auto_ping: BTreeMap<String, AutoPingSetting>,
    #[serde(default)]
    pub resume_jobs: Vec<ResumeJob>,
    #[serde(default = "current_schema_version")]
    pub schema_version: u32,
    /// Resolved CLI binary/config dirs per tool. Missing = detect on startup / ask user.
    #[serde(default)]
    pub tool_setups: BTreeMap<String, ToolSetup>,
    /// Local OpenAI/Anthropic-compatible proxy settings for the API tab.
    #[serde(default)]
    pub api_gateway: ApiGatewayConfig,
}

fn default_threshold() -> f64 {
    92.0
}

/// Serializes every write to `state.json` inside this process.
static STATE_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(unix)]
pub(crate) fn set_owner_only_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn set_owner_only_permissions(path: &std::path::Path) -> Result<()> {
    apply_windows_acl(path)
}

#[cfg(unix)]
pub(crate) fn set_owner_only_directory_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn set_owner_only_directory_permissions(path: &std::path::Path) -> Result<()> {
    apply_windows_acl(path)
}

#[cfg(windows)]
fn windows_system_binary(name: &str) -> Result<std::path::PathBuf> {
    let root = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .context("Windows system directory is unavailable")?;
    Ok(root.join("System32").join(name))
}

#[cfg(windows)]
fn hidden_windows_command(path: std::path::PathBuf) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let mut command = std::process::Command::new(path);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(windows)]
fn windows_current_principal() -> Result<String> {
    static PRINCIPAL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(principal) = PRINCIPAL.get() {
        return Ok(principal.clone());
    }

    let output = hidden_windows_command(windows_system_binary("whoami.exe")?)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .context("Couldn't identify the Windows user")?;
    if !output.status.success() {
        anyhow::bail!("Couldn't identify the Windows user");
    }
    let principal = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if principal.is_empty() {
        anyhow::bail!("Couldn't identify the Windows user");
    }
    let _ = PRINCIPAL.set(principal.clone());
    Ok(principal)
}

#[cfg(windows)]
fn apply_windows_acl(path: &std::path::Path) -> Result<()> {
    let principal = windows_current_principal()?;
    let status = hidden_windows_command(windows_system_binary("icacls.exe")?)
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{principal}:F"))
        .arg("*S-1-5-18:F")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("Couldn't secure persisted app data")?;
    if !status.success() {
        anyhow::bail!("Couldn't secure persisted app data");
    }
    Ok(())
}

fn lock_state_writes() -> std::sync::MutexGuard<'static, ()> {
    // A previous writer panicking mid-save must not make the state file permanently unwritable.
    STATE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Default for StoredState {
    fn default() -> Self {
        Self {
            disclaimer_accepted: false,
            accounts: Vec::new(),
            auto_switch: false,
            auto_switch_threshold: default_threshold(),
            auto_switch_settings: BTreeMap::new(),
            auto_prime: BTreeMap::new(),
            auto_ping_enabled: false,
            launch_with_windows: false,
            auto_ping: BTreeMap::new(),
            resume_jobs: Vec::new(),
            schema_version: current_schema_version(),
            tool_setups: BTreeMap::new(),
            api_gateway: ApiGatewayConfig::default(),
        }
    }
}

fn current_schema_version() -> u32 {
    // Version 4 retires legacy recovery statuses that otherwise remain visible forever.
    4
}

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new() -> Result<Self> {
        let dirs = ProjectDirs::from("dev", "hoangphan", "AI Account Switcher")
            .context("Couldn't determine the app data directory")?;
        let root = dirs.data_local_dir().to_path_buf();
        fs::create_dir_all(root.join("accounts"))?;
        fs::create_dir_all(root.join("backups"))?;
        cleanup_stale_state_staging_files(&root)?;
        set_owner_only_directory_permissions(&root)?;
        set_owner_only_directory_permissions(&root.join("accounts"))?;
        set_owner_only_directory_permissions(&root.join("backups"))?;
        for secret_path in [root.join("state.json"), root.join("state.json.bak")] {
            if secret_path.exists() {
                set_owner_only_permissions(&secret_path)?;
            }
        }
        Ok(Self { root })
    }

    pub fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }

    /// Encrypted user-entered credentials. The file is never included in the normal app snapshot.
    pub fn vault_path(&self) -> PathBuf {
        self.root.join("credentials.vault")
    }

    /// Incremental token-usage cache (per-file cursors + aggregated buckets).
    pub fn usage_cache_path(&self) -> PathBuf {
        self.root.join("usage.json")
    }

    /// Cached copy of LiteLLM's pricing dataset (refreshed at most daily).
    pub fn price_cache_path(&self) -> PathBuf {
        self.root.join("litellm_prices.json")
    }

    /// Usage produced by the local API gateway only. Kept separate from `usage.json`
    /// because CLI JSONL scans may also see some proxy-originated requests.
    pub fn api_usage_path(&self) -> PathBuf {
        self.root.join("api_usage.json")
    }

    /// Best-effort Codex handoff prompts generated after auto-switches.
    pub fn handoff_dir(&self) -> PathBuf {
        self.root.join("handoffs")
    }

    /// The root data dir. Only used by tests (to clean their temp store).
    #[cfg(test)]
    pub fn root_dir(&self) -> &std::path::Path {
        &self.root
    }

    /// The tool's accounts root (`accounts/<tool>/`), holding one dir per profile account.
    pub fn tool_accounts_root(&self, tool_id: &ToolId) -> PathBuf {
        self.root.join("accounts").join(tool_id.as_str())
    }

    pub fn account_dir(&self, tool_id: &ToolId, account_id: &str) -> PathBuf {
        self.root
            .join("accounts")
            .join(tool_id.as_str())
            .join(account_id)
    }

    pub fn active_profile_path(&self, tool_id: &ToolId) -> PathBuf {
        self.root
            .join("active")
            .join(format!("{}.profile", tool_id.as_str()))
    }

    pub fn codex_machine_default_auth_backup(&self) -> PathBuf {
        self.root
            .join("machine-default")
            .join("codex")
            .join("auth.json")
    }

    pub fn codex_machine_default_auth_missing_marker(&self) -> PathBuf {
        self.root
            .join("machine-default")
            .join("codex")
            .join("auth.missing")
    }

    /// Copy of the last successfully written state, used to recover if `state.json` is left
    /// truncated or spliced by a crash.
    pub fn state_backup_path(&self) -> PathBuf {
        self.root.join("state.json.bak")
    }

    pub fn load(&self) -> Result<StoredState> {
        let path = self.state_path();
        if !path.exists() {
            return Ok(StoredState::default());
        }
        let text = fs::read_to_string(&path)?;
        match serde_json::from_str(&text) {
            Ok(state) => Ok(state),
            Err(error) => fs::read_to_string(self.state_backup_path())
                .ok()
                .and_then(|backup| serde_json::from_str(&backup).ok())
                .ok_or(error)
                .context("state.json is unreadable and the backup can't be used either"),
        }
    }

    pub fn save(&self, state: &StoredState) -> Result<()> {
        let _guard = lock_state_writes();
        // Gateway workers persist live cooldown/error state independently from the app mutex.
        // Preserve those newer runtime fields when an older UI snapshot is saved, while still
        // allowing the caller to remove participation entries for deleted accounts.
        let mut merged = state.clone();
        if let Ok(latest) = self.load() {
            merge_gateway_runtime_state(&mut merged, &latest);
        }
        self.write_state(&merged)
    }

    /// Load, mutate and save while holding the write lock, so two threads can't each read the
    /// same state, apply a different change, and have one silently overwrite the other. The API
    /// gateway persists account state from its own workers without the app's state mutex, so
    /// plain `load` + `save` there would lose updates.
    pub fn update<F>(&self, mutate: F) -> Result<StoredState>
    where
        F: FnOnce(&mut StoredState),
    {
        let _guard = lock_state_writes();
        let mut data = self.load()?;
        mutate(&mut data);
        self.write_state(&data)?;
        Ok(data)
    }

    /// Caller must hold the state write lock.
    fn write_state(&self, state: &StoredState) -> Result<()> {
        let path = self.state_path();
        let encoded = serde_json::to_vec_pretty(state)?;
        // Process-unique staging file: a shared fixed name lets a second writer truncate and
        // refill the file we are about to rename into place.
        let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
        fs::write(&tmp, &encoded)?;
        set_owner_only_permissions(&tmp)?;
        if path.exists() {
            let backup = self.state_backup_path();
            if fs::copy(&path, &backup).is_ok() {
                set_owner_only_permissions(&backup)?;
            }
        }
        fs::rename(&tmp, &path)?;
        set_owner_only_permissions(&path)?;
        Ok(())
    }

    #[cfg(test)]
    pub fn for_test(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("accounts"))?;
        fs::create_dir_all(root.join("backups"))?;
        Ok(Self { root })
    }
}

fn cleanup_stale_state_staging_files(root: &std::path::Path) -> Result<()> {
    const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(60 * 60);
    let now = std::time::SystemTime::now();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(process_suffix) = name.strip_prefix("state.json.tmp") else {
            continue;
        };
        if process_suffix.is_empty() || !process_suffix.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let metadata = entry.metadata()?;
        let old_enough = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= STALE_AFTER);
        if old_enough && metadata.is_file() {
            match fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).context("Couldn't remove stale state staging file")
                }
            }
        }
    }
    Ok(())
}

fn merge_gateway_runtime_state(incoming: &mut StoredState, latest: &StoredState) {
    let account_exists = |tool_id: &ToolId, account_id: &str| {
        incoming
            .accounts
            .iter()
            .any(|account| &account.tool_id == tool_id && account.id == account_id)
    };

    incoming
        .api_gateway
        .accounts
        .retain(|entry| account_exists(&entry.tool_id, &entry.account_id));

    for latest_entry in latest
        .api_gateway
        .accounts
        .iter()
        .filter(|entry| account_exists(&entry.tool_id, &entry.account_id))
    {
        if let Some(current) = incoming.api_gateway.accounts.iter_mut().find(|entry| {
            entry.tool_id == latest_entry.tool_id && entry.account_id == latest_entry.account_id
        }) {
            current.state = latest_entry.state.clone();
            current.cooldown_until = latest_entry.cooldown_until.clone();
            current.error = latest_entry.error.clone();
        } else {
            incoming.api_gateway.accounts.push(latest_entry.clone());
        }
    }
}

pub fn normalize_account_states(
    accounts: &mut [Account],
    tool_id: &ToolId,
    active_id: Option<&str>,
) {
    for account in accounts.iter_mut().filter(|item| &item.tool_id == tool_id) {
        account.state = if Some(account.id.as_str()) == active_id {
            if account.state == AccountState::Exhausted {
                AccountState::Exhausted
            } else {
                AccountState::Active
            }
        } else if account.state == AccountState::Active {
            AccountState::Idle
        } else {
            account.state.clone()
        };
    }
}

/// Narrow wrappers keep recovery-manifest permission calls independent from the store's
/// platform-specific implementation and from other account-data permission callers.
pub(crate) fn set_recovery_manifest_permissions(path: &std::path::Path) -> Result<()> {
    set_owner_only_permissions(path)
}

pub(crate) fn set_recovery_manifest_directory_permissions(path: &std::path::Path) -> Result<()> {
    set_owner_only_directory_permissions(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ApiGatewayAccount, ApiPoolAccountState, ToolId};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_store(name: &str) -> Store {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("aisw-store-{name}-{unique}"));
        Store::for_test(root).unwrap()
    }

    #[test]
    fn startup_cleanup_removes_only_abandoned_state_staging_files() {
        let store = temp_store("stale-stage");
        let root = store.root_dir();
        let stale = root.join("state.json.tmp12345");
        let unrelated = root.join("state.json.tmp-not-a-process");
        fs::write(&stale, b"stale").unwrap();
        fs::write(&unrelated, b"keep").unwrap();
        let old = SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
        fs::OpenOptions::new()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();

        cleanup_stale_state_staging_files(root).unwrap();

        assert!(!stale.exists());
        assert!(unrelated.exists());
        let _ = fs::remove_dir_all(root);
    }

    /// A crash mid-write must not cost the user every account: the previous good state is kept
    /// alongside and used when `state.json` no longer parses.
    #[test]
    fn corrupt_state_falls_back_to_the_last_good_copy() {
        let store = temp_store("corrupt");
        let mut data = StoredState::default();
        data.disclaimer_accepted = true;
        store.save(&data).unwrap();
        // A second save is what creates the backup of the first.
        data.auto_switch = true;
        store.save(&data).unwrap();

        // Simulate a save that was interrupted after truncating the file.
        fs::write(store.state_path(), b"{\"disclaimerAccepted\": tr").unwrap();

        let recovered = store.load().expect("should recover from the backup");
        assert!(recovered.disclaimer_accepted);
        let _ = fs::remove_dir_all(store.root_dir());
    }

    #[test]
    fn update_applies_and_persists_the_mutation() {
        let store = temp_store("update");
        store.save(&StoredState::default()).unwrap();

        let returned = store
            .update(|data| {
                data.auto_switch_threshold = 77.0;
            })
            .unwrap();

        assert_eq!(returned.auto_switch_threshold, 77.0);
        assert_eq!(store.load().unwrap().auto_switch_threshold, 77.0);
        let _ = fs::remove_dir_all(store.root_dir());
    }

    #[test]
    fn save_preserves_newer_gateway_runtime_state() {
        let store = temp_store("gateway-runtime-merge");
        let mut initial = StoredState::default();
        initial.accounts.push(Account {
            id: "acc1".to_string(),
            tool_id: ToolId::Claude,
            name: "acc1".to_string(),
            state: AccountState::Idle,
            fingerprint: "fingerprint".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_used_at: None,
            quota: None,
            launcher_command: None,
            launcher_paths: None,
            is_default: false,
            avatar_url: None,
            account_email: None,
            api_provider: None,
            quota_status: None,
            codex_login_method: Default::default(),
        });
        initial.api_gateway.accounts.push(ApiGatewayAccount {
            tool_id: ToolId::Claude,
            account_id: "acc1".to_string(),
            enabled: true,
            state: ApiPoolAccountState::Available,
            cooldown_until: None,
            error: None,
        });
        store.save(&initial).unwrap();

        store
            .update(|data| {
                let entry = data.api_gateway.accounts.first_mut().unwrap();
                entry.state = ApiPoolAccountState::CoolingDown;
                entry.cooldown_until = Some("2026-08-07T15:00:00Z".to_string());
                entry.error = Some("rate limited".to_string());
            })
            .unwrap();

        // Simulate an app-state writer holding a stale snapshot while the gateway worker updates
        // its runtime state through Store::update.
        let mut stale = initial;
        stale.auto_switch_threshold = 88.0;
        store.save(&stale).unwrap();

        let persisted = store.load().unwrap();
        let entry = persisted.api_gateway.accounts.first().unwrap();
        assert_eq!(persisted.auto_switch_threshold, 88.0);
        assert_eq!(entry.state, ApiPoolAccountState::CoolingDown);
        assert_eq!(
            entry.cooldown_until.as_deref(),
            Some("2026-08-07T15:00:00Z")
        );
        assert_eq!(entry.error.as_deref(), Some("rate limited"));
        let _ = fs::remove_dir_all(store.root_dir());
    }

    /// Two threads saving at once must never publish a spliced file.
    #[test]
    fn concurrent_saves_always_leave_parsable_state() {
        let store = temp_store("concurrent");
        store.save(&StoredState::default()).unwrap();
        let root = store.root_dir().to_path_buf();

        std::thread::scope(|scope| {
            for worker in 0..4 {
                let store = Store::for_test(root.clone()).unwrap();
                scope.spawn(move || {
                    for round in 0..25 {
                        let _ = store.update(|data| {
                            data.auto_switch_threshold = f64::from(worker * 100 + round);
                            data.accounts.clear();
                        });
                        assert!(store.load().is_ok(), "state.json must always parse");
                    }
                });
            }
        });

        assert!(store.load().is_ok());
        let _ = fs::remove_dir_all(&root);
    }
}
