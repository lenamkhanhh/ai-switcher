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
fn set_owner_only_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_directory_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only_directory_permissions(_path: &std::path::Path) -> Result<()> {
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
    2
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
        self.write_state(state)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_store(name: &str) -> Store {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("aisw-store-{name}-{unique}"));
        Store::for_test(root).unwrap()
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
