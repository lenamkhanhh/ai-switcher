use crate::codex_app_server::{
    classify_terminal_turn, recovery_error_message, terminal_error_fingerprint, AppServerConfig,
    CodexAppServerClient,
};
use crate::bulk_login::run_staging_bulk_login;
use crate::codex_import::{codex_auth_json, parse_k12_export, parse_k12_export_batch};
use crate::models::{
    Account, AccountState, AddAccountInput, AddApiAccountInput, ApiGatewayAccount, ApiGatewayCombo,
    ApiGatewayConfig, ApiGatewayKey, ApiGatewayServerState, ApiGatewaySnapshot, ApiProvider,
    ApiUsageReport, AppSnapshot, AutoSwitchSetting, BulkImportCodexAccountsInput,
    BulkLoginCodexInput, CodexLoginMethod, CreateApiGatewayKeyInput, CreateApiGatewayKeyResult,
    CreateVirtualApiAccountInput,
    DeleteApiGatewayComboInput, DeleteApiGatewayKeyInput, DetectionReport, GoalSnapshot,
    ImportCodexAccountInput, QuotaInfo, RecoveryErrorKind, RecoveryPlanStep,
    RecoveryRuntimeSnapshot, RenameAccountInput, ResumeJob, ResumeJobStatus,
    SaveApiGatewayComboInput, SetApiGatewayAccountInput, SetLauncherInput, SetToolSetupInput,
    StartApiGatewayInput, SwitchAccountInput, ToolId, ToolStatus, UsageReport, UsageReports,
    VaultCredential, VaultCredentialInput, VaultCredentialSummary, VaultStatus,
};
use crate::quota::{codex_account_email, read_quota};
use crate::store::{normalize_account_states, Store, StoredState};
use crate::tools::{
    antigravity_capture, antigravity_current_token, antigravity_new_login, antigravity_open_ide,
    antigravity_quit_ide, antigravity_restore, antigravity_saved_profile, antigravity_saved_token,
    clear_active_profile, create_profile_with_default, current_platform, default_config_dir,
    delete_account_files, finalize_staged_account_files, full_launcher_name, install_shell_hook,
    is_codex_imported_session, is_installed, launch_profile_login,
    launcher_name_collides_with_system, launcher_paths, link_shared_config_to,
    link_shared_sessions_to, mark_codex_imported_session, refresh_codex_frontends_after_switch,
    remove_launcher, restart_codex_runtime_after_switch, restore_staged_account_files,
    stage_account_files, sync_codex_ide_credentials, sync_reauthenticated_codex_credentials,
    write_active_profile, write_api_key_file, write_api_launcher, write_claude_proxy_settings,
    write_codex_auth, write_codex_proxy_config, write_launcher,
};
use crate::vault::VaultService;
use anyhow::{Context, Result};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;
use uuid::Uuid;
use zeroize::Zeroize;

pub struct ManagedState {
    pub store: Store,
    pub data: Mutex<StoredState>,
    pub vault: VaultService,
    pub api_server: Mutex<crate::api_gateway::ApiServerHandle>,
    /// Per-rollout byte offsets for the Codex quota-error watcher.
    pub codex_quota_watch: Mutex<BTreeMap<String, u64>>,
    pub codex_active_rollout: Mutex<Option<std::path::PathBuf>>,
    /// True while a prime batch is running, so the per-minute scheduler tick doesn't start a
    /// second overlapping batch (a single attempt can block for minutes on send retries).
    /// True while a Codex handoff is running. The handoff refreshes the account it just switched
    /// to, and that refresh calls auto-switch again — without this one quota event would walk the
    /// whole account list, switching and spawning a resume console for each.
    pub codex_handoff_running: std::sync::atomic::AtomicBool,
}

/// Clears its flag however the guarded scope exits, including on `?`.
struct FlagGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for FlagGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl ManagedState {
    pub fn new() -> Result<Self> {
        let store = Store::new()?;
        let mut data = store.load()?;
        migrate_defaults(&mut data.accounts);
        migrate_codex_imported_login_methods(&store, &mut data.accounts);
        migrate_codex_reauth_states(&mut data.accounts);
        migrate_auto_switch_settings(&mut data);
        migrate_resume_jobs(&mut data);
        retire_auto_ping_settings(&mut data);
        autodetect_missing_tool_setups(&store, &mut data);
        store.save(&data)?;
        let server = crate::api_gateway::ApiServerHandle::stopped(&data.api_gateway);
        let vault = VaultService::new(&store);
        let managed = Self {
            store,
            data: Mutex::new(data),
            vault,
            api_server: Mutex::new(server),
            codex_quota_watch: Mutex::new(BTreeMap::new()),
            codex_active_rollout: Mutex::new(None),
            codex_handoff_running: std::sync::atomic::AtomicBool::new(false),
        };
        // Clean up orphan active files: pointing to a deleted profile → clear + reinstall the hook.
        managed.heal_active_profiles();
        // The VS Code extension reads the machine-default auth file and caches it for the lifetime
        // of its app-server process. Align that file with the selected account on every app start.
        managed.heal_codex_ide_credentials();
        // The API server never auto-starts. Recover from a crash/forced quit that may have left
        // the bare CLI command pointing at a virtual account whose endpoint is now offline.
        let _ = managed.deactivate_virtual_api_accounts();
        Ok(managed)
    }

    fn heal_codex_ide_credentials(&self) {
        let selection = {
            let data = match self.data.lock() {
                Ok(data) => data,
                Err(_) => return,
            };
            let default_dir = resolved_default_config_dir(&data, &ToolId::Codex);
            let active_id = active_account_id_for(&self.store, &ToolId::Codex, &data.accounts);
            active_id.and_then(|id| {
                data.accounts
                    .iter()
                    .find(|account| {
                        account.tool_id == ToolId::Codex
                            && account.id == id
                            && account.api_provider.is_none()
                            && account.state != AccountState::NeedsLogin
                    })
                    .map(|account| (account.id.clone(), account.is_default, default_dir))
            })
        };
        if let Some((account_id, is_default, default_dir)) = selection {
            let _ = sync_codex_ide_credentials(&self.store, &account_id, is_default, &default_dir);
        }
    }

    /// For each CLI tool: if the active file points to a profile that belongs to NO account
    /// in the state (the account was deleted — the dir may be recreated by the CLI itself, so we
    /// can't rely on the dir's existence) → clear it so the plain command falls back to the
    /// machine Default. Also clean up orphan profile dirs on disk.
    fn heal_active_profiles(&self) {
        let data = match self.data.lock() {
            Ok(data) => data,
            Err(_) => return,
        };
        let mut changed = false;
        for tool_id in visible_cli_tools() {
            let valid_dirs: Vec<std::path::PathBuf> = data
                .accounts
                .iter()
                .filter(|a| a.tool_id == tool_id && !a.is_default)
                .map(|a| self.store.account_dir(&tool_id, &a.id))
                .collect();

            // Seed the onboarding flag + link the shared session for every profile (idempotent)
            // — ensures accounts logged in from a previous session also skip the wizard and
            // share the session store with Default.
            for account in data
                .accounts
                .iter()
                .filter(|a| a.tool_id == tool_id && !a.is_default)
            {
                let dir = self.store.account_dir(&tool_id, &account.id);
                crate::tools::seed_onboarding(&tool_id, &dir);
                if let Some(default_dir) = configured_default_config_dir(&data, &tool_id) {
                    link_shared_sessions_to(&tool_id, &dir, &default_dir);
                    if account.api_provider.is_none() {
                        link_shared_config_to(&tool_id, &dir, &default_dir);
                    }
                }
            }

            // Clear active if it points to a profile that belongs to no account.
            let active = self.store.active_profile_path(&tool_id);
            if let Ok(target) = std::fs::read_to_string(&active) {
                let target = target.trim();
                if !target.is_empty() && !valid_dirs.iter().any(|d| d.to_string_lossy() == target) {
                    let _ = clear_active_profile(&tool_id, &self.store);
                    changed = true;
                }
            }

            // Clean up orphan profile dirs (not managed by any account).
            let tool_root = self.store.account_dir(&tool_id, "");
            if let Ok(entries) = std::fs::read_dir(&tool_root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() && !valid_dirs.contains(&path) {
                        let _ = std::fs::remove_dir_all(&path);
                    }
                }
            }
        }
        if changed {
            let _ = install_shell_hook(&self.store);
        }
    }

    pub fn snapshot(&self) -> Result<AppSnapshot> {
        let data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        let snapshot_data = self.store.load().unwrap_or_else(|_| data.clone());
        let status = self
            .api_server
            .lock()
            .map(|server| server.status.clone())
            .unwrap_or_else(|_| {
                crate::api_gateway::ApiServerHandle::stopped(&snapshot_data.api_gateway).status
            });
        Ok(build_snapshot(&self.store, &snapshot_data, status))
    }

    pub fn vault_status(&self) -> Result<VaultStatus> {
        self.vault.status()
    }

    pub fn initialize_vault(&self, passkey: String) -> Result<VaultStatus> {
        let result = self.vault.initialize(&passkey);
        let mut passkey = passkey;
        passkey.zeroize();
        result
    }

    pub fn unlock_vault(&self, passkey: String) -> Result<VaultStatus> {
        let result = self.vault.unlock(&passkey);
        let mut passkey = passkey;
        passkey.zeroize();
        result
    }

    pub fn lock_vault(&self) {
        self.vault.lock();
    }

    pub fn list_vault_credentials(&self) -> Result<Vec<VaultCredentialSummary>> {
        self.vault.list()
    }

    pub fn get_vault_credential(
        &self,
        tool_id: ToolId,
        account_id: String,
    ) -> Result<Option<VaultCredential>> {
        self.validate_vault_account(&tool_id, &account_id)?;
        self.vault.get(&tool_id, &account_id)
    }

    pub fn save_vault_credential(
        &self,
        input: VaultCredentialInput,
    ) -> Result<VaultCredentialSummary> {
        self.validate_vault_account(&input.tool_id, &input.account_id)?;
        self.vault.save(input)
    }

    pub fn delete_vault_credential(&self, tool_id: ToolId, account_id: String) -> Result<()> {
        self.vault.delete(&tool_id, &account_id)
    }

    fn validate_vault_account(&self, tool_id: &ToolId, account_id: &str) -> Result<()> {
        let data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        let account = data
            .accounts
            .iter()
            .find(|account| account.tool_id == *tool_id && account.id == account_id)
            .context("Account not found")?;
        if account.api_provider.is_some() {
            anyhow::bail!("API/proxy credentials are stored with the account profile");
        }
        Ok(())
    }

    pub fn start_api_gateway(&self, input: StartApiGatewayInput) -> Result<AppSnapshot> {
        // NOTE: do NOT refresh the model registry here. Discovery spawns a Codex subprocess and
        // makes blocking HTTP calls per account — doing that inline froze Start (and could hang
        // the whole app). The gateway serves fine without a fresh registry (name heuristics +
        // the cached registry). The UI kicks off a background refresh after Start succeeds.
        let bind_host = input.bind_host.trim();
        if !matches!(bind_host, "127.0.0.1" | "0.0.0.0") {
            anyhow::bail!("API server bind address must be 127.0.0.1 or 0.0.0.0");
        }
        if input.port == 0 {
            anyhow::bail!("API server port must be between 1 and 65535");
        }
        let config = {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.api_gateway.bind_host = bind_host.to_string();
            data.api_gateway.port = input.port;
            data.api_gateway.quota_threshold = input.quota_threshold.clamp(50.0, 100.0);
            data.api_gateway.rotation_strategy = input.rotation_strategy;
            self.store.save(&data)?;
            data.api_gateway.clone()
        };
        let mut server = self
            .api_server
            .lock()
            .map_err(|_| anyhow::anyhow!("API server lock poisoned"))?;
        server.stop(&config);
        let handle = crate::api_gateway::start_server(self.store.clone(), config.clone());
        *server = match handle {
            Ok(handle) => handle,
            Err(err) => crate::api_gateway::ApiServerHandle {
                shutdown: None,
                thread: None,
                status: crate::models::ApiGatewayStatus {
                    state: ApiGatewayServerState::Errored,
                    base_url: crate::api_gateway::base_url(&config),
                    error: Some(err.to_string()),
                },
            },
        };
        let errored = server.status.state == ApiGatewayServerState::Errored;
        drop(server);
        if errored {
            self.deactivate_virtual_api_accounts()?;
        }
        self.snapshot()
    }

    pub fn stop_api_gateway(&self) -> Result<AppSnapshot> {
        let config = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.api_gateway.clone()
        };
        self.api_server
            .lock()
            .map_err(|_| anyhow::anyhow!("API server lock poisoned"))?
            .stop(&config);
        self.deactivate_virtual_api_accounts()?;
        self.snapshot()
    }

    pub fn create_virtual_api_account(
        &self,
        input: CreateVirtualApiAccountInput,
    ) -> Result<AppSnapshot> {
        if !matches!(input.tool_id, ToolId::Claude | ToolId::Codex) {
            anyhow::bail!("Local API accounts are only supported for Claude Code and Codex");
        }
        let running = self
            .api_server
            .lock()
            .map_err(|_| anyhow::anyhow!("API server lock poisoned"))?
            .status
            .state
            == ApiGatewayServerState::Running;
        if !running {
            anyhow::bail!("Start the local API gateway before adding a local API account");
        }
        let name = virtual_api_name(&input.tool_id).to_string();
        let (id, base_url, api_key, model, default_dir, is_new) = {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let existing_id = data
                .accounts
                .iter()
                .find(|account| account.tool_id == input.tool_id && account.name == name)
                .map(|account| account.id.clone());
            let api_key = match data
                .api_gateway
                .keys
                .iter()
                .find_map(|key| key.secret.clone())
            {
                Some(secret) => secret,
                None => {
                    let secret = generate_api_key();
                    data.api_gateway.keys.push(ApiGatewayKey {
                        id: Uuid::new_v4().to_string(),
                        name: "Local CLI".to_string(),
                        prefix: mask_key(&secret),
                        secret: Some(secret.clone()),
                        enabled: true,
                        expires_at: None,
                        created_at: now(),
                    });
                    secret
                }
            };
            // Bind to the requested model if given — a combo name OR any model the gateway can
            // serve directly. Else fall back to the first enabled combo.
            let model = match input
                .model
                .as_deref()
                .map(str::trim)
                .filter(|m| !m.is_empty())
            {
                Some(requested) => {
                    if !crate::api_gateway::model_is_servable(&data, requested) {
                        anyhow::bail!(
                            "'{requested}' isn't a combo or a model your enabled accounts serve"
                        );
                    }
                    requested.to_string()
                }
                None => data
                    .api_gateway
                    .combos
                    .iter()
                    .find(|combo| combo.enabled)
                    .map(|combo| combo.name.clone())
                    .context(
                        "Create at least one combo or pick a model before adding the account",
                    )?,
            };
            let base_url = crate::api_gateway::base_url(&data.api_gateway);
            let default_dir = configured_default_config_dir(&data, &input.tool_id)
                .context("CLI setup is ambiguous — choose the tool's default config first")?;
            self.store.save(&data)?;
            let is_new = existing_id.is_none();
            (
                existing_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
                base_url,
                api_key,
                model,
                default_dir,
                is_new,
            )
        };

        let profile = create_profile_with_default(&input.tool_id, &self.store, &id, &default_dir)?;
        match input.tool_id {
            ToolId::Codex => {
                write_codex_proxy_config(
                    &profile,
                    &name,
                    &format!("{}/v1", base_url.trim_end_matches('/')),
                    &model,
                )?;
                write_api_key_file(&profile, &api_key)?;
            }
            ToolId::Claude => {
                write_claude_proxy_settings(&profile, &base_url, &api_key, &model)?;
                crate::tools::seed_onboarding(&input.tool_id, &profile);
            }
            ToolId::Antigravity => unreachable!("guarded above"),
        }
        // Give the virtual account its own standalone command (`claude-api` / `codex-api`) so it can
        // be run in parallel from any terminal without "Use"-ing it as the active account. Failure
        // to write the launcher is non-fatal — the account still works via "Use".
        let launcher = {
            let binary = {
                let data = self
                    .data
                    .lock()
                    .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
                configured_binary_path(&data, &input.tool_id)
            };
            let full = full_launcher_name(&input.tool_id, &name).ok();
            match (full, binary) {
                (Some(full), Some(binary)) => write_api_launcher(
                    &input.tool_id,
                    &self.store,
                    &id,
                    &full,
                    &model,
                    false,
                    &binary,
                )
                .ok()
                .map(|_| full),
                _ => None,
            }
        };
        let timestamp = now();
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            match input.tool_id {
                ToolId::Claude => data.api_gateway.virtual_claude_enabled = true,
                ToolId::Codex => data.api_gateway.virtual_codex_enabled = true,
                ToolId::Antigravity => {}
            }
            if is_new {
                data.accounts.push(Account {
                    id,
                    tool_id: input.tool_id.clone(),
                    name,
                    state: AccountState::Idle,
                    fingerprint: "api-local".to_string(),
                    created_at: timestamp.clone(),
                    updated_at: timestamp,
                    last_used_at: None,
                    quota: None,
                    launcher_command: launcher,
                    is_default: false,
                    avatar_url: None,
                    account_email: None,
                    api_provider: Some(ApiProvider {
                        base_url,
                        model,
                        bypass: false,
                    }),
                    launcher_paths: None,
                    quota_status: None,
                    codex_login_method: CodexLoginMethod::DeviceCode,
                });
            } else if let Some(account) = data
                .accounts
                .iter_mut()
                .find(|account| account.tool_id == input.tool_id && account.name == name)
            {
                account.state = if account.state == AccountState::Active {
                    AccountState::Active
                } else {
                    AccountState::Idle
                };
                account.fingerprint = "api-local".to_string();
                account.quota = None;
                account.launcher_command = launcher;
                account.updated_at = timestamp;
                account.api_provider = Some(ApiProvider {
                    base_url,
                    model,
                    bypass: false,
                });
            }
            self.store.save(&data)?;
        }
        self.snapshot()
    }

    fn deactivate_virtual_api_accounts(&self) -> Result<()> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        for tool_id in [ToolId::Claude, ToolId::Codex] {
            let active_target = std::fs::read_to_string(self.store.active_profile_path(&tool_id))
                .ok()
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty());
            let active_virtual_id = active_target.as_deref().and_then(|target| {
                data.accounts
                    .iter()
                    .find(|account| {
                        account.tool_id == tool_id
                            && is_virtual_api_account(account)
                            && self
                                .store
                                .account_dir(&tool_id, &account.id)
                                .to_string_lossy()
                                == target
                    })
                    .map(|account| account.id.clone())
            });
            if active_virtual_id.is_some() {
                let _ = clear_active_profile(&tool_id, &self.store);
                let default_id = data
                    .accounts
                    .iter()
                    .find(|account| account.tool_id == tool_id && account.is_default)
                    .map(|account| account.id.clone());
                normalize_account_states(&mut data.accounts, &tool_id, default_id.as_deref());
            }
            for account in data
                .accounts
                .iter_mut()
                .filter(|account| account.tool_id == tool_id && is_virtual_api_account(account))
            {
                if account.state == AccountState::Active {
                    account.state = AccountState::Idle;
                    account.updated_at = now();
                }
            }
        }
        data.api_gateway.virtual_claude_enabled = false;
        data.api_gateway.virtual_codex_enabled = false;
        self.store.save(&data)?;
        let _ = install_shell_hook(&self.store);
        Ok(())
    }

    pub fn create_api_gateway_key(
        &self,
        input: CreateApiGatewayKeyInput,
    ) -> Result<CreateApiGatewayKeyResult> {
        let secret = generate_api_key();
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.api_gateway.keys.push(ApiGatewayKey {
                id: Uuid::new_v4().to_string(),
                name: input.name.trim().chars().take(40).collect(),
                prefix: mask_key(&secret),
                secret: Some(secret.clone()),
                enabled: true,
                expires_at: input.expires_at,
                created_at: now(),
            });
            self.store.save(&data)?;
        }
        Ok(CreateApiGatewayKeyResult {
            snapshot: self.snapshot()?,
            secret,
        })
    }

    pub fn delete_api_gateway_key(&self, input: DeleteApiGatewayKeyInput) -> Result<AppSnapshot> {
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.api_gateway.keys.retain(|key| key.id != input.key_id);
            self.store.save(&data)?;
        }
        self.snapshot()
    }

    /// Return the full secret for a stored key so the UI can copy it on demand. Snapshots redact
    /// secrets, so this is the only way to recover a previously-created key (local single-user app).
    pub fn reveal_api_gateway_key(&self, key_id: String) -> Result<String> {
        let data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        data.api_gateway
            .keys
            .iter()
            .find(|key| key.id == key_id)
            .and_then(|key| key.secret.clone())
            .context("Key not found")
    }

    pub fn save_api_gateway_combo(&self, input: SaveApiGatewayComboInput) -> Result<AppSnapshot> {
        let name = input.name.trim();
        if name.is_empty() {
            anyhow::bail!("Combo name is required");
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            anyhow::bail!("Combo name allows only letters, numbers, '-', '_' and '.'");
        }
        // De-dupe member models, preserving order; drop blanks.
        let mut seen = std::collections::HashSet::new();
        let members: Vec<String> = input
            .members
            .iter()
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty() && seen.insert(model.clone()))
            .collect();
        if members.is_empty() {
            anyhow::bail!("A combo must include at least one model");
        }
        let timestamp = now();
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            if data
                .api_gateway
                .combos
                .iter()
                .any(|combo| combo.name == name && Some(combo.id.as_str()) != input.id.as_deref())
            {
                anyhow::bail!("Combo name must be unique");
            }
            let existing = input
                .id
                .as_deref()
                .and_then(|id| data.api_gateway.combos.iter().find(|combo| combo.id == id));
            let combo = ApiGatewayCombo {
                id: input
                    .id
                    .clone()
                    .unwrap_or_else(|| Uuid::new_v4().to_string()),
                name: name.to_string(),
                members,
                strategy: input.strategy,
                enabled: existing.is_none_or(|combo| combo.enabled),
                created_at: existing.map_or_else(|| timestamp.clone(), |c| c.created_at.clone()),
                updated_at: timestamp,
            };
            match data
                .api_gateway
                .combos
                .iter()
                .position(|existing| existing.id == combo.id)
            {
                Some(index) => data.api_gateway.combos[index] = combo,
                None => data.api_gateway.combos.push(combo),
            }
            self.store.save(&data)?;
        }
        self.snapshot()
    }

    pub fn delete_api_gateway_combo(
        &self,
        input: DeleteApiGatewayComboInput,
    ) -> Result<AppSnapshot> {
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.api_gateway
                .combos
                .retain(|combo| combo.id != input.combo_id);
            self.store.save(&data)?;
        }
        self.snapshot()
    }

    /// Toggle whether a subscription account participates in gateway rotation. Upserts the
    /// participation entry (missing = enabled by default).
    pub fn set_api_gateway_account(&self, input: SetApiGatewayAccountInput) -> Result<AppSnapshot> {
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            match data.api_gateway.accounts.iter_mut().find(|entry| {
                entry.tool_id == input.tool_id && entry.account_id == input.account_id
            }) {
                Some(entry) => entry.enabled = input.enabled,
                None => data.api_gateway.accounts.push(ApiGatewayAccount {
                    tool_id: input.tool_id,
                    account_id: input.account_id,
                    enabled: input.enabled,
                    state: crate::models::ApiPoolAccountState::Available,
                    cooldown_until: None,
                    error: None,
                }),
            }
            self.store.save(&data)?;
        }
        self.snapshot()
    }

    pub fn refresh_api_gateway_models(&self) -> Result<AppSnapshot> {
        let (data, accounts) = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let accounts = data
                .accounts
                .iter()
                .filter(|account| {
                    matches!(account.tool_id, ToolId::Claude | ToolId::Codex)
                        && account.api_provider.is_none()
                        && account.state != AccountState::NeedsLogin
                })
                .cloned()
                .collect::<Vec<_>>();
            (data.clone(), accounts)
        };
        let registry = accounts
            .iter()
            .map(|account| {
                crate::api_gateway::discover_account_models(
                    &self.store,
                    &data,
                    account,
                    configured_binary_path(&data, &account.tool_id).as_deref(),
                )
            })
            .collect();
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.api_gateway.model_registry = registry;
            self.store.save(&data)?;
        }
        self.snapshot()
    }

    pub fn detect_tool_setup(&self, tool_id: ToolId) -> DetectionReport {
        crate::detection::detect_tool_setup(&tool_id, &self.store)
    }

    pub fn validate_tool_setup(&self, input: SetToolSetupInput) -> DetectionReport {
        let mut report = crate::detection::detect_tool_setup(&input.tool_id, &self.store);
        let config = crate::detection::validate_config_dir(
            &input.tool_id,
            &self.store,
            &input.default_config_dir,
        );
        let binary = crate::detection::validate_binary_path(&input.tool_id, &input.binary_path);
        report.config_candidates.insert(0, config);
        report.binary_candidates.insert(0, binary);
        report
    }

    pub fn set_tool_setup(&self, input: SetToolSetupInput) -> Result<AppSnapshot> {
        let (setup, _) = crate::detection::setup_from_manual(
            &input.tool_id,
            &self.store,
            input.binary_path,
            input.default_config_dir,
        );
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.tool_setups
                .insert(input.tool_id.as_str().to_string(), setup);
            self.store.save(&data)?;
        }
        self.snapshot()
    }

    /// Build the token-usage report (Usage tab): incrementally scan every Claude/Codex config
    /// dir on the machine, aggregate per tool, and price it via the LiteLLM cache. Antigravity
    /// is excluded (no token logs). Cheap to call repeatedly thanks to the per-file cursor cache.
    pub fn usage_report(&self, range_days: u32) -> UsageReport {
        crate::usage::build_report(
            &self.store.usage_cache_path(),
            &self.store.price_cache_path(),
            &self.config_dirs(&ToolId::Claude),
            &self.config_dirs(&ToolId::Codex),
            range_days,
        )
    }

    pub fn usage_reports(&self, range_days: u32) -> UsageReports {
        crate::usage::build_reports(
            &self.store.usage_cache_path(),
            &self.store.price_cache_path(),
            &self.config_dirs(&ToolId::Claude),
            &self.config_dirs(&ToolId::Codex),
            range_days,
        )
    }

    pub fn api_usage_report(&self) -> ApiUsageReport {
        crate::api_gateway::usage_report(&self.store)
    }

    /// Every config dir to scan for a tool: the machine default (`~/.claude`, `~/.codex`) plus
    /// every per-account profile dir under the app's accounts root.
    fn config_dirs(&self, tool_id: &ToolId) -> Vec<std::path::PathBuf> {
        let default_dir = self
            .data
            .lock()
            .ok()
            .map(|data| resolved_default_config_dir(&data, tool_id))
            .unwrap_or_else(|| default_config_dir(tool_id));
        let mut dirs = vec![default_dir];
        if let Ok(entries) = std::fs::read_dir(self.store.tool_accounts_root(tool_id)) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                }
            }
        }
        dirs
    }

    /// Scan EVERY account in `NeedsLogin`: any that already has a token (the user finished
    /// logging in while the app was closed) → move to Idle + read quota. Called on every
    /// snapshot load so the app is correct as soon as it opens, without needing to press Refresh.
    pub fn recheck_pending_logins(&self) -> Result<()> {
        let pending: Vec<(ToolId, String)> = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.accounts
                .iter()
                .filter(|a| a.state == AccountState::NeedsLogin && !a.is_default)
                .map(|a| (a.tool_id.clone(), a.id.clone()))
                .collect()
        };
        for (tool_id, account_id) in pending {
            let _ = self.confirm_login(&tool_id, &account_id);
        }
        Ok(())
    }

    pub fn refresh_tool(&self, tool_id: ToolId, app: Option<&AppHandle>) -> Result<AppSnapshot> {
        // Phase 1: collect (account_id, config_dir) — brief lock, no HTTP.
        let accounts_info: Vec<(String, std::path::PathBuf)> = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let default_config_dir = resolved_default_config_dir(&data, &tool_id);
            data.accounts
                .iter()
                .filter(|a| {
                    a.tool_id == tool_id
                        && a.state != AccountState::NeedsLogin
                        && a.api_provider.is_none()
                })
                .map(|a| {
                    (
                        a.id.clone(),
                        account_config_dir_with_default(&self.store, a, &default_config_dir),
                    )
                })
                .collect()
        };
        // Mutex released — HTTP calls run in parallel without blocking other operations.

        // Phase 2: fetch quotas without flooding the provider. A full refresh used to launch one
        // HTTP request per account at once, which made otherwise healthy accounts intermittently
        // hit the 20-second network timeout. Keep a small amount of parallelism and retry only
        // transient transport failures; authentication and quota HTTP responses are never retried.
        let mut results: Vec<(String, QuotaInfo)> = Vec::with_capacity(accounts_info.len());
        for range in quota_refresh_batch_ranges(accounts_info.len()) {
            let handles: Vec<_> = accounts_info[range]
                .iter()
                .cloned()
                .map(|(account_id, config_dir)| {
                    let tid = tool_id.clone();
                    std::thread::spawn(move || {
                        let quota = read_quota_resilient(&tid, &config_dir);
                        (account_id, quota)
                    })
                })
                .collect();
            results.extend(handles.into_iter().filter_map(|handle| handle.join().ok()));
        }

        // Phase 3: write all quotas back — brief lock.
        let exhausted_accounts: Vec<Account> = {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let timestamp = now();
            let mut exhausted = Vec::new();
            for (account_id, quota) in results {
                let requires_reauth = quota_requires_reauth(&tool_id, &quota);
                if let Some(account) = data
                    .accounts
                    .iter_mut()
                    .find(|a| a.tool_id == tool_id && a.id == account_id)
                {
                    let was_exhausted = account.state == AccountState::Exhausted;
                    let quota = keep_last_quota_on_transient_failure(account.quota.as_ref(), quota);
                    // A quota that still carries an error is a cycle we couldn't read cleanly
                    // (transient failure showing the last-known numbers). We have no fresh truth,
                    // so KEEP the current state — inferring recovery from it would flip an
                    // exhausted account to Idle, suppress auto-switch, and refire the "out of
                    // quota" notification on the next successful poll.
                    let unread_this_cycle = quota.error.is_some();
                    account.quota = Some(quota);
                    account.updated_at = timestamp.clone();
                    account.state = state_after_refresh(
                        account.state.clone(),
                        account.is_default,
                        requires_reauth,
                        unread_this_cycle,
                        is_exhausted(account),
                        was_exhausted,
                    );
                    // Notify only on the TRANSITION into exhausted, not on every refresh while it
                    // stays exhausted — otherwise the 5-minute poller fires the same notification
                    // over and over until the window resets.
                    if account.state == AccountState::Exhausted && !was_exhausted {
                        exhausted.push(account.clone());
                    }
                }
            }
            self.store.save(&data)?;
            exhausted
        };

        if let Some(app) = app {
            for account in &exhausted_accounts {
                notify_exhausted(app, account);
            }
        }

        let setting = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            auto_switch_setting(&data, &tool_id)
        };
        if setting.enabled {
            self.maybe_auto_switch(&tool_id, setting.threshold, app)?;
        }
        self.snapshot()
    }

    /// Refresh quota for a single account. Releases the mutex during the HTTP call so other
    /// operations (switch, add) are not blocked while waiting for the network.
    pub fn refresh_single_account(
        &self,
        tool_id: &ToolId,
        account_id: &str,
        app: Option<&AppHandle>,
    ) -> Result<AppSnapshot> {
        // Phase 1: get config_dir — brief lock.
        let config_dir: Option<std::path::PathBuf> = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let default_dir = resolved_default_config_dir(&data, tool_id);
            data.accounts
                .iter()
                .find(|a| {
                    a.tool_id == *tool_id
                        && a.id == account_id
                        && a.state != AccountState::NeedsLogin
                        && a.api_provider.is_none()
                })
                .map(|a| account_config_dir_with_default(&self.store, a, &default_dir))
        };
        let Some(config_dir) = config_dir else {
            return self.snapshot();
        };

        // Phase 2: HTTP call — no mutex held.
        let quota = read_quota_resilient(tool_id, &config_dir);
        let requires_reauth = quota_requires_reauth(tool_id, &quota);

        // Phase 3: write back — brief lock.
        let exhausted: Option<Account> = {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let mut result = None;
            if let Some(account) = data
                .accounts
                .iter_mut()
                .find(|a| a.tool_id == *tool_id && a.id == account_id)
            {
                let was_exhausted = account.state == AccountState::Exhausted;
                let quota = keep_last_quota_on_transient_failure(account.quota.as_ref(), quota);
                let unread_this_cycle = quota.error.is_some();
                account.quota = Some(quota);
                account.updated_at = now();
                account.state = state_after_refresh(
                    account.state.clone(),
                    account.is_default,
                    requires_reauth,
                    unread_this_cycle,
                    is_exhausted(account),
                    was_exhausted,
                );
                // Notify only on the transition INTO exhausted (see refresh_tool) — not on every
                // single-account refresh while it stays exhausted.
                if account.state == AccountState::Exhausted && !was_exhausted {
                    result = Some(account.clone());
                }
            }
            self.store.save(&data)?;
            result
        };

        if let (Some(app), Some(account)) = (app, exhausted) {
            notify_exhausted(app, &account);
        }

        let setting = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            auto_switch_setting(&data, tool_id)
        };
        if setting.enabled {
            self.maybe_auto_switch(tool_id, setting.threshold, app)?;
        }
        self.snapshot()
    }

    /// Watch Codex rollout logs for quota/rate-limit failures and immediately run the same
    /// auto-switch path as a manual "Use" click. This makes an in-progress VS Code/Codex session
    /// reconnect much closer to the moment it gets blocked, instead of waiting for the 5-minute
    /// quota poll.
    pub fn check_codex_quota_error_and_auto_switch(&self, app: Option<&AppHandle>) -> Result<()> {
        let (enabled, threshold, sessions_dir) = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let setting = auto_switch_setting(&data, &ToolId::Codex);
            let default_dir = resolved_default_config_dir(&data, &ToolId::Codex);
            (
                setting.enabled,
                setting.threshold,
                default_dir.join("sessions"),
            )
        };
        if !enabled {
            return Ok(());
        }

        // Shared session history can contain several active rollouts. Inspect every file with a
        // newly appended tail and bind the handoff to the exact file that emitted the quota line;
        // never overwrite it with whichever rollout happens to have the newest mtime.
        for file in rollout_files(&sessions_dir) {
            let key = file.to_string_lossy().to_string();
            let len = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            let start = {
                let mut guard = self
                    .codex_quota_watch
                    .lock()
                    .map_err(|_| anyhow::anyhow!("watch lock poisoned"))?;
                let entry = guard.entry(key.clone()).or_insert(len);
                if *entry > len {
                    *entry = 0;
                }
                *entry
            };
            if start >= len {
                continue;
            }
            // Read first, then advance the cursor by what was actually consumed. Committing `len`
            // before the read dropped every byte the read couldn't return — a tail ending mid-line
            // or mid-character was never looked at again, taking any quota error in it with it.
            let (text, consumed) = match read_file_tail_from(&file, start) {
                Ok(tail) => tail,
                // One unreadable rollout (locked, rotated) must not abandon the others this pass.
                Err(_) => continue,
            };
            if consumed > 0 {
                if let Ok(mut guard) = self.codex_quota_watch.lock() {
                    guard.insert(key, start + consumed);
                }
            }
            if text.lines().any(codex_quota_error_line) {
                if let Ok(mut active) = self.codex_active_rollout.lock() {
                    *active = Some(file.clone());
                }
                // Refresh first so the UI/quota model catches up, then force the manual-switch
                // runtime refresh path. The explicit rollout binding above survives refreshes.
                let _ = self.refresh_tool(ToolId::Codex, app);
                self.try_seamless_codex_handoff(
                    threshold.min(crate::seamless::DEFAULT_HANDOFF_THRESHOLD),
                    true,
                    app,
                )?;
                break;
            }
        }
        Ok(())
    }

    /// Legacy command: apply the same auto-switch setting to Claude and Codex.
    pub fn set_auto_switch(&self, enabled: bool, threshold: f64) -> Result<AppSnapshot> {
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.auto_switch = enabled;
            data.auto_switch_threshold = threshold.clamp(50.0, 100.0);
            let threshold = data.auto_switch_threshold;
            for tool_id in [ToolId::Claude, ToolId::Codex] {
                data.auto_switch_settings.insert(
                    tool_id.as_str().to_string(),
                    AutoSwitchSetting { enabled, threshold },
                );
            }
            self.store.save(&data)?;
        }
        self.snapshot()
    }

    /// Enable/disable auto-switch + set the threshold for one CLI tool.
    pub fn set_auto_switch_setting(
        &self,
        tool_id: ToolId,
        enabled: bool,
        threshold: f64,
    ) -> Result<AppSnapshot> {
        if matches!(tool_id, ToolId::Antigravity) {
            anyhow::bail!("Auto-switch is only supported for Claude Code and Codex");
        }
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.auto_switch_settings.insert(
                tool_id.as_str().to_string(),
                AutoSwitchSetting {
                    enabled,
                    threshold: threshold.clamp(50.0, 100.0),
                },
            );
            let claude = auto_switch_setting(&data, &ToolId::Claude);
            let codex = auto_switch_setting(&data, &ToolId::Codex);
            data.auto_switch = claude.enabled || codex.enabled;
            data.auto_switch_threshold = if claude.enabled {
                claude.threshold
            } else {
                codex.threshold
            };
            self.store.save(&data)?;
        }
        self.snapshot()
    }

    pub fn set_launch_with_windows(&self, enabled: bool) -> Result<AppSnapshot> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        data.launch_with_windows = enabled;
        self.store.save(&data)?;
        drop(data);
        self.snapshot()
    }

    /// If the tool's currently-used account (plain command) has hit the threshold → automatically
    /// switch to the healthiest account (same tool, not yet at the threshold). Claude/Codex only.
    /// Applied via the same switch mechanism (hook + active file), WITHOUT touching custom launchers.
    fn maybe_auto_switch(
        &self,
        tool_id: &ToolId,
        threshold: f64,
        app: Option<&AppHandle>,
    ) -> Result<()> {
        if tool_id == &ToolId::Codex
            && self.try_seamless_codex_handoff(
                threshold.min(crate::seamless::DEFAULT_HANDOFF_THRESHOLD),
                false,
                app,
            )?
        {
            return Ok(());
        }
        if !matches!(tool_id, ToolId::Claude | ToolId::Codex) {
            return Ok(());
        }
        let target = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let active_id = active_account_id_for(&self.store, tool_id, &data.accounts);
            // No active account (using Default) or the active one still has quota → no switch needed.
            auto_switch_target(&data.accounts, tool_id, threshold, active_id.as_deref()).map(
                |account| {
                    let previous_name = active_id
                        .as_deref()
                        .and_then(|id| {
                            data.accounts
                                .iter()
                                .find(|item| item.tool_id == *tool_id && item.id == id)
                        })
                        .map(|item| item.name.clone())
                        .unwrap_or_else(|| "previous account".to_string());
                    (
                        account.id.clone(),
                        account.is_default,
                        account.api_provider.is_some(),
                        previous_name,
                    )
                },
            )
        };

        let Some((target_id, is_default, is_api, previous_name)) = target else {
            return Ok(());
        };

        self.refresh_runtime_for_account_switch(tool_id, &target_id, is_default, is_api)
            .context("Auto-switch failed while refreshing the selected runtime")?;
        if is_default {
            clear_active_profile(tool_id, &self.store)
                .context("Auto-switch failed while restoring the default profile")?;
        } else {
            write_active_profile(tool_id, &self.store, &target_id)
                .context("Auto-switch failed while writing the active profile")?;
        }
        install_shell_hook(&self.store).context("Auto-switch failed while installing the hook")?;

        let switched_name = {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let timestamp = now();
            normalize_account_states(&mut data.accounts, tool_id, Some(&target_id));
            let mut name = String::new();
            for account in data.accounts.iter_mut().filter(|a| a.tool_id == *tool_id) {
                if account.id == target_id {
                    account.state = AccountState::Active;
                    account.last_used_at = Some(timestamp.clone());
                    account.updated_at = timestamp.clone();
                    name = account.name.clone();
                }
            }
            self.store.save(&data)?;
            name
        };

        if matches!(tool_id, ToolId::Codex) && !is_api {
            refresh_codex_frontends_after_switch();
        }

        if let Some(app) = app {
            let message = format!(
                "{} reached the quota cutoff and switched to {}. Codex and VS Code will reconnect with the new account.",
                tool_id.display_name(),
                switched_name
            );
            if matches!(tool_id, ToolId::Codex) && !is_api {
                let default_dir = {
                    let data = self
                        .data
                        .lock()
                        .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
                    resolved_default_config_dir(&data, &ToolId::Codex)
                };
                if let Ok(Some(handoff)) = crate::handoff::create_latest_codex_handoff(
                    &default_dir,
                    &self.store.handoff_dir(),
                    &previous_name,
                    &switched_name,
                ) {
                    let _ = app.emit("auto-switch-handoff", &handoff);
                    let _ = app
                        .notification()
                        .builder()
                        .title("Codex handoff ready")
                        .body("Open a new Codex task and paste the continuation prompt.")
                        .show();
                }
            }
            let _ = app
                .notification()
                .builder()
                .title("Auto-switched account")
                .body(&message)
                .show();
            // In-app banner (more reliable than the system notification if the user disabled the permission).
            let _ = app.emit("auto-switched", message);
            if let Ok(snapshot) = self.snapshot() {
                let _ = app.emit("snapshot-changed", snapshot);
            }
        }
        Ok(())
    }

    fn try_seamless_codex_handoff(
        &self,
        threshold: f64,
        forced_quota_error: bool,
        app: Option<&AppHandle>,
    ) -> Result<bool> {
        // Re-entered from a quota refresh or the ten-second watcher while the same transaction is
        // active. A recovery is single-flight: another observer must read the durable job rather
        // than start a second credential switch or turn.
        if self
            .codex_handoff_running
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(true);
        }
        let _handoff_guard = FlagGuard(&self.codex_handoff_running);

        let (current, used, explicit_quota_error, binary, default_dir) = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let Some(active_id) =
                active_account_id_for(&self.store, &ToolId::Codex, &data.accounts)
            else {
                return Ok(false);
            };
            let Some(account) = data
                .accounts
                .iter()
                .find(|a| a.tool_id == ToolId::Codex && a.id == active_id)
            else {
                return Ok(false);
            };
            let used = account
                .quota
                .as_ref()
                .and_then(|q| q.five_hour.percent_used);
            let explicit_quota_error = forced_quota_error
                || account
                    .quota
                    .as_ref()
                    .and_then(|q| q.error.as_ref())
                    .is_some();
            if !crate::seamless::should_start_handoff(used, explicit_quota_error, threshold) {
                return Ok(false);
            }
            (
                account.clone(),
                used,
                explicit_quota_error,
                configured_binary_path(&data, &ToolId::Codex),
                resolved_default_config_dir(&data, &ToolId::Codex),
            )
        };

        // A quota poll can run without a task. That is still an ordinary account-rotation case;
        // the transactional path is entered only when the watcher has bound an exact rollout.
        let rollout = self
            .codex_active_rollout
            .lock()
            .ok()
            .and_then(|path| path.clone());
        let Some(rollout) = rollout else {
            return Ok(false);
        };
        let Some(binary) = binary else {
            self.emit_recovery_message(
                app,
                "Resume could not be verified: Codex CLI is not configured.",
            );
            return Ok(true);
        };

        let workspace_cwd =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let server_cwd = workspace_cwd.clone();
        let config = AppServerConfig::new(binary.clone(), default_dir.clone(), server_cwd);
        let mut client = match CodexAppServerClient::connect(&config) {
            Ok(client) => client,
            Err(error) => {
                self.emit_recovery_message(
                    app,
                    &format!(
                        "Resume could not be verified: {}.",
                        crate::codex_app_server::redact_text(&error.to_string())
                    ),
                );
                return Ok(true);
            }
        };

        // The App Server is authoritative for thread identity and turn state. A rollout filename
        // is only an observation/binding for the watcher, never a reason to switch on its text.
        let Some(thread_hint) = crate::handoff::thread_id_from_rollout(&rollout)
            .ok()
            .flatten()
        else {
            self.emit_recovery_message(
                app,
                "Resume could not be verified: the rollout did not contain an exact thread id.",
            );
            return Ok(true);
        };
        let snapshot = match client.read_thread(&thread_hint) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.emit_recovery_message(
                    app,
                    &format!(
                        "Resume could not be verified: {}.",
                        crate::codex_app_server::redact_text(&error.to_string())
                    ),
                );
                return Ok(true);
            }
        };
        let thread_id = snapshot.thread_id.clone();
        let cwd = if snapshot.runtime.cwd.is_empty() {
            workspace_cwd.to_string_lossy().to_string()
        } else {
            snapshot.runtime.cwd.clone()
        };
        if snapshot.active_turn().is_some() {
            let job_id =
                self.persist_waiting_resume_job(&thread_id, &current.id, &cwd, &rollout)?;
            self.update_resume_job(&job_id, |job| {
                job.status = ResumeJobStatus::AwaitingTerminal;
                job.error = None;
            })?;
            self.emit_recovery_message(
                app,
                "Waiting for the current Codex turn to finish before switching accounts.",
            );
            return Ok(true);
        }

        let terminal = snapshot.turns.iter().rev().find(|turn| turn.is_terminal());
        let failed_turn_id = terminal
            .map(|turn| turn.id.clone())
            .unwrap_or_else(|| format!("planned-{thread_id}"));
        let (failure_kind, failure_message) = if forced_quota_error {
            if let Some(turn) = terminal.filter(|turn| {
                matches!(turn.status.as_str(), "failed" | "interrupted" | "cancelled")
            }) {
                (classify_terminal_turn(turn), recovery_error_message(turn))
            } else {
                (RecoveryErrorKind::Unknown, None)
            }
        } else if let Some(turn) = terminal
            .filter(|turn| matches!(turn.status.as_str(), "failed" | "interrupted" | "cancelled"))
        {
            (classify_terminal_turn(turn), recovery_error_message(turn))
        } else {
            // Planned handoff: the threshold was reached, the current turn is already terminal,
            // and the next turn can be accepted on a preflighted account without replaying work.
            (RecoveryErrorKind::UsageLimitExceeded, None)
        };
        let fingerprint = terminal_error_fingerprint(
            failure_kind,
            failure_message.as_deref().or(Some("planned handoff")),
        );
        let goal = client
            .read_goal(&thread_id)
            .context("Could not read the Codex Goal")?;
        let goal_for_manifest = goal.clone().unwrap_or_default();
        let runtime = snapshot.runtime.clone();
        let rollout_offset = std::fs::metadata(&rollout)
            .map(|meta| meta.len())
            .unwrap_or(0);

        let (job_id, recovery_id, attempt) = {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let duplicate = crate::seamless::is_recovery_duplicate(
                &data.resume_jobs,
                &thread_id,
                &failed_turn_id,
                &fingerprint,
            );
            let existing_index = data.resume_jobs.iter().position(|job| {
                (duplicate
                    && job.thread_id == thread_id
                    && job.failed_turn_id == failed_turn_id
                    && job.terminal_error_fingerprint.as_deref() == Some(fingerprint.as_str()))
                    || (job.thread_id == thread_id
                        && job.status == ResumeJobStatus::AwaitingTerminal)
            });
            if let Some(index) = existing_index {
                let existing = &data.resume_jobs[index];
                if existing.status.is_acknowledged()
                    || matches!(
                        existing.status,
                        ResumeJobStatus::NeedsAttention
                            | ResumeJobStatus::Failed
                            | ResumeJobStatus::Cancelled
                    )
                {
                    return Ok(true);
                }
                let id = existing.id.clone();
                let recovery_id = if existing.recovery_id.is_empty() {
                    Uuid::new_v4().to_string()
                } else {
                    existing.recovery_id.clone()
                };
                let attempt = existing.attempt.max(1);
                let timestamp = now();
                let job = &mut data.resume_jobs[index];
                job.recovery_id = recovery_id.clone();
                job.thread_id = thread_id.clone();
                job.session_id = thread_id.clone();
                job.failed_turn_id = failed_turn_id.clone();
                job.cwd = cwd.clone();
                job.rollout_path = rollout.to_string_lossy().to_string();
                job.rollout_offset = rollout_offset;
                job.terminal_error_kind = Some(failure_kind);
                job.terminal_error_fingerprint = Some(fingerprint.clone());
                job.model = runtime.model.clone();
                job.reasoning_effort = runtime.reasoning_effort.clone();
                job.personality = runtime.personality.clone();
                job.approval_policy = runtime.approval_policy.clone();
                job.sandbox_policy = runtime.sandbox_policy.clone();
                job.goal_snapshot = goal.clone();
                job.status = ResumeJobStatus::Checkpointing;
                job.updated_at = timestamp;
                job.error = None;
                self.store.save(&data)?;
                (id, recovery_id, attempt)
            } else {
                let timestamp = now();
                let id = Uuid::new_v4().to_string();
                let recovery_id = Uuid::new_v4().to_string();
                let job = ResumeJob {
                    id: id.clone(),
                    schema_version: 3,
                    recovery_id: recovery_id.clone(),
                    session_id: thread_id.clone(),
                    thread_id: thread_id.clone(),
                    failed_turn_id: failed_turn_id.clone(),
                    resumed_turn_id: None,
                    event_key: fingerprint.clone(),
                    cwd: cwd.clone(),
                    from_account_id: current.id.clone(),
                    to_account_id: None,
                    checkpoint_path: String::new(),
                    rollout_path: rollout.to_string_lossy().to_string(),
                    rollout_offset,
                    terminal_error_kind: Some(failure_kind),
                    terminal_error_fingerprint: Some(fingerprint.clone()),
                    manifest_path: None,
                    recovery_marker: Some(crate::handoff::marker(&recovery_id, 1)),
                    model: runtime.model.clone(),
                    reasoning_effort: runtime.reasoning_effort.clone(),
                    personality: runtime.personality.clone(),
                    approval_policy: runtime.approval_policy.clone(),
                    sandbox_policy: runtime.sandbox_policy.clone(),
                    goal_snapshot: goal.clone(),
                    attempt: 1,
                    status: ResumeJobStatus::Checkpointing,
                    created_at: timestamp.clone(),
                    updated_at: timestamp,
                    last_ack_at: None,
                    error: None,
                };
                data.resume_jobs.push(job);
                self.store.save(&data)?;
                (id, recovery_id, 1)
            }
        };

        let mut plan = crate::handoff::plan_from_thread(&snapshot);
        if plan.is_empty() {
            plan.push(RecoveryPlanStep {
                id: "continue-first-incomplete-step".into(),
                description: "Continue the first incomplete Goal plan step".into(),
                status: "incomplete".into(),
            });
        }
        let manifest = crate::handoff::manifest_from_thread(
            &recovery_id,
            &failed_turn_id,
            failure_kind,
            failure_message.as_deref(),
            &snapshot,
            goal_for_manifest,
            plan,
            crate::handoff::workspace_snapshot(std::path::Path::new(&cwd)),
            vec![],
        );
        let manifest_path =
            crate::handoff::write_manifest_atomic(&self.store.handoff_dir(), &manifest)?;
        self.update_resume_job(&job_id, |job| {
            job.checkpoint_path = manifest_path.to_string_lossy().to_string();
            job.manifest_path = Some(manifest_path.to_string_lossy().to_string());
            job.recovery_marker = Some(crate::handoff::marker(&recovery_id, attempt));
            job.status = ResumeJobStatus::Checkpointed;
        })?;

        if failure_kind != RecoveryErrorKind::UsageLimitExceeded {
            let message = recovery_attention_message(failure_kind);
            if is_same_account_retryable(failure_kind) {
                let next_attempt = self.schedule_capacity_retry(&job_id, &message)?;
                if let Some(next_attempt) = next_attempt {
                    self.emit_recovery_message(app, &message);
                    drop(client);
                    std::thread::sleep(recovery_retry_delay(next_attempt));
                    if let Err(error) = self.retry_same_account(
                        &job_id,
                        &thread_id,
                        &runtime,
                        goal.as_ref(),
                        &manifest_path,
                        &recovery_id,
                        next_attempt,
                        &current.name,
                        &binary,
                        &default_dir,
                        &cwd,
                        app,
                    ) {
                        self.mark_resume_attention(
                            &job_id,
                            format!(
                                "Resume could not be verified: {}.",
                                crate::codex_app_server::redact_text(&error.to_string())
                            ),
                            app,
                        )?;
                    }
                    return Ok(true);
                }
            }
            self.mark_resume_attention(&job_id, message.clone(), app)?;
            return Ok(true);
        }

        drop(client);
        let candidates = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            crate::seamless::rank_candidates(
                &data.accounts,
                &current.id,
                if explicit_quota_error { None } else { used },
            )
            .into_iter()
            .cloned()
            .collect::<Vec<_>>()
        };
        let retry_target = self.data.lock().ok().and_then(|data| {
            data.resume_jobs
                .iter()
                .find(|job| job.id == job_id)
                .and_then(|job| job.to_account_id.clone())
        });
        let mut selected = retry_target
            .as_deref()
            .filter(|account_id| *account_id == current.id)
            .map(|_| current.clone());
        let mut preflight_reason = None;
        if selected.is_none() {
            for candidate in candidates {
                let candidate_home =
                    account_config_dir_with_default(&self.store, &candidate, &default_dir);
                let candidate_config = AppServerConfig::new(
                    binary.clone(),
                    candidate_home,
                    std::path::PathBuf::from(&cwd),
                );
                let mut candidate_client = match CodexAppServerClient::connect(&candidate_config) {
                    Ok(client) => client,
                    Err(error) => {
                        preflight_reason =
                            Some(crate::codex_app_server::redact_text(&error.to_string()));
                        continue;
                    }
                };
                match candidate_client.preflight_candidate(
                    runtime.model.as_deref(),
                    runtime.reasoning_effort.as_deref(),
                    used,
                ) {
                    Ok(result) if result.eligible => {
                        selected = Some(candidate);
                        break;
                    }
                    Ok(result) => preflight_reason = result.reason,
                    Err(error) => {
                        preflight_reason =
                            Some(crate::codex_app_server::redact_text(&error.to_string()))
                    }
                }
            }
        }
        let Some(candidate) = selected else {
            self.mark_resume_attention(
                &job_id,
                "No eligible Codex account has enough quota to continue this task.".into(),
                app,
            )?;
            let _ = preflight_reason;
            return Ok(true);
        };
        self.update_resume_job(&job_id, |job| {
            job.to_account_id = Some(candidate.id.clone());
            job.status = ResumeJobStatus::CandidateVerified;
        })?;
        self.emit_recovery_message(
            app,
            &format!(
                "Codex quota reached. Recovering this task with {}…",
                candidate.name
            ),
        );

        self.update_resume_job(&job_id, |job| job.status = ResumeJobStatus::Switching)?;
        if let Err(error) = self.commit_recovery_account_switch(&candidate) {
            self.mark_resume_attention(
                &job_id,
                format!(
                    "Resume could not be verified: {}.",
                    crate::codex_app_server::redact_text(&error.to_string())
                ),
                app,
            )?;
            return Ok(true);
        }

        self.update_resume_job(&job_id, |job| job.status = ResumeJobStatus::ThreadResuming)?;
        let recovery_config =
            AppServerConfig::new(binary, default_dir, std::path::PathBuf::from(&cwd));
        let mut recovery_client = match CodexAppServerClient::connect(&recovery_config) {
            Ok(client) => client,
            Err(error) => {
                self.mark_resume_attention(
                    &job_id,
                    format!(
                        "Resume could not be verified: {}.",
                        crate::codex_app_server::redact_text(&error.to_string())
                    ),
                    app,
                )?;
                return Ok(true);
            }
        };
        let resumed = match recovery_client.resume_thread(&thread_id, &runtime) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.mark_resume_attention(
                    &job_id,
                    format!(
                        "Resume could not be verified: {}.",
                        crate::codex_app_server::redact_text(&error.to_string())
                    ),
                    app,
                )?;
                return Ok(true);
            }
        };
        self.update_resume_job(&job_id, |job| job.status = ResumeJobStatus::ThreadResumed)?;

        let mut verified_goal = goal.clone();
        if let Some(expected_goal) = &goal {
            let actual_goal = recovery_client
                .read_goal(&thread_id)
                .context("Could not read the resumed Codex Goal")?;
            let Some(mut actual_goal) = actual_goal else {
                self.mark_resume_attention(
                    &job_id,
                    "Resume could not be verified: the Codex Goal was not returned.".into(),
                    app,
                )?;
                return Ok(true);
            };
            if actual_goal.objective != expected_goal.objective
                || actual_goal.token_budget != expected_goal.token_budget
            {
                self.mark_resume_attention(
                    &job_id,
                    "Resume could not be verified: the resumed Goal did not match the original objective.".into(),
                    app,
                )?;
                return Ok(true);
            }
            if actual_goal.status == "usageLimited" {
                actual_goal.status = "active".into();
                recovery_client.set_goal(&thread_id, &actual_goal)?;
            }
            let final_goal = recovery_client
                .read_goal(&thread_id)
                .context("Could not verify the resumed Codex Goal")?
                .context("Codex Goal disappeared during recovery")?;
            if final_goal.objective != expected_goal.objective
                || final_goal.token_budget != expected_goal.token_budget
                || (expected_goal.status != "usageLimited"
                    && final_goal.status != expected_goal.status)
            {
                self.mark_resume_attention(
                    &job_id,
                    "Resume could not be verified: the resumed Goal did not match the original state.".into(),
                    app,
                )?;
                return Ok(true);
            }
            verified_goal = Some(final_goal);
        }
        self.update_resume_job(&job_id, |job| {
            job.goal_snapshot = verified_goal.clone();
            job.status = ResumeJobStatus::GoalVerified;
        })?;

        // Reconcile once more after resume. A previous crash may already have accepted the exact
        // marker; in that case reading it is the idempotent success path and no new turn is sent.
        let after_resume = recovery_client.read_thread(&thread_id)?;
        let marker = crate::handoff::marker(&recovery_id, attempt);
        if after_resume.contains_marker(&marker) {
            let ack_at = now();
            self.update_resume_job(&job_id, |job| {
                job.status = ResumeJobStatus::Recovered;
                job.last_ack_at = Some(ack_at.clone());
                job.error = None;
            })?;
            self.update_resume_job(&job_id, |job| job.status = ResumeJobStatus::Monitoring)?;
            self.emit_recovery_message(app, &format!("Task resumed with {}.", candidate.name));
            if let Some(app) = app {
                let _ = crate::tools::open_codex_thread_in_app(&thread_id);
                let _ = app.emit("snapshot-changed", self.snapshot()?);
            }
            return Ok(true);
        }

        self.update_resume_job(&job_id, |job| {
            job.status = ResumeJobStatus::TurnStarting;
            job.recovery_marker = Some(marker.clone());
        })?;
        let prompt = crate::handoff::recovery_prompt(&recovery_id, attempt, &manifest_path);
        let client_message_id = format!("{recovery_id}:attempt-{attempt}");
        let ack = match recovery_client.start_turn_and_wait_ack(
            &thread_id,
            &prompt,
            &runtime,
            &client_message_id,
        ) {
            Ok(ack) => ack,
            Err(error) => {
                self.mark_resume_attention(
                    &job_id,
                    format!(
                        "Resume could not be verified: {}.",
                        crate::codex_app_server::redact_text(&error.to_string())
                    ),
                    app,
                )?;
                return Ok(true);
            }
        };
        let ack_at = now();
        self.update_resume_job(&job_id, |job| {
            job.resumed_turn_id = Some(ack.turn_id.clone());
            job.last_ack_at = Some(ack_at.clone());
            job.status = ResumeJobStatus::TurnStarted;
            job.error = None;
        })?;
        // The user-facing success claim is intentionally after the exact ACK and durable write.
        self.update_resume_job(&job_id, |job| job.status = ResumeJobStatus::Recovered)?;
        self.update_resume_job(&job_id, |job| job.status = ResumeJobStatus::Monitoring)?;
        self.emit_recovery_message(app, &format!("Task resumed with {}.", candidate.name));
        if let Some(app) = app {
            let _ = crate::tools::open_codex_thread_in_app(&ack.thread_id);
            let _ = app.emit("snapshot-changed", self.snapshot()?);
        }
        let _ = resumed;
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    fn retry_same_account(
        &self,
        job_id: &str,
        thread_id: &str,
        runtime: &RecoveryRuntimeSnapshot,
        goal: Option<&GoalSnapshot>,
        manifest_path: &std::path::Path,
        recovery_id: &str,
        attempt: u32,
        account_name: &str,
        binary: &std::path::Path,
        default_dir: &std::path::Path,
        cwd: &str,
        app: Option<&AppHandle>,
    ) -> Result<()> {
        self.update_resume_job(job_id, |job| job.status = ResumeJobStatus::ThreadResuming)?;
        let config = AppServerConfig::new(
            binary.to_path_buf(),
            default_dir.to_path_buf(),
            std::path::PathBuf::from(cwd),
        );
        let mut client = CodexAppServerClient::connect(&config)?;
        let _resumed = client.resume_thread(thread_id, runtime)?;
        self.update_resume_job(job_id, |job| job.status = ResumeJobStatus::ThreadResumed)?;

        let mut verified_goal = goal.cloned();
        if let Some(expected_goal) = goal {
            let mut actual_goal = client
                .read_goal(thread_id)?
                .context("the retried Codex Goal was not returned")?;
            if actual_goal.objective != expected_goal.objective
                || actual_goal.token_budget != expected_goal.token_budget
            {
                anyhow::bail!("the retried Codex Goal did not match the original objective");
            }
            if actual_goal.status == "usageLimited" {
                actual_goal.status = "active".into();
                client.set_goal(thread_id, &actual_goal)?;
            }
            let final_goal = client
                .read_goal(thread_id)?
                .context("The retried Codex Goal disappeared during recovery")?;
            if final_goal.objective != expected_goal.objective
                || final_goal.token_budget != expected_goal.token_budget
                || (expected_goal.status != "usageLimited"
                    && final_goal.status != expected_goal.status)
            {
                anyhow::bail!("the retried Codex Goal did not match the original state");
            }
            verified_goal = Some(final_goal);
        }
        self.update_resume_job(job_id, |job| {
            job.goal_snapshot = verified_goal.clone();
            job.status = ResumeJobStatus::GoalVerified;
        })?;

        let marker = crate::handoff::marker(recovery_id, attempt);
        if client.read_thread(thread_id)?.contains_marker(&marker) {
            self.update_resume_job(job_id, |job| {
                job.status = ResumeJobStatus::Recovered;
                job.last_ack_at = Some(now());
                job.error = None;
            })?;
            self.update_resume_job(job_id, |job| job.status = ResumeJobStatus::Monitoring)?;
            self.emit_recovery_message(app, &format!("Task resumed with {account_name}."));
            if app.is_some() {
                let _ = crate::tools::open_codex_thread_in_app(thread_id);
            }
            return Ok(());
        }

        self.update_resume_job(job_id, |job| {
            job.status = ResumeJobStatus::TurnStarting;
            job.recovery_marker = Some(marker);
        })?;
        let prompt = crate::handoff::recovery_prompt(recovery_id, attempt, manifest_path);
        let client_message_id = format!("{recovery_id}:attempt-{attempt}");
        let ack =
            client.start_turn_and_wait_ack(thread_id, &prompt, runtime, &client_message_id)?;
        self.update_resume_job(job_id, |job| {
            job.resumed_turn_id = Some(ack.turn_id);
            job.last_ack_at = Some(now());
            job.status = ResumeJobStatus::TurnStarted;
            job.error = None;
        })?;
        self.update_resume_job(job_id, |job| job.status = ResumeJobStatus::Recovered)?;
        self.update_resume_job(job_id, |job| job.status = ResumeJobStatus::Monitoring)?;
        self.emit_recovery_message(app, &format!("Task resumed with {account_name}."));
        if app.is_some() {
            let _ = crate::tools::open_codex_thread_in_app(thread_id);
        }
        Ok(())
    }

    fn persist_waiting_resume_job(
        &self,
        thread_id: &str,
        from_account_id: &str,
        cwd: &str,
        rollout: &std::path::Path,
    ) -> Result<String> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        if let Some(job) = data.resume_jobs.iter_mut().find(|job| {
            job.thread_id == thread_id && job.status == ResumeJobStatus::AwaitingTerminal
        }) {
            job.updated_at = now();
            job.rollout_path = rollout.to_string_lossy().to_string();
            job.rollout_offset = std::fs::metadata(rollout)
                .map(|meta| meta.len())
                .unwrap_or(0);
            let id = job.id.clone();
            self.store.save(&data)?;
            return Ok(id);
        }
        let timestamp = now();
        let id = Uuid::new_v4().to_string();
        let recovery_id = Uuid::new_v4().to_string();
        data.resume_jobs.push(ResumeJob {
            id: id.clone(),
            schema_version: 3,
            recovery_id,
            session_id: thread_id.into(),
            thread_id: thread_id.into(),
            failed_turn_id: String::new(),
            resumed_turn_id: None,
            event_key: format!("awaiting-terminal:{thread_id}"),
            cwd: cwd.into(),
            from_account_id: from_account_id.into(),
            to_account_id: None,
            checkpoint_path: String::new(),
            rollout_path: rollout.to_string_lossy().to_string(),
            rollout_offset: std::fs::metadata(rollout)
                .map(|meta| meta.len())
                .unwrap_or(0),
            terminal_error_kind: None,
            terminal_error_fingerprint: None,
            manifest_path: None,
            recovery_marker: None,
            model: None,
            reasoning_effort: None,
            personality: None,
            approval_policy: None,
            sandbox_policy: None,
            goal_snapshot: None,
            attempt: 0,
            status: ResumeJobStatus::AwaitingTerminal,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            last_ack_at: None,
            error: None,
        });
        self.store.save(&data)?;
        Ok(id)
    }

    fn update_resume_job<F>(&self, job_id: &str, update: F) -> Result<()>
    where
        F: FnOnce(&mut ResumeJob),
    {
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        let job = data
            .resume_jobs
            .iter_mut()
            .find(|job| job.id == job_id)
            .context("Recovery job disappeared")?;
        update(job);
        job.updated_at = now();
        self.store.save(&data)?;
        Ok(())
    }

    fn mark_resume_attention(
        &self,
        job_id: &str,
        message: String,
        app: Option<&AppHandle>,
    ) -> Result<()> {
        self.update_resume_job(job_id, |job| {
            job.status = ResumeJobStatus::NeedsAttention;
            job.error = Some(crate::codex_app_server::redact_text(&message));
        })?;
        self.emit_recovery_message(app, &message);
        Ok(())
    }

    fn schedule_capacity_retry(&self, job_id: &str, message: &str) -> Result<Option<u32>> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        let job = data
            .resume_jobs
            .iter_mut()
            .find(|job| job.id == job_id)
            .context("Recovery job disappeared")?;
        if job.attempt >= 3 {
            return Ok(None);
        }
        job.attempt += 1;
        let next_attempt = job.attempt;
        job.status = ResumeJobStatus::Retrying;
        job.error = Some(message.to_string());
        job.updated_at = now();
        self.store.save(&data)?;
        Ok(Some(next_attempt))
    }

    fn emit_recovery_message(&self, app: Option<&AppHandle>, message: &str) {
        if let Some(app) = app {
            let _ = app.emit("auto-switched", message.to_string());
            if let Ok(snapshot) = self.snapshot() {
                let _ = app.emit("snapshot-changed", snapshot);
            }
        }
    }

    fn commit_recovery_account_switch(&self, candidate: &Account) -> Result<()> {
        if candidate.api_provider.is_some() || candidate.state == AccountState::NeedsLogin {
            anyhow::bail!("candidate account is not an eligible subscription account");
        }
        let default_dir = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            resolved_default_config_dir(&data, &ToolId::Codex)
        };
        // This is the one credential copy in the transaction. It runs only after the isolated
        // candidate App Server passed auth/quota/model/effort preflight.
        sync_codex_ide_credentials(
            &self.store,
            &candidate.id,
            candidate.is_default,
            &default_dir,
        )
        .context("Failed to switch the Codex recovery credential")?;
        restart_codex_runtime_after_switch();
        if candidate.is_default {
            clear_active_profile(&ToolId::Codex, &self.store)?;
        } else {
            write_active_profile(&ToolId::Codex, &self.store, &candidate.id)?;
        }
        install_shell_hook(&self.store)?;
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        let timestamp = now();
        normalize_account_states(&mut data.accounts, &ToolId::Codex, Some(&candidate.id));
        if let Some(account) = data
            .accounts
            .iter_mut()
            .find(|account| account.tool_id == ToolId::Codex && account.id == candidate.id)
        {
            account.state = AccountState::Active;
            account.last_used_at = Some(timestamp.clone());
            account.updated_at = timestamp;
        }
        self.store.save(&data)?;
        Ok(())
    }

    /// Startup healing never invents a new turn. It only reads the exact thread and durable
    /// marker, then marks an ambiguous pre-ACK job for explicit attention. This is the crash-safe
    /// boundary: a process restart cannot duplicate a logical recovery turn.
    pub fn heal_resume_jobs(&self, app: Option<&AppHandle>) -> Result<()> {
        let (binary, default_dir, jobs) = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            (
                configured_binary_path(&data, &ToolId::Codex),
                resolved_default_config_dir(&data, &ToolId::Codex),
                data.resume_jobs.clone(),
            )
        };
        let Some(binary) = binary else {
            return Ok(());
        };
        let mut changed = false;
        for job in jobs.into_iter().filter(|job| !job.status.is_terminal()) {
            if job.status == ResumeJobStatus::Retrying {
                self.update_resume_job(&job.id, |item| {
                    item.status = ResumeJobStatus::NeedsAttention;
                    item.error = Some(
                        "Recovery retry was interrupted; inspect the thread before retrying."
                            .into(),
                    );
                })?;
                changed = true;
                continue;
            }
            if matches!(job.status, ResumeJobStatus::AwaitingTerminal) {
                continue;
            }
            if matches!(
                job.status,
                ResumeJobStatus::Checkpointed
                    | ResumeJobStatus::CandidateVerified
                    | ResumeJobStatus::Switching
                    | ResumeJobStatus::ThreadResuming
                    | ResumeJobStatus::ThreadResumed
                    | ResumeJobStatus::GoalVerified
            ) {
                self.update_resume_job(&job.id, |item| {
                    item.status = ResumeJobStatus::NeedsAttention;
                    item.error = Some(
                        "Recovery stopped before an exact turn/started ACK; inspect the thread before retrying.".into(),
                    );
                })?;
                changed = true;
                continue;
            }
            if !matches!(
                job.status,
                ResumeJobStatus::TurnStarting
                    | ResumeJobStatus::TurnStarted
                    | ResumeJobStatus::Recovered
                    | ResumeJobStatus::Monitoring
            ) {
                continue;
            }
            let config = AppServerConfig::new(
                binary.clone(),
                default_dir.clone(),
                std::path::PathBuf::from(&job.cwd),
            );
            let Ok(mut client) = CodexAppServerClient::connect(&config) else {
                self.update_resume_job(&job.id, |item| {
                    item.status = ResumeJobStatus::NeedsAttention;
                    item.error = Some("Resume could not be verified after restart.".into());
                })?;
                changed = true;
                continue;
            };
            let Ok(thread) = client.read_thread(&job.thread_id) else {
                self.update_resume_job(&job.id, |item| {
                    item.status = ResumeJobStatus::NeedsAttention;
                    item.error = Some("Resume could not be verified after restart.".into());
                })?;
                changed = true;
                continue;
            };
            let marker_seen = job
                .recovery_marker
                .as_deref()
                .is_some_and(|marker| thread.contains_marker(marker));
            let turn_seen = job
                .resumed_turn_id
                .as_deref()
                .and_then(|turn_id| thread.turn(turn_id))
                .is_some();
            if !marker_seen && !turn_seen {
                self.update_resume_job(&job.id, |item| {
                    item.status = ResumeJobStatus::NeedsAttention;
                    item.error = Some("Resume could not be verified after restart.".into());
                })?;
                changed = true;
                continue;
            }
            let completed = job
                .resumed_turn_id
                .as_deref()
                .and_then(|turn_id| thread.turn(turn_id))
                .is_some_and(|turn| turn.status.eq_ignore_ascii_case("completed"));
            self.update_resume_job(&job.id, |item| {
                item.status = if completed {
                    ResumeJobStatus::Completed
                } else {
                    ResumeJobStatus::Recovered
                };
                if item.last_ack_at.is_none() {
                    item.last_ack_at = Some(now());
                }
                item.error = None;
            })?;
            if !completed {
                self.update_resume_job(&job.id, |item| item.status = ResumeJobStatus::Monitoring)?;
            }
            changed = true;
        }
        if changed {
            if let Some(app) = app {
                let _ = app.emit("snapshot-changed", self.snapshot()?);
            }
        }
        Ok(())
    }

    pub fn retry_resume_job(&self, app: &AppHandle, job_id: &str) -> Result<AppSnapshot> {
        let rollout = {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let job = data
                .resume_jobs
                .iter_mut()
                .find(|job| job.id == job_id)
                .context("Recovery job not found")?;
            if !matches!(
                job.status,
                ResumeJobStatus::NeedsAttention | ResumeJobStatus::Failed
            ) {
                anyhow::bail!("Recovery job is not waiting for a retry");
            }
            job.attempt = job.attempt.saturating_add(1).max(1);
            job.recovery_marker = Some(crate::handoff::marker(&job.recovery_id, job.attempt));
            job.status = ResumeJobStatus::Retrying;
            job.error = None;
            job.rollout_path.clone()
        };
        let data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?
            .clone();
        self.store.save(&data)?;
        if rollout.is_empty() {
            self.mark_resume_attention(
                job_id,
                "Resume could not be verified: the original rollout is unavailable.".into(),
                Some(app),
            )?;
            return self.snapshot();
        }
        if let Ok(mut active) = self.codex_active_rollout.lock() {
            *active = Some(std::path::PathBuf::from(rollout));
        }
        let _ = self.try_seamless_codex_handoff(
            crate::seamless::DEFAULT_HANDOFF_THRESHOLD,
            true,
            Some(app),
        );
        self.snapshot()
    }

    pub fn cancel_resume_job(&self, job_id: &str) -> Result<AppSnapshot> {
        self.update_resume_job(job_id, |job| {
            if job.status.is_dismissible() {
                job.status = ResumeJobStatus::Cancelled;
                job.error = Some("Cancelled by the user".into());
            }
        })?;
        self.snapshot()
    }

    pub fn open_resume_job(&self, job_id: &str) -> Result<AppSnapshot> {
        let thread_id = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let job = data
                .resume_jobs
                .iter()
                .find(|job| job.id == job_id)
                .context("Recovery job not found")?;
            if !job.status.is_acknowledged() {
                anyhow::bail!("The Codex thread is not acknowledged yet");
            }
            job.thread_id.clone()
        };
        crate::tools::open_codex_thread_in_app(&thread_id)?;
        self.snapshot()
    }

    /// Called by the background poll: if the account has finished logging in (the token exists) →
    /// move NeedsLogin to Idle + read the real quota. Returns true once the token is present.
    fn refresh_runtime_for_account_switch(
        &self,
        tool_id: &ToolId,
        account_id: &str,
        is_default: bool,
        is_api: bool,
    ) -> Result<()> {
        let plan = switch_runtime_plan(tool_id, is_api);
        if plan.sync_codex_ide_credentials {
            let default_dir = {
                let data = self
                    .data
                    .lock()
                    .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
                resolved_default_config_dir(&data, &ToolId::Codex)
            };
            sync_codex_ide_credentials(&self.store, account_id, is_default, &default_dir)
                .context("Failed to switch the Codex IDE account")?;
        }
        if plan.restart_codex_runtime {
            restart_codex_runtime_after_switch();
        }
        Ok(())
    }

    pub fn confirm_login(&self, tool_id: &ToolId, account_id: &str) -> Result<bool> {
        let config_dir = self.store.account_dir(tool_id, account_id);
        if !crate::tools::profile_has_credentials(tool_id, &config_dir) {
            return Ok(false);
        }
        let quota = read_quota(tool_id, &config_dir);
        if quota_requires_reauth(tool_id, &quota) {
            return Ok(false);
        }
        // Seed the onboarding flag only after login completes (claude auth login overwrites
        // .claude.json, so it must be seeded AFTERWARD) — so interactive mode skips the wizard.
        // Re-link the shared session after login too (login may create the real dir).
        crate::tools::seed_onboarding(tool_id, &config_dir);
        if let Some(default_dir) = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            configured_default_config_dir(&data, tool_id)
        } {
            link_shared_sessions_to(tool_id, &config_dir, &default_dir);
            link_shared_config_to(tool_id, &config_dir, &default_dir);
        }
        let mut data = self
            .data
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        let was_active = active_account_id_for(&self.store, tool_id, &data.accounts)
            .is_some_and(|active| active == account_id);
        if let Some(account) = data
            .accounts
            .iter_mut()
            .find(|account| account.tool_id == *tool_id && account.id == account_id)
        {
            if account.state == AccountState::NeedsLogin {
                account.state = AccountState::Idle;
                account.quota = Some(quota);
                account.updated_at = now();
            }
        }
        self.store.save(&data)?;
        crate::tools::clear_codex_reauth_pending(&config_dir);
        drop(data);
        if was_active && *tool_id == ToolId::Codex {
            let default_dir = {
                let data = self
                    .data
                    .lock()
                    .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
                resolved_default_config_dir(&data, tool_id)
            };
            sync_reauthenticated_codex_credentials(&config_dir, &default_dir)?;
            restart_codex_runtime_after_switch();
        }
        Ok(true)
    }

    pub fn reauthenticate_account(
        &self,
        app: &AppHandle,
        tool_id: ToolId,
        account_id: &str,
    ) -> Result<AppSnapshot> {
        if tool_id != ToolId::Codex {
            anyhow::bail!("Re-login is currently available for Codex accounts only");
        }
        let (default_dir, binary, profile, codex_login_method) = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let account = data
                .accounts
                .iter()
                .find(|a| a.tool_id == tool_id && a.id == account_id)
                .context("Codex account not found")?;
            if account.is_default || account.api_provider.is_some() {
                anyhow::bail!("Machine default and API accounts cannot be re-authenticated here");
            }
            (
                resolved_default_config_dir(&data, &tool_id),
                configured_binary_path(&data, &tool_id).context("Codex CLI is not configured")?,
                account_config_dir_with_default(
                    &self.store,
                    account,
                    &resolved_default_config_dir(&data, &tool_id),
                ),
                account.codex_login_method,
            )
        };
        crate::tools::mark_codex_reauth_pending(&profile)?;
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            if let Some(account) = data
                .accounts
                .iter_mut()
                .find(|a| a.tool_id == tool_id && a.id == account_id)
            {
                account.state = AccountState::NeedsLogin;
                account.quota = Some(QuotaInfo::with_message(match codex_login_method {
                    CodexLoginMethod::DeviceCode => {
                        "Codex sign-in expired — approve the new device code"
                    }
                    CodexLoginMethod::BrowserProfile => {
                        "Codex sign-in expired — complete the browser sign-in"
                    }
                }));
                account.updated_at = now();
            }
            self.store.save(&data)?;
        }
        launch_profile_login(
            &tool_id,
            &self.store,
            account_id,
            &default_dir,
            &binary,
            Some(app.clone()),
            codex_login_method,
        )?;
        spawn_login_watch(app.clone(), tool_id, account_id.to_string());
        self.snapshot()
    }

    pub fn add_account(&self, app: &AppHandle, input: AddAccountInput) -> Result<AppSnapshot> {
        validate_name(&input.tool_id, None, &input.name, self)?;
        // Antigravity IDE: each account = its own userData; open the IDE with --user-data-dir
        // to log in a new Google account (the login lives in that dir's state.vscdb).
        if matches!(input.tool_id, ToolId::Antigravity) {
            return self.create_antigravity_account(input);
        }
        // Claude/Codex: create the profile + custom command, then open Terminal to log in.
        self.create_profile_account(app, input)
    }

    /// Import one K12 OpenAI OAuth export into an isolated Codex profile. The raw JSON is parsed
    /// in memory, reduced to Codex's four token fields, and never enters `state.json` or logs.
    pub fn import_codex_account(&self, mut input: ImportCodexAccountInput) -> Result<AppSnapshot> {
        let tool_id = ToolId::Codex;
        let mut raw_session = zeroize::Zeroizing::new(std::mem::take(&mut input.session_json));
        validate_name(&tool_id, None, &input.name, self)?;
        let raw_launcher = input.launcher.trim();
        if raw_launcher.is_empty() {
            anyhow::bail!("A custom command is required for the account (e.g. codex-k12)");
        }
        let id = Uuid::new_v4().to_string();
        let full_launcher = self.validated_launcher(&tool_id, &id, raw_launcher)?;
        let name = normalized_or_default_name(&tool_id, &input.name, self)?;
        let session_result = parse_k12_export(&raw_session);
        // The IPC payload is no longer needed once parsing finishes. Do not retain it across the
        // profile/state work below, including on validation errors.
        raw_session.zeroize();
        let session = session_result?;
        let auth_json = codex_auth_json(&session)?;

        let (default_dir, binary_path) = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let default_dir = configured_default_config_dir(&data, &tool_id)
                .unwrap_or_else(|| default_config_dir(&tool_id));
            let binary_path = configured_binary_path(&data, &tool_id)
                .context("CLI setup is ambiguous — choose the Codex binary first")?;
            (default_dir, binary_path)
        };

        // Avoid silently creating two profiles for the same provider account, including the
        // machine-default profile. Read only the account id from each existing auth file.
        {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let duplicate = data.accounts.iter().any(|account| {
                if account.tool_id != tool_id || account.api_provider.is_some() {
                    return false;
                }
                let config_dir = if account.is_default {
                    default_dir.clone()
                } else {
                    self.store.account_dir(&tool_id, &account.id)
                };
                crate::tools::codex_auth_account_id(&config_dir.join("auth.json")).as_deref()
                    == Some(session.account_id.as_str())
            });
            if duplicate {
                anyhow::bail!("This Codex account is already saved");
            }
        }

        let profile = create_profile_with_default(&tool_id, &self.store, &id, &default_dir)?;
        link_shared_config_to(&tool_id, &profile, &default_dir);

        if let Err(error) = write_codex_auth(&profile, &auth_json) {
            remove_launcher(&full_launcher);
            let _ = delete_account_files(&tool_id, &self.store, &id);
            return Err(error).context("Couldn't install imported Codex credentials");
        }
        if let Err(error) = write_launcher(&tool_id, &self.store, &id, &full_launcher, &binary_path)
        {
            remove_launcher(&full_launcher);
            let _ = delete_account_files(&tool_id, &self.store, &id);
            return Err(error).context("Couldn't create the account's custom command");
        }
        crate::tools::seed_onboarding(&tool_id, &profile);
        if let Err(error) = mark_codex_imported_session(&profile) {
            remove_launcher(&full_launcher);
            let _ = delete_account_files(&tool_id, &self.store, &id);
            return Err(error).context("Couldn't mark the imported Codex profile");
        }

        let timestamp = now();
        let account = Account {
            id: id.clone(),
            tool_id: tool_id.clone(),
            name,
            state: AccountState::Idle,
            fingerprint: format!("profile:{id}"),
            created_at: timestamp.clone(),
            updated_at: timestamp,
            last_used_at: None,
            quota: Some(QuotaInfo::with_message(
                "Imported session — click Refresh to read quota",
            )),
            launcher_command: Some(full_launcher.clone()),
            is_default: false,
            avatar_url: None,
            account_email: session
                .email
                .clone()
                .or_else(|| codex_account_email(&profile)),
            api_provider: None,
            launcher_paths: None,
            quota_status: None,
            // Imported K12 sessions belong to a subscription account. Re-login must use the
            // standard browser OAuth flow rather than the device-code-only path.
            codex_login_method: CodexLoginMethod::BrowserProfile,
        };

        let save_result = {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.accounts.push(account);
            match self.store.save(&data) {
                Ok(()) => Ok(()),
                Err(error) => {
                    data.accounts.retain(|entry| entry.id != id);
                    Err(error)
                }
            }
        };
        if let Err(error) = save_result {
            remove_launcher(&full_launcher);
            let _ = delete_account_files(&tool_id, &self.store, &id);
            return Err(error).context("Couldn't save the imported account");
        }

        self.snapshot()
    }

    /// Import a K12 OAuth export containing multiple Codex subscription accounts.
    ///
    /// Profiles and launchers are staged first; `state.json` is committed only
    /// after every new profile has been written successfully. Imported
    /// accounts are enrolled in the local API gateway pool in the same state
    /// commit, so a single bulk import is immediately represented in both the
    /// native account list and the pool membership list.
    pub fn import_codex_accounts_bulk(
        &self,
        mut input: BulkImportCodexAccountsInput,
    ) -> Result<AppSnapshot> {
        let tool_id = ToolId::Codex;
        let mut raw_session = zeroize::Zeroizing::new(std::mem::take(&mut input.session_json));
        let session_result = parse_k12_export_batch(&raw_session);
        raw_session.zeroize();
        let sessions = session_result?;
        let name_prefix = bulk_prefix(input.name_prefix.as_deref(), "acc", "Account name prefix")?;
        let launcher_prefix =
            bulk_prefix(input.launcher_prefix.as_deref(), "bulk-", "Launcher prefix")?;

        let (default_dir, binary_path, existing_account_ids) = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let default_dir = configured_default_config_dir(&data, &tool_id)
                .unwrap_or_else(|| default_config_dir(&tool_id));
            let binary_path = configured_binary_path(&data, &tool_id)
                .context("CLI setup is ambiguous — choose the Codex binary first")?;
            let existing_account_ids = data
                .accounts
                .iter()
                .filter(|account| account.tool_id == tool_id && account.api_provider.is_none())
                .filter_map(|account| {
                    let config_dir = if account.is_default {
                        default_dir.clone()
                    } else {
                        self.store.account_dir(&tool_id, &account.id)
                    };
                    crate::tools::codex_auth_account_id(&config_dir.join("auth.json"))
                })
                .collect::<HashSet<_>>();
            (default_dir, binary_path, existing_account_ids)
        };

        let mut staged_files: Vec<(String, String)> = Vec::new();
        let mut imported_accounts = Vec::new();
        let mut imported_profile_ids = Vec::new();
        let mut imported_provider_ids = Vec::new();
        let mut reserved_names = HashSet::new();
        let mut reserved_launchers = HashSet::new();
        let mut next_label = 1_usize;

        let cleanup = |files: &[(String, String)]| {
            for (account_id, launcher) in files {
                remove_launcher(launcher);
                let _ = delete_account_files(&tool_id, &self.store, account_id);
            }
        };

        for session in sessions {
            // Re-running the same export is safe: already-known provider
            // accounts are skipped instead of producing duplicate profiles.
            if existing_account_ids.contains(&session.account_id) {
                continue;
            }

            let id = Uuid::new_v4().to_string();
            let (name, full_launcher) = match next_bulk_account_labels(
                &self.data,
                &name_prefix,
                &launcher_prefix,
                &mut next_label,
                &mut reserved_names,
                &mut reserved_launchers,
            ) {
                Ok(labels) => labels,
                Err(error) => {
                    cleanup(&staged_files);
                    return Err(error);
                }
            };
            let auth_json = match codex_auth_json(&session) {
                Ok(auth_json) => auth_json,
                Err(error) => {
                    cleanup(&staged_files);
                    return Err(error);
                }
            };
            let profile =
                match create_profile_with_default(&tool_id, &self.store, &id, &default_dir) {
                    Ok(profile) => profile,
                    Err(error) => {
                        let _ = delete_account_files(&tool_id, &self.store, &id);
                        cleanup(&staged_files);
                        return Err(error.context("Couldn't create an imported Codex profile"));
                    }
                };
            link_shared_config_to(&tool_id, &profile, &default_dir);

            if let Err(error) = write_codex_auth(&profile, &auth_json) {
                cleanup(&staged_files);
                let _ = delete_account_files(&tool_id, &self.store, &id);
                return Err(error).context("Couldn't install imported Codex credentials");
            }
            if let Err(error) =
                write_launcher(&tool_id, &self.store, &id, &full_launcher, &binary_path)
            {
                cleanup(&staged_files);
                let _ = delete_account_files(&tool_id, &self.store, &id);
                return Err(error).context("Couldn't create an imported Codex command");
            }
            crate::tools::seed_onboarding(&tool_id, &profile);
            if let Err(error) = mark_codex_imported_session(&profile) {
                remove_launcher(&full_launcher);
                cleanup(&staged_files);
                let _ = delete_account_files(&tool_id, &self.store, &id);
                return Err(error).context("Couldn't mark the imported Codex profile");
            }

            let timestamp = now();
            let fingerprint = format!("profile:{id}");
            imported_provider_ids.push(session.account_id.clone());
            imported_profile_ids.push(id.clone());
            staged_files.push((id.clone(), full_launcher.clone()));
            imported_accounts.push(Account {
                id,
                tool_id: tool_id.clone(),
                name,
                state: AccountState::Idle,
                fingerprint,
                created_at: timestamp.clone(),
                updated_at: timestamp,
                last_used_at: None,
                quota: Some(QuotaInfo::with_message(
                    "Imported session — click Refresh to read quota",
                )),
                launcher_command: Some(full_launcher),
                is_default: false,
                avatar_url: None,
                account_email: session
                    .email
                    .clone()
                    .or_else(|| codex_account_email(&profile)),
                api_provider: None,
                launcher_paths: None,
                quota_status: None,
                codex_login_method: CodexLoginMethod::BrowserProfile,
            });
        }

        if imported_accounts.is_empty() {
            anyhow::bail!("All accounts in this export are already saved");
        }

        let save_result = {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let original_data = data.clone();
            // A concurrent import may have landed while profiles were being
            // staged. Do not commit a second profile for the same provider id.
            let current_ids = data
                .accounts
                .iter()
                .filter(|account| account.tool_id == tool_id && account.api_provider.is_none())
                .filter_map(|account| {
                    let config_dir = if account.is_default {
                        default_dir.clone()
                    } else {
                        self.store.account_dir(&tool_id, &account.id)
                    };
                    crate::tools::codex_auth_account_id(&config_dir.join("auth.json"))
                })
                .collect::<HashSet<_>>();
            let imported_provider_id_set = imported_provider_ids
                .iter()
                .cloned()
                .collect::<HashSet<_>>();
            if current_ids
                .intersection(&imported_provider_id_set)
                .next()
                .is_none()
            {
                data.accounts.extend(imported_accounts);
                crate::api_gateway::enroll_codex_accounts(
                    &mut data.api_gateway,
                    imported_profile_ids.iter(),
                );
                match self.store.save(&data) {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        *data = original_data;
                        Err(error)
                    }
                }
            } else {
                Err(anyhow::anyhow!(
                    "Codex account state changed while importing; nothing was committed"
                ))
            }
        };
        if let Err(error) = save_result {
            cleanup(&staged_files);
            return Err(error).context("Couldn't save the imported Codex accounts");
        }

        self.snapshot()
    }

    /// Run the opt-in local login bridge, then feed its short-lived OAuth export through the
    /// existing transactional bulk importer. The bridge process owns the browser interaction;
    /// this method never stores raw email/password/2FA material in app state.
    pub fn bulk_login_codex_staging(
        &self,
        input: BulkLoginCodexInput,
        resource_dir: Option<std::path::PathBuf>,
    ) -> Result<AppSnapshot> {
        let oauth_export = run_staging_bulk_login(input, resource_dir.as_deref())?;
        self.import_codex_accounts_bulk(BulkImportCodexAccountsInput {
            session_json: oauth_export,
            name_prefix: Some("acc".to_string()),
            launcher_prefix: Some("bulk-".to_string()),
        })
    }

    /// Save the Antigravity IDE account currently logged in: capture the token from the default
    /// state.vscdb. The user must ensure the IDE is logged into the exact account they want to save.
    fn create_antigravity_account(&self, input: AddAccountInput) -> Result<AppSnapshot> {
        // The IDE only writes the token to state.vscdb on EXIT (lazy write) → quit to flush
        // the logged-in session, read the token, then reopen the IDE for the user (regardless of capture outcome).
        antigravity_quit_ide();
        let id = Uuid::new_v4().to_string();
        let captured = antigravity_capture(&self.store, &id);
        let _ = antigravity_open_ide();
        let fingerprint = captured.context("Failed to save account")?;

        // Don't save duplicates: the same Google account = the same avatar (even if the token blob
        // differs across two captures). Fall back to comparing tokens if the avatar is missing.
        // Remove the just-captured dir and report.
        let new_profile = antigravity_saved_profile(&self.store, &id);
        let new_token = antigravity_saved_token(&self.store, &id);
        {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let dup = data.accounts.iter().any(|account| {
                if account.tool_id != ToolId::Antigravity {
                    return false;
                }
                match (
                    &new_profile,
                    antigravity_saved_profile(&self.store, &account.id),
                ) {
                    (Some(new), Some(existing)) => *new == existing,
                    _ => antigravity_saved_token(&self.store, &account.id) == new_token,
                }
            });
            if dup {
                drop(data);
                let _ = delete_account_files(&ToolId::Antigravity, &self.store, &id);
                anyhow::bail!("This account is already saved");
            }
        }

        let name = normalized_or_default_name(&ToolId::Antigravity, &input.name, self)?;
        let quota = read_quota(
            &ToolId::Antigravity,
            &default_config_dir(&ToolId::Antigravity),
        );
        let timestamp = now();
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.accounts.push(Account {
                id,
                tool_id: ToolId::Antigravity,
                name,
                state: AccountState::Idle,
                fingerprint,
                created_at: timestamp.clone(),
                updated_at: timestamp,
                last_used_at: None,
                quota: Some(quota),
                launcher_command: None,
                is_default: false,
                avatar_url: None,
                account_email: None,
                api_provider: None,
                launcher_paths: None,
                quota_status: None,
                codex_login_method: CodexLoginMethod::DeviceCode,
            });
            self.store.save(&data)?;
        }
        self.snapshot()
    }

    fn create_profile_account(
        &self,
        app: &AppHandle,
        input: AddAccountInput,
    ) -> Result<AppSnapshot> {
        // A launcher is required for Claude/Codex — it's the ONLY way to use the account.
        let raw_launcher = input
            .launcher
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("A custom command is required for the account (e.g. claude-work)")?;
        let id = Uuid::new_v4().to_string();
        let full_launcher = self.validated_launcher(&input.tool_id, &id, raw_launcher)?;
        let name = normalized_or_default_name(&input.tool_id, &input.name, self)?;

        let default_dir = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            configured_default_config_dir(&data, &input.tool_id)
        }
        .context("CLI setup is ambiguous — choose the tool's binary and default config first")?;
        let binary_path = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            configured_binary_path(&data, &input.tool_id)
        }
        .context("CLI setup is ambiguous — choose the tool's binary first")?;
        launch_profile_login(
            &input.tool_id,
            &self.store,
            &id,
            &default_dir,
            &binary_path,
            Some(app.clone()),
            input.codex_login_method,
        )
        .context("Login not completed, account not added")?;
        write_launcher(
            &input.tool_id,
            &self.store,
            &id,
            &full_launcher,
            &binary_path,
        )
        .context("Couldn't create the account's custom command")?;

        let timestamp = now();
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.accounts.push(Account {
                id: id.clone(),
                tool_id: input.tool_id.clone(),
                name,
                state: AccountState::NeedsLogin,
                fingerprint: format!("profile:{id}"),
                created_at: timestamp.clone(),
                updated_at: timestamp,
                last_used_at: None,
                quota: Some(crate::models::QuotaInfo::with_message(
                    "Waiting for browser approval — the app will detect it when done",
                )),
                launcher_command: Some(full_launcher),
                is_default: false,
                avatar_url: None,
                account_email: None,
                api_provider: None,
                launcher_paths: None,
                quota_status: None,
                codex_login_method: input.codex_login_method,
            });
            self.store.save(&data)?;
        }

        // Background poll: login done (token appears) → NeedsLogin to Idle + read quota.
        spawn_login_watch(app.clone(), input.tool_id.clone(), id);
        self.snapshot()
    }

    /// Add an API/proxy account (Codex): create the profile, write the gateway config + key, and an
    /// optional custom command. No OAuth login → the account is ready (Idle) immediately, with no quota.
    pub fn add_api_account(&self, input: AddApiAccountInput) -> Result<AppSnapshot> {
        if !matches!(input.tool_id, ToolId::Codex | ToolId::Claude) {
            anyhow::bail!("API/proxy accounts are only supported for Codex and Claude Code");
        }
        validate_name(&input.tool_id, None, &input.name, self)?;

        let base_url = input.base_url.trim().to_string();
        if !base_url.starts_with("https://") {
            anyhow::bail!("Gateway URL must start with https://");
        }
        let api_key = input.api_key.trim().to_string();
        if api_key.is_empty() {
            anyhow::bail!("API key is required");
        }
        let model = input.model.trim().to_string();
        if model.is_empty() {
            anyhow::bail!("Pick a model");
        }

        let id = Uuid::new_v4().to_string();
        let name = normalized_or_default_name(&input.tool_id, &input.name, self)?;

        // Validate the optional launcher up front (collision/charset) before writing anything.
        let full_launcher = match input
            .launcher
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(raw) => Some(self.validated_launcher(&input.tool_id, &id, raw)?),
            None => None,
        };

        let default_dir = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            configured_default_config_dir(&data, &input.tool_id)
        }
        .context("CLI setup is ambiguous — choose the tool's binary and default config first")?;
        let profile = create_profile_with_default(&input.tool_id, &self.store, &id, &default_dir)?;
        let binary_path = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            configured_binary_path(&data, &input.tool_id)
        }
        .context("CLI setup is ambiguous — choose the tool's binary first")?;
        match input.tool_id {
            ToolId::Codex => {
                write_codex_proxy_config(&profile, &name, &base_url, &model)
                    .context("Couldn't write the Codex config")?;
                write_api_key_file(&profile, &api_key).context("Couldn't store the API key")?;
            }
            ToolId::Claude => {
                write_claude_proxy_settings(&profile, &base_url, &api_key, &model)
                    .context("Couldn't write the Claude settings")?;
                // Skip the first-run wizard so the bare command / launcher start straight into a session.
                crate::tools::seed_onboarding(&input.tool_id, &profile);
            }
            ToolId::Antigravity => unreachable!("guarded above"),
        }
        if let Some(full) = &full_launcher {
            write_api_launcher(
                &input.tool_id,
                &self.store,
                &id,
                full,
                &model,
                input.bypass,
                &binary_path,
            )
            .context("Couldn't create the account's custom command")?;
        }

        let timestamp = now();
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.accounts.push(Account {
                id,
                tool_id: input.tool_id.clone(),
                name,
                state: AccountState::Idle,
                fingerprint: "api".to_string(),
                created_at: timestamp.clone(),
                updated_at: timestamp,
                last_used_at: None,
                // API/proxy gateways expose no quota — hide the bars.
                quota: None,
                launcher_command: full_launcher,
                is_default: false,
                avatar_url: None,
                account_email: None,
                api_provider: Some(ApiProvider {
                    base_url,
                    model,
                    bypass: input.bypass,
                }),
                launcher_paths: None,
                quota_status: None,
                codex_login_method: CodexLoginMethod::DeviceCode,
            });
            self.store.save(&data)?;
        }
        self.snapshot()
    }

    pub fn rename_account(&self, input: RenameAccountInput) -> Result<AppSnapshot> {
        validate_name(&input.tool_id, Some(&input.account_id), &input.name, self)?;
        let name = if input.name.trim().is_empty() {
            normalized_or_default_name(&input.tool_id, "", self)?
        } else {
            input.name.trim().to_string()
        };

        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let account = data
                .accounts
                .iter_mut()
                .find(|account| account.tool_id == input.tool_id && account.id == input.account_id)
                .context("Account not found")?;
            if account.is_default {
                anyhow::bail!("Can't rename the machine default account");
            }
            if is_virtual_api_account(account) {
                anyhow::bail!("Local API accounts are managed from the API tab");
            }
            account.name = name;
            account.updated_at = now();
            self.store.save(&data)?;
        }

        self.snapshot()
    }

    /// Set/rename the account's custom command (Claude/Codex).
    pub fn set_launcher(&self, input: SetLauncherInput) -> Result<AppSnapshot> {
        let raw = input.name.trim();
        if raw.is_empty() {
            anyhow::bail!("Command name is empty");
        }
        let full = self.validated_launcher(&input.tool_id, &input.account_id, raw)?;
        let (old_launcher, api) = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let account = data
                .accounts
                .iter()
                .find(|a| a.tool_id == input.tool_id && a.id == input.account_id)
                .context("Account not found")?;
            if is_virtual_api_account(account) {
                anyhow::bail!("Local API accounts are managed from the API tab");
            }
            (
                account.launcher_command.clone(),
                account
                    .api_provider
                    .as_ref()
                    .map(|p| (p.model.clone(), p.bypass)),
            )
        };

        // API/proxy accounts need a launcher that exports the key + pins the model (+ optional bypass).
        match api {
            Some((model, bypass)) => {
                let binary_path = {
                    let data = self
                        .data
                        .lock()
                        .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
                    configured_binary_path(&data, &input.tool_id)
                }
                .context("CLI setup is ambiguous — choose the tool's binary first")?;
                write_api_launcher(
                    &input.tool_id,
                    &self.store,
                    &input.account_id,
                    &full,
                    &model,
                    bypass,
                    &binary_path,
                )
                .context("Couldn't create the custom command")?;
            }
            None => {
                let binary_path = {
                    let data = self
                        .data
                        .lock()
                        .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
                    configured_binary_path(&data, &input.tool_id)
                }
                .context("CLI setup is ambiguous — choose the tool's binary first")?;
                write_launcher(
                    &input.tool_id,
                    &self.store,
                    &input.account_id,
                    &full,
                    &binary_path,
                )
                .context("Couldn't create the custom command")?;
            }
        }
        if let Some(old) = old_launcher.filter(|old| old != &full) {
            remove_launcher(&old);
        }

        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            if let Some(account) = data
                .accounts
                .iter_mut()
                .find(|a| a.tool_id == input.tool_id && a.id == input.account_id)
            {
                account.launcher_command = Some(full);
                account.updated_at = now();
            }
            self.store.save(&data)?;
        }
        self.snapshot()
    }

    /// Switch = pick the account for the PLAIN `claude`/`codex` command (via shell hook +
    /// active file, WITHOUT wrapping the binary). Antigravity still copy-swaps credentials.
    pub fn switch_account(&self, input: SwitchAccountInput) -> Result<AppSnapshot> {
        let (is_default, state, is_api) = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let account = data
                .accounts
                .iter()
                .find(|account| account.tool_id == input.tool_id && account.id == input.account_id)
                .context("Failed to switch account — kept the previous account")?;
            (
                account.is_default,
                account.state.clone(),
                account.api_provider.is_some(),
            )
        };

        if state == AccountState::NeedsLogin {
            anyhow::bail!("Account hasn't finished logging in yet");
        }

        match input.tool_id {
            ToolId::Claude | ToolId::Codex => {
                if input.tool_id == ToolId::Codex && !is_api {
                    let default_dir = {
                        let data = self
                            .data
                            .lock()
                            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
                        resolved_default_config_dir(&data, &ToolId::Codex)
                    };
                    sync_codex_ide_credentials(
                        &self.store,
                        &input.account_id,
                        is_default,
                        &default_dir,
                    )
                    .context(
                        "Failed to switch the Codex IDE account — kept the previous account",
                    )?;
                    restart_codex_runtime_after_switch();
                }
                if is_default {
                    clear_active_profile(&input.tool_id, &self.store)
                        .context("Failed to switch account — kept the previous account")?;
                } else {
                    write_active_profile(&input.tool_id, &self.store, &input.account_id)
                        .context("Failed to switch account — kept the previous account")?;
                }
                install_shell_hook(&self.store)
                    .context("Couldn't install the shell hook (~/.zshrc)")?;
            }
            ToolId::Antigravity => {
                // Quit the IDE (to avoid overwriting state.vscdb on exit) → write the chosen
                // account's token into the default state.vscdb → reopen the IDE logged into that account.
                antigravity_quit_ide();
                antigravity_restore(&self.store, &input.account_id)
                    .context("Failed to switch account — check Antigravity IDE")?;
                let _ = antigravity_open_ide();
            }
        }

        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let timestamp = now();
            normalize_account_states(&mut data.accounts, &input.tool_id, Some(&input.account_id));
            for account in data.accounts.iter_mut().filter(|account| {
                account.tool_id == input.tool_id && account.id == input.account_id
            }) {
                account.state = if is_exhausted(account) {
                    AccountState::Exhausted
                } else {
                    AccountState::Active
                };
                account.last_used_at = Some(timestamp.clone());
                account.updated_at = timestamp.clone();
            }
            self.store.save(&data)?;
        }

        if input.tool_id == ToolId::Codex && !is_api {
            refresh_codex_frontends_after_switch();
        }

        self.snapshot()
    }

    pub fn delete_account(&self, tool_id: ToolId, account_id: String) -> Result<AppSnapshot> {
        let (original_data, launcher, was_active) = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let account = data
                .accounts
                .iter()
                .find(|a| a.tool_id == tool_id && a.id == account_id)
                .context("Account not found")?;
            if account.is_default {
                anyhow::bail!("Can't delete the machine default account");
            }
            // "In use" comes from the active file, NOT from `state == Active`: an account that hit
            // its quota is still the one the plain command runs as, but its state is Exhausted.
            // Reading state here would leave `active/<tool>.profile` pointing at a deleted dir.
            let was_active = active_account_id_for(&self.store, &tool_id, &data.accounts)
                .as_deref()
                == Some(account_id.as_str());
            (data.clone(), account.launcher_command.clone(), was_active)
        };

        // A configured vault must be unlocked before deleting an account so its matching secret
        // record can be removed in the same user action instead of leaving an orphaned credential.
        self.vault.ensure_unlocked_for_account_delete()?;

        let active_path = self.store.active_profile_path(&tool_id);
        let active_contents = if was_active {
            std::fs::read(&active_path).ok()
        } else {
            None
        };
        let original_credential = self.vault.get(&tool_id, &account_id)?;
        let staged = stage_account_files(&tool_id, &self.store, &account_id)?;

        // Every failure after staging restores state, credentials, the active profile file, and
        // the profile directory. The profile is only permanently removed after all durable
        // records have committed successfully.
        let rollback = |error: anyhow::Error| -> Result<AppSnapshot> {
            let mut rollback_errors = Vec::new();
            if let Err(rollback_error) = self.store.save(&original_data) {
                rollback_errors.push(format!("state: {rollback_error}"));
            }
            if let Some(credential) = original_credential.as_ref() {
                if let Err(rollback_error) =
                    self.vault.save(vault_input_from_credential(credential))
                {
                    rollback_errors.push(format!("vault: {rollback_error}"));
                }
            }
            if was_active {
                if let Err(rollback_error) =
                    restore_active_profile_file(&active_path, active_contents.as_deref())
                {
                    rollback_errors.push(format!("active profile: {rollback_error}"));
                }
            }
            if let Some(staged_path) = staged.as_deref() {
                if let Err(rollback_error) =
                    restore_staged_account_files(&tool_id, &self.store, &account_id, staged_path)
                {
                    rollback_errors.push(format!("account files: {rollback_error}"));
                }
            }
            if rollback_errors.is_empty() {
                Err(error)
            } else {
                Err(error.context(format!(
                    "Account deletion rolled back with warnings: {}",
                    rollback_errors.join("; ")
                )))
            }
        };

        let mut next_data = original_data.clone();
        next_data
            .accounts
            .retain(|account| !(account.tool_id == tool_id && account.id == account_id));
        next_data
            .api_gateway
            .accounts
            .retain(|entry| !(entry.tool_id == tool_id && entry.account_id == account_id));
        next_data
            .api_gateway
            .model_registry
            .retain(|entry| !(entry.tool_id == tool_id && entry.account_id == account_id));
        next_data.auto_prime.remove(&account_id);
        next_data.auto_ping.remove(&account_id);

        if let Err(error) = self.store.save(&next_data) {
            return rollback(error.context("Couldn't save account deletion"));
        }
        if let Err(error) = self.vault.delete(&tool_id, &account_id) {
            return rollback(error.context("Couldn't remove the account's saved credentials"));
        }

        // Deleting the active account (the one the plain command uses) → clear the active file so
        // the plain command falls back to the machine Default, NOT leaving it pointing to a deleted profile.
        if was_active && matches!(tool_id, ToolId::Claude | ToolId::Codex) {
            if let Err(error) = clear_active_profile(&tool_id, &self.store)
                .and_then(|_| install_shell_hook(&self.store))
            {
                return rollback(
                    error.context("Couldn't clear the deleted account's active profile"),
                );
            }
        }

        if let Some(name) = launcher {
            remove_launcher(&name);
        }
        if let Some(staged_path) = staged.as_deref() {
            if let Err(error) =
                finalize_staged_account_files(&tool_id, &self.store, &account_id, staged_path)
            {
                return rollback(error.context("Couldn't finalize account deletion"));
            }
        } else {
            // No profile directory was present (for example, an interrupted prior cleanup), but
            // the encrypted record still needs the same final cleanup path.
            delete_account_files(&tool_id, &self.store, &account_id)?;
        }

        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            *data = self.store.load().unwrap_or(next_data);
        }
        self.snapshot()
    }

    pub fn accept_disclaimer(&self) -> Result<AppSnapshot> {
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.disclaimer_accepted = true;
            self.store.save(&data)?;
        }
        self.snapshot()
    }

    /// Bring the Antigravity IDE to the login screen to add an account that has never logged in.
    pub fn antigravity_new_login(&self) -> Result<AppSnapshot> {
        let saved: Vec<String> = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.accounts
                .iter()
                .filter(|account| account.tool_id == ToolId::Antigravity)
                .filter_map(|account| antigravity_saved_token(&self.store, &account.id))
                .collect()
        };
        antigravity_new_login(&saved)?;
        self.snapshot()
    }

    /// Normalize + validate the command name: enforce the prefix and charset, no collision with
    /// another account's launcher, and no overriding a system binary.
    fn validated_launcher(&self, tool_id: &ToolId, account_id: &str, raw: &str) -> Result<String> {
        let full = full_launcher_name(tool_id, raw)?;
        {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let dup = data.accounts.iter().any(|a| {
                a.id != account_id && a.launcher_command.as_deref() == Some(full.as_str())
            });
            if dup {
                anyhow::bail!("Command '{full}' is already used by another account");
            }
        }
        if launcher_name_collides_with_system(&full) {
            anyhow::bail!("Command '{full}' conflicts with an existing system command");
        }
        Ok(full)
    }
}

/// Watch in the background after opening Terminal to log in: check every ~2s whether the token
/// exists, for up to ~3 minutes. Token present → confirm_login + emit to update the UI.
fn spawn_login_watch(app: AppHandle, tool_id: ToolId, account_id: String) {
    std::thread::spawn(move || {
        for _ in 0..90 {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let state = app.state::<ManagedState>();
            if let Ok(true) = state.confirm_login(&tool_id, &account_id) {
                if let Ok(snapshot) = state.snapshot() {
                    let _ = app.emit("snapshot-changed", snapshot);
                }
                break;
            }
        }
    });
}

/// The account's config dir for reading quota: profile accounts read from their own directory,
/// the rest (default, antigravity import) read from the machine's default config dir.
fn account_config_dir_with_default(
    store: &Store,
    account: &Account,
    default_config_dir: &std::path::Path,
) -> std::path::PathBuf {
    if account.fingerprint.starts_with("profile:") {
        store.account_dir(&account.tool_id, &account.id)
    } else {
        default_config_dir.to_path_buf()
    }
}

fn resolved_default_config_dir(data: &StoredState, tool_id: &ToolId) -> std::path::PathBuf {
    configured_default_config_dir(data, tool_id).unwrap_or_else(|| default_config_dir(tool_id))
}

fn configured_default_config_dir(
    data: &StoredState,
    tool_id: &ToolId,
) -> Option<std::path::PathBuf> {
    data.tool_setups
        .get(tool_id.as_str())
        .and_then(|setup| setup.default_config_dir.clone())
}

fn configured_binary_path(data: &StoredState, tool_id: &ToolId) -> Option<std::path::PathBuf> {
    data.tool_setups
        .get(tool_id.as_str())
        .and_then(|setup| setup.binary_path.clone())
}

fn default_auto_switch_setting_from_legacy(data: &StoredState) -> AutoSwitchSetting {
    AutoSwitchSetting {
        enabled: data.auto_switch,
        threshold: data.auto_switch_threshold.clamp(50.0, 100.0),
    }
}

fn auto_switch_setting(data: &StoredState, tool_id: &ToolId) -> AutoSwitchSetting {
    data.auto_switch_settings
        .get(tool_id.as_str())
        .cloned()
        .unwrap_or_else(|| default_auto_switch_setting_from_legacy(data))
}

/// Drop the old-style "system-default" account, ensuring each CLI tool has one machine Default
/// account (pointing to ~/.claude, ~/.codex) — read-only, reading quota like a normal account.
fn migrate_defaults(accounts: &mut Vec<Account>) {
    accounts.retain(|a| a.id != "system-default");
    // Antigravity (capture/swap) has no machine Default account — remove the old one if present.
    accounts.retain(|a| a.id != "default-antigravity");
    for tool_id in [ToolId::Claude, ToolId::Codex] {
        let default_id = format!("default-{}", tool_id.as_str());
        if accounts.iter().any(|a| a.id == default_id) {
            continue;
        }
        let timestamp = now();
        accounts.push(Account {
            id: default_id,
            tool_id: tool_id.clone(),
            name: "Machine default".to_string(),
            state: AccountState::Idle,
            fingerprint: "default".to_string(),
            created_at: timestamp.clone(),
            updated_at: timestamp,
            last_used_at: None,
            quota: Some(crate::models::QuotaInfo::with_message(
                "Click Refresh to read quota",
            )),
            launcher_command: None,
            is_default: true,
            avatar_url: None,
            account_email: None,
            api_provider: None,
            launcher_paths: None,
            quota_status: None,
            codex_login_method: CodexLoginMethod::DeviceCode,
        });
    }
}

/// Older releases could persist a Codex quota HTTP 401 while leaving the account in Idle/Active.
/// Heal that mismatch during startup so the installed app immediately offers Re-login without
/// requiring another network refresh. Machine-default and API/proxy accounts use different auth
/// ownership, so they intentionally remain unchanged.
fn migrate_codex_reauth_states(accounts: &mut [Account]) {
    for account in accounts.iter_mut().filter(|account| {
        account.tool_id == ToolId::Codex
            && !account.is_default
            && account.api_provider.is_none()
            && account
                .quota
                .as_ref()
                .is_some_and(|quota| quota_requires_reauth(&ToolId::Codex, quota))
    }) {
        account.state = AccountState::NeedsLogin;
    }
}

/// K12 imports use standard browser subscription OAuth when they need to refresh. The marker
/// covers newly imported profiles; the launcher/name fallback upgrades accounts created by the
/// first importer release, which predates the marker and used the documented `k12` defaults.
fn migrate_codex_imported_login_methods(store: &Store, accounts: &mut [Account]) {
    for account in accounts.iter_mut().filter(|account| {
        account.tool_id == ToolId::Codex
            && !account.is_default
            && account.api_provider.is_none()
            && account.codex_login_method == CodexLoginMethod::DeviceCode
    }) {
        let profile = store.account_dir(&ToolId::Codex, &account.id);
        let legacy_defaults = account.name.trim().eq_ignore_ascii_case("k12")
            && account
                .launcher_command
                .as_deref()
                .is_some_and(|launcher| launcher.trim().eq_ignore_ascii_case("codex-k12"));
        if is_codex_imported_session(&profile) || legacy_defaults {
            account.codex_login_method = CodexLoginMethod::BrowserProfile;
        }
    }
}

fn migrate_auto_switch_settings(data: &mut StoredState) {
    let legacy = default_auto_switch_setting_from_legacy(data);
    for tool_id in [ToolId::Claude, ToolId::Codex] {
        data.auto_switch_settings
            .entry(tool_id.as_str().to_string())
            .or_insert_with(|| legacy.clone());
    }
    data.auto_switch_settings
        .remove(ToolId::Antigravity.as_str());
    data.auto_switch = data
        .auto_switch_settings
        .values()
        .any(|setting| setting.enabled);
}

fn migrate_resume_jobs(data: &mut StoredState) {
    const RECOVERY_SCHEMA_VERSION: u32 = 4;
    const TERMINAL_HISTORY_LIMIT: usize = 20;

    if data.schema_version < RECOVERY_SCHEMA_VERSION {
        for job in &mut data.resume_jobs {
            job.status = match &job.status {
                ResumeJobStatus::Resumed => ResumeJobStatus::Completed,
                ResumeJobStatus::Failed => ResumeJobStatus::Cancelled,
                status => status.clone(),
            };
        }
        data.schema_version = RECOVERY_SCHEMA_VERSION;
    }

    let terminal_count = data
        .resume_jobs
        .iter()
        .filter(|job| job.status.is_terminal())
        .count();
    let mut terminal_to_drop = terminal_count.saturating_sub(TERMINAL_HISTORY_LIMIT);
    data.resume_jobs.retain(|job| {
        if terminal_to_drop > 0 && job.status.is_terminal() {
            terminal_to_drop -= 1;
            false
        } else {
            true
        }
    });
}

fn retire_auto_ping_settings(data: &mut StoredState) {
    data.auto_ping_enabled = false;
    data.auto_ping.clear();
    data.schema_version = data.schema_version.max(3);
}

fn natural_name_cmp(left: &str, right: &str) -> Ordering {
    let left_chunks = natural_chunks(left);
    let right_chunks = natural_chunks(right);

    for ((left_chunk, left_is_digit), (right_chunk, right_is_digit)) in
        left_chunks.iter().zip(right_chunks.iter())
    {
        let order = if *left_is_digit && *right_is_digit {
            let left_significant = left_chunk.trim_start_matches('0');
            let right_significant = right_chunk.trim_start_matches('0');
            let left_significant = if left_significant.is_empty() {
                "0"
            } else {
                left_significant
            };
            let right_significant = if right_significant.is_empty() {
                "0"
            } else {
                right_significant
            };

            left_significant
                .len()
                .cmp(&right_significant.len())
                .then_with(|| left_significant.cmp(right_significant))
                .then_with(|| left_chunk.len().cmp(&right_chunk.len()))
        } else {
            left_chunk.cmp(right_chunk)
        };

        if order != Ordering::Equal {
            return order;
        }
    }

    left_chunks
        .len()
        .cmp(&right_chunks.len())
        .then_with(|| left.cmp(right))
}

fn natural_chunks(value: &str) -> Vec<(&str, bool)> {
    let mut chunks = Vec::new();
    let mut chunk_start = 0;
    let mut chunk_is_digit = None;

    for (index, character) in value.char_indices() {
        let is_digit = character.is_ascii_digit();
        match chunk_is_digit {
            None => {
                chunk_start = index;
                chunk_is_digit = Some(is_digit);
            }
            Some(previous_is_digit) if previous_is_digit != is_digit => {
                chunks.push((&value[chunk_start..index], previous_is_digit));
                chunk_start = index;
                chunk_is_digit = Some(is_digit);
            }
            Some(_) => {}
        }
    }

    if let Some(is_digit) = chunk_is_digit {
        chunks.push((&value[chunk_start..], is_digit));
    }

    chunks
}

fn build_snapshot(
    store: &Store,
    data: &StoredState,
    status: crate::models::ApiGatewayStatus,
) -> AppSnapshot {
    let show_virtual_api = status.state == ApiGatewayServerState::Running;
    let tools = visible_tools()
        .into_iter()
        .map(|tool_id| {
            let mut accounts = data
                .accounts
                .iter()
                .filter(|account| {
                    account.tool_id == tool_id
                        && (show_virtual_api || !is_virtual_api_account(account))
                })
                .cloned()
                .collect::<Vec<_>>();
            // Default first, then names in natural order (`acc2` before `acc10`).
            accounts.sort_by(|a, b| {
                b.is_default
                    .cmp(&a.is_default)
                    .then_with(|| natural_name_cmp(&a.name, &b.name))
            });
            // Antigravity: attach the Google avatar (account identity) for the UI to display.
            if matches!(tool_id, ToolId::Antigravity) {
                for account in accounts.iter_mut() {
                    account.avatar_url = crate::tools::antigravity_avatar_url(store, &account.id);
                }
            }
            if matches!(tool_id, ToolId::Codex) {
                let default_dir = resolved_default_config_dir(data, &ToolId::Codex);
                for account in accounts
                    .iter_mut()
                    .filter(|account| account.api_provider.is_none())
                {
                    let config_dir = if account.is_default {
                        default_dir.clone()
                    } else {
                        store.account_dir(&ToolId::Codex, &account.id)
                    };
                    account.account_email = codex_account_email(&config_dir);
                }
            }
            for account in accounts.iter_mut() {
                account.launcher_paths = account
                    .launcher_command
                    .as_ref()
                    .map(|name| launcher_paths(name));
                account.quota_status = Some(if account.api_provider.is_some() {
                    "api".to_string()
                } else if account.quota.is_none() {
                    "missing".to_string()
                } else if account
                    .quota
                    .as_ref()
                    .and_then(|quota| quota.error.as_ref())
                    .is_some()
                {
                    "error".to_string()
                } else {
                    "ready".to_string()
                });
            }
            let active_account_id = active_account_id_for(store, &tool_id, &accounts);
            ToolStatus {
                id: tool_id.clone(),
                name: tool_id.display_name().to_string(),
                platform: current_platform().to_string(),
                installed: is_installed_resolved(data, &tool_id),
                active_account_id,
                accounts,
            }
        })
        .collect();

    AppSnapshot {
        tools,
        disclaimer_accepted: data.disclaimer_accepted,
        auto_switch: data.auto_switch,
        auto_switch_threshold: data.auto_switch_threshold,
        auto_switch_settings: data.auto_switch_settings.clone(),
        auto_prime: data.auto_prime.clone(),
        auto_ping_enabled: data.auto_ping_enabled,
        launch_with_windows: data.launch_with_windows,
        auto_ping: data.auto_ping.clone(),
        resume_jobs: data.resume_jobs.clone(),
        tool_setups: data.tool_setups.clone(),
        api_gateway: ApiGatewaySnapshot {
            config: redacted_api_gateway_config(data),
            status,
        },
    }
}

fn redacted_api_gateway_config(data: &StoredState) -> ApiGatewayConfig {
    use crate::models::{ApiGatewayAccount, ApiPoolAccountState};
    let mut redacted = data.api_gateway.clone();
    for key in &mut redacted.keys {
        key.secret = None;
    }
    // Surface every eligible subscription account with a live participation state. Accounts with
    // no stored entry default to enabled; the UI renders this list for on/off toggles + status.
    let mut accounts = Vec::new();
    for account in data
        .accounts
        .iter()
        .filter(|account| visible_cli_tools().contains(&account.tool_id))
        .filter(|account| account.api_provider.is_none())
    {
        let stored = data
            .api_gateway
            .accounts
            .iter()
            .find(|entry| entry.tool_id == account.tool_id && entry.account_id == account.id);
        let enabled = stored.is_none_or(|entry| entry.enabled);
        let mut state = ApiPoolAccountState::Available;
        let mut cooldown_until = None;
        let mut error = None;
        if !enabled {
            state = ApiPoolAccountState::Excluded;
        } else if matches!(account.state, AccountState::NeedsLogin) {
            state = ApiPoolAccountState::Errored;
            error = Some("Account needs login".to_string());
        } else if max_percent_used(account) >= data.api_gateway.quota_threshold {
            state = ApiPoolAccountState::Exhausted;
        } else {
            let cooling = stored
                .and_then(|entry| entry.cooldown_until.as_deref())
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .is_some_and(|until| until > chrono::Utc::now());
            if cooling {
                state = ApiPoolAccountState::CoolingDown;
                cooldown_until = stored.and_then(|entry| entry.cooldown_until.clone());
            } else if matches!(
                stored.map(|entry| &entry.state),
                Some(ApiPoolAccountState::Errored)
            ) {
                state = ApiPoolAccountState::Errored;
                error = stored.and_then(|entry| entry.error.clone());
            }
        }
        accounts.push(ApiGatewayAccount {
            tool_id: account.tool_id.clone(),
            account_id: account.id.clone(),
            enabled,
            state,
            cooldown_until,
            error,
        });
    }
    redacted.accounts = accounts;
    redacted
}

fn autodetect_missing_tool_setups(store: &Store, data: &mut StoredState) {
    for tool_id in visible_cli_tools() {
        if data.tool_setups.contains_key(tool_id.as_str()) {
            continue;
        }
        let report = crate::detection::detect_tool_setup(&tool_id, store);
        if let Some(setup) = report.resolution.setup {
            data.tool_setups.insert(tool_id.as_str().to_string(), setup);
        }
    }
}

fn visible_tools() -> Vec<ToolId> {
    if cfg!(windows) {
        // Claude Code and Codex both work on Windows via per-profile config dirs. Antigravity
        // stays macOS-only — it switches accounts by rewriting the IDE's `state.vscdb`.
        vec![ToolId::Claude, ToolId::Codex]
    } else {
        vec![ToolId::Claude, ToolId::Codex, ToolId::Antigravity]
    }
}

fn visible_cli_tools() -> Vec<ToolId> {
    visible_tools()
        .into_iter()
        .filter(|tool_id| !matches!(tool_id, ToolId::Antigravity))
        .collect()
}

fn is_installed_resolved(data: &StoredState, tool_id: &ToolId) -> bool {
    data.tool_setups
        .get(tool_id.as_str())
        .and_then(|setup| setup.binary_path.as_ref())
        .is_some_and(|path| path.exists())
        || is_installed(tool_id)
}

fn vault_input_from_credential(credential: &VaultCredential) -> VaultCredentialInput {
    VaultCredentialInput {
        tool_id: credential.tool_id.clone(),
        account_id: credential.account_id.clone(),
        username: credential.username.clone(),
        password: credential.password.clone(),
        totp_secret: credential.totp_secret.clone(),
        source: credential.source.clone(),
        notes: credential.notes.clone(),
    }
}

fn restore_active_profile_file(path: &std::path::Path, contents: Option<&[u8]>) -> Result<()> {
    match contents {
        Some(bytes) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, bytes)?;
        }
        None => {
            if path.exists() {
                std::fs::remove_file(path)?;
            }
        }
    }
    Ok(())
}

/// The account the PLAIN COMMAND is using. For Claude/Codex this is the real source of truth:
/// the active file (`active/<tool>.profile`) that the shell hook reads to export the config dir —
/// NOT inferred from `state==Active` (an exhausted account is still the one the plain command uses,
/// but its state is Exhausted, so inferring from state would be wrong). Empty/missing file = machine Default.
/// Antigravity is copy-swap (no active file), so it still follows `state==Active`.
fn active_account_id_for(store: &Store, tool_id: &ToolId, accounts: &[Account]) -> Option<String> {
    if matches!(tool_id, ToolId::Antigravity) {
        // The account in use = the account whose token matches the IDE's current token in state.vscdb.
        let current = antigravity_current_token()?;
        return accounts
            .iter()
            .find(|account| {
                antigravity_saved_token(store, &account.id).as_deref() == Some(current.as_str())
            })
            .map(|account| account.id.clone());
    }

    let target = std::fs::read_to_string(store.active_profile_path(tool_id))
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());

    match target {
        // The active file points to a specific account's profile dir → that account is in use.
        Some(target) => accounts
            .iter()
            .find(|account| {
                !account.is_default
                    && store.account_dir(tool_id, &account.id).to_string_lossy() == target
            })
            .map(|account| account.id.clone()),
        // No active file → the plain command uses the machine Default.
        None => accounts
            .iter()
            .find(|account| account.is_default)
            .map(|account| account.id.clone()),
    }
}

/// The account's highest % used (5h or weekly); 0 if there's no data.
fn max_percent_used(account: &Account) -> f64 {
    account.quota.as_ref().map_or(0.0, |quota| {
        if quota.error.is_some() {
            return 0.0;
        }
        [quota.five_hour.percent_used, quota.weekly.percent_used]
            .into_iter()
            .flatten()
            .fold(0.0_f64, f64::max)
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SwitchRuntimePlan {
    sync_codex_ide_credentials: bool,
    restart_codex_runtime: bool,
}

fn switch_runtime_plan(tool_id: &ToolId, is_api: bool) -> SwitchRuntimePlan {
    let refresh_codex = matches!(tool_id, ToolId::Codex) && !is_api;
    SwitchRuntimePlan {
        sync_codex_ide_credentials: refresh_codex,
        // The automatic VS Code "Developer: Reload Window" below restarts its app-server
        // gracefully. Killing it first races the Codex webview while its stdin is still in use.
        restart_codex_runtime: false,
    }
}

fn auto_switch_target<'a>(
    accounts: &'a [Account],
    tool_id: &ToolId,
    threshold: f64,
    active_account_id: Option<&str>,
) -> Option<&'a Account> {
    let active_id = active_account_id?;
    let active = accounts
        .iter()
        .find(|account| account.tool_id == *tool_id && account.id == active_id)?;
    if max_percent_used(active) < threshold {
        return None;
    }
    best_replacement(accounts, tool_id, threshold, Some(active_id))
}

fn rollout_files(sessions_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut stack = vec![sessions_dir.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let is_rollout = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"));
            if !is_rollout {
                continue;
            }
            files.push(path);
        }
    }
    files.sort();
    files
}

/// Most bytes taken from one rollout in a single watcher pass. Rollouts reach hundreds of
/// megabytes; whatever is left over is picked up by the next poll.
const MAX_TAIL_READ_BYTES: usize = 4 * 1024 * 1024;

/// Read the whole lines that follow `offset`, returning the text and how many bytes it covered.
///
/// The watcher polls while Codex is still writing, so the tail routinely ends in the middle of a
/// line — and therefore in the middle of a multi-byte character. Decoding is lossy and the result
/// is cut at the last newline so a split character can't fail the read, and the caller advances
/// its cursor by the returned count rather than by the file length, so nothing is skipped.
///
/// An offset that starts mid-line (cursors are seeded from the file length) discards that partial
/// first line: judging a fragment as if it were a complete record is how stale text gets misread.
fn read_file_tail_from(path: &std::path::Path, offset: u64) -> Result<(String, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let starts_mid_line = if offset > 0 {
        file.seek(SeekFrom::Start(offset - 1))?;
        let mut previous = [0u8; 1];
        file.read_exact(&mut previous)
            .map(|()| previous[0] != b'\n')
            .unwrap_or(false)
    } else {
        false
    };

    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = Vec::new();
    (&mut file)
        .take(MAX_TAIL_READ_BYTES as u64)
        .read_to_end(&mut buffer)?;

    let mut skipped = 0usize;
    if starts_mid_line {
        match buffer.iter().position(|byte| *byte == b'\n') {
            Some(index) => skipped = index + 1,
            // No end to the partial line yet. If it hasn't filled the window, wait for the rest;
            // if it HAS (a single line bigger than the window), skip it so the watcher advances
            // instead of re-reading the same bytes forever — symmetric with the tail branch below.
            None if buffer.len() < MAX_TAIL_READ_BYTES => return Ok((String::new(), 0)),
            None => return Ok((String::new(), buffer.len() as u64)),
        }
    }

    let body = &buffer[skipped..];
    let consumed = match body.iter().rposition(|byte| *byte == b'\n') {
        Some(index) => index + 1,
        // No line break yet: leave it for the next poll, unless the line is bigger than the whole
        // window, in which case skip it so the watcher can't stall on the same bytes forever.
        None if body.len() < MAX_TAIL_READ_BYTES => 0,
        None => body.len(),
    };
    let text = String::from_utf8_lossy(&body[..consumed]).into_owned();
    Ok((text, (skipped + consumed) as u64))
}

fn codex_quota_error_line(line: &str) -> bool {
    let text = if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
        let is_event = value.get("type").and_then(serde_json::Value::as_str) == Some("event_msg");
        let Some(payload) = value.get("payload") else {
            return false;
        };
        let event_type = payload
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let has_explicit_error = payload.get("error").is_some()
            || payload.get("last_error").is_some()
            || matches!(event_type, "error" | "turn_aborted");
        if !is_event || !has_explicit_error {
            return false;
        }
        payload.to_string()
    } else {
        line.to_string()
    };
    let lower = text.to_ascii_lowercase();
    (lower.contains("quota") || lower.contains("rate limit") || lower.contains("usage limit"))
        && (lower.contains("exceeded")
            || lower.contains("reached")
            || lower.contains("exhausted")
            || lower.contains("429")
            || lower.contains("limit"))
}

fn quota_requires_reauth(tool_id: &ToolId, quota: &QuotaInfo) -> bool {
    if *tool_id != ToolId::Codex {
        return false;
    }
    if quota_error_is_transient(quota) {
        return false;
    }
    let Some(error) = quota.error.as_deref() else {
        return false;
    };
    let lower = error.to_ascii_lowercase();
    lower.contains("401")
        || lower.contains("couldn't get codex's access_token")
        || lower.contains("access_token")
        || lower.contains("codex authentication missing")
        || lower.contains("codex authentication invalid")
        || lower.contains("codex refresh_token missing")
        || lower.contains("codex token refresh rejected")
}

const QUOTA_REFRESH_CONCURRENCY: usize = 3;
const QUOTA_READ_MAX_ATTEMPTS: usize = 2;

fn quota_refresh_batch_ranges(total: usize) -> Vec<std::ops::Range<usize>> {
    (0..total)
        .step_by(QUOTA_REFRESH_CONCURRENCY)
        .map(|start| start..(start + QUOTA_REFRESH_CONCURRENCY).min(total))
        .collect()
}

fn quota_error_is_transient(quota: &QuotaInfo) -> bool {
    let Some(error) = quota.error.as_deref() else {
        return false;
    };
    let lower = error.to_ascii_lowercase();
    lower.contains("network error")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("connection reset")
        || lower.contains("connection refused")
        || lower.contains("dns error")
        || lower.contains("codex token refresh response invalid")
        || lower.contains("codex token refresh failed: http 500")
        || lower.contains("codex token refresh failed: http 502")
        || lower.contains("codex token refresh failed: http 503")
        || lower.contains("codex token refresh failed: http 504")
}

fn keep_last_quota_on_transient_failure(
    previous: Option<&QuotaInfo>,
    refreshed: QuotaInfo,
) -> QuotaInfo {
    if !quota_error_is_transient(&refreshed) {
        return refreshed;
    }
    let Some(previous) = previous else {
        return refreshed;
    };
    let has_known_quota = previous.five_hour.percent_used.is_some()
        || previous.weekly.percent_used.is_some()
        || previous
            .models
            .as_ref()
            .is_some_and(|models| models.iter().any(|model| model.percent_used.is_some()));
    if !has_known_quota {
        return refreshed;
    }

    let mut stale = previous.clone();
    stale.error = Some("Network issue; showing last known quota".to_string());
    stale
}

fn retry_quota_read<Read, Wait>(mut read: Read, mut wait: Wait) -> QuotaInfo
where
    Read: FnMut() -> QuotaInfo,
    Wait: FnMut(std::time::Duration),
{
    let mut quota = read();
    for attempt in 1..QUOTA_READ_MAX_ATTEMPTS {
        if !quota_error_is_transient(&quota) {
            break;
        }
        wait(std::time::Duration::from_millis(750 * attempt as u64));
        quota = read();
    }
    quota
}

fn read_quota_resilient(tool_id: &ToolId, config_dir: &std::path::Path) -> QuotaInfo {
    retry_quota_read(|| read_quota(tool_id, config_dir), std::thread::sleep)
}

/// The best replacement account: same tool, not `excluded`, not yet at the threshold,
/// logged in (Idle/Active), with the most quota left. The Default account is eligible.
fn best_replacement<'a>(
    accounts: &'a [Account],
    tool_id: &ToolId,
    threshold: f64,
    excluded_account_id: Option<&str>,
) -> Option<&'a Account> {
    accounts
        .iter()
        .filter(|account| {
            &account.tool_id == tool_id
                && Some(account.id.as_str()) != excluded_account_id
                && !matches!(account.state, AccountState::NeedsLogin)
                && max_percent_used(account) < threshold
                // Skip accounts with no quota data yet (unsure whether any is left).
                && account.quota.as_ref().is_some_and(|q| q.error.is_none())
        })
        .min_by(|left, right| max_percent_used(left).total_cmp(&max_percent_used(right)))
}

fn validate_name(
    tool_id: &ToolId,
    account_id: Option<&str>,
    name: &str,
    state: &ManagedState,
) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.chars().count() > 20 {
        anyhow::bail!("Account name is limited to 20 characters");
    }
    if trimmed.is_empty() {
        return Ok(());
    }
    if [
        virtual_api_name(&ToolId::Claude),
        virtual_api_name(&ToolId::Codex),
    ]
    .contains(&trimmed)
    {
        anyhow::bail!("This account name is reserved for the local API gateway");
    }
    let data = state
        .data
        .lock()
        .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
    let duplicate = data.accounts.iter().any(|account| {
        &account.tool_id == tool_id
            && account.name == trimmed
            && Some(account.id.as_str()) != account_id
    });
    if duplicate {
        anyhow::bail!("Account name must be unique within the same tool");
    }
    Ok(())
}

fn bulk_prefix(raw: Option<&str>, fallback: &str, label: &str) -> Result<String> {
    let value = raw.unwrap_or(fallback).trim().to_ascii_lowercase();
    if value.is_empty() {
        anyhow::bail!("{label} cannot be empty");
    }
    if value.chars().count() > 16 {
        anyhow::bail!("{label} is limited to 16 characters");
    }
    if !value.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '-' | '_')
    }) {
        anyhow::bail!("{label} may only contain lowercase a-z, digits, - or _");
    }
    Ok(value)
}

/// Allocate one deterministic display name and command for a bulk import.
///
/// A single sequence is used for both labels so the resulting account list
/// stays naturally ordered (`acc2` before `acc10`). Existing names/commands
/// and system binaries are skipped without overwriting them.
fn next_bulk_account_labels(
    data: &Mutex<StoredState>,
    name_prefix: &str,
    launcher_prefix: &str,
    next_label: &mut usize,
    reserved_names: &mut HashSet<String>,
    reserved_launchers: &mut HashSet<String>,
) -> Result<(String, String)> {
    let data = data
        .lock()
        .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
    loop {
        let suffix = next_label.to_string();
        *next_label = next_label.saturating_add(1);
        let name = format!("{name_prefix}{suffix}");
        if name.chars().count() > 20 {
            anyhow::bail!("Generated account name is too long; use a shorter name prefix");
        }
        if reserved_names.contains(&name)
            || data
                .accounts
                .iter()
                .any(|account| account.tool_id == ToolId::Codex && account.name == name)
        {
            continue;
        }

        let full_launcher =
            full_launcher_name(&ToolId::Codex, &format!("{launcher_prefix}{suffix}"))?;
        if reserved_launchers.contains(&full_launcher)
            || data.accounts.iter().any(|account| {
                account.tool_id == ToolId::Codex
                    && account.launcher_command.as_deref() == Some(full_launcher.as_str())
            })
            || launcher_name_collides_with_system(&full_launcher)
        {
            continue;
        }

        reserved_names.insert(name.clone());
        reserved_launchers.insert(full_launcher.clone());
        return Ok((name, full_launcher));
    }
}

fn virtual_api_name(tool_id: &ToolId) -> &'static str {
    match tool_id {
        ToolId::Claude => "claude-api",
        ToolId::Codex => "codex-api",
        ToolId::Antigravity => "antigravity-api",
    }
}

fn is_virtual_api_account(account: &Account) -> bool {
    account.fingerprint == "api-local"
}

fn generate_api_key() -> String {
    use rand::RngCore;
    let mut bytes = [0_u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    let encoded = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sk-{encoded}")
}

fn mask_key(secret: &str) -> String {
    let suffix = secret
        .chars()
        .rev()
        .take(6)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("sk-...{suffix}")
}

fn normalized_or_default_name(
    tool_id: &ToolId,
    name: &str,
    state: &ManagedState,
) -> Result<String> {
    let trimmed = name.trim();
    if !trimmed.is_empty() {
        return Ok(trimmed.to_string());
    }

    let data = state
        .data
        .lock()
        .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
    let base = tool_id.display_name().replace(" Code", "");
    for index in 1.. {
        let candidate = format!("{base} {index}");
        let exists = data
            .accounts
            .iter()
            .any(|account| &account.tool_id == tool_id && account.name == candidate);
        if !exists {
            return Ok(candidate.chars().take(20).collect());
        }
    }
    unreachable!()
}

/// Decide an account's state after a quota refresh.
///
/// `unread_this_cycle` = the refresh couldn't read fresh quota (a transient error, so the account
/// is showing its last-known numbers). With no fresh truth we keep the current state — inferring
/// recovery would flip an exhausted account to Idle, suppress auto-switch, and refire the
/// "out of quota" notification on the next clean poll.
fn state_after_refresh(
    current: AccountState,
    is_default: bool,
    requires_reauth: bool,
    unread_this_cycle: bool,
    exhausted_now: bool,
    was_exhausted: bool,
) -> AccountState {
    if requires_reauth && !is_default {
        AccountState::NeedsLogin
    } else if unread_this_cycle {
        current
    } else if exhausted_now {
        AccountState::Exhausted
    } else if was_exhausted {
        AccountState::Idle
    } else {
        current
    }
}

fn is_exhausted(account: &Account) -> bool {
    account.quota.as_ref().is_some_and(|quota| {
        quota.error.is_none()
            && [quota.five_hour.percent_used, quota.weekly.percent_used]
                .into_iter()
                .flatten()
                .any(|percent| percent >= 100.0)
    })
}

fn notify_exhausted(app: &AppHandle, account: &Account) {
    let reset = account
        .quota
        .as_ref()
        .and_then(|quota| {
            quota
                .five_hour
                .reset_at
                .clone()
                .or_else(|| quota.weekly.reset_at.clone())
        })
        // Show a readable local time with the UTC-offset label, not the raw ISO/UTC string.
        .map(|iso| local_time_label_from_iso(&iso))
        .unwrap_or_else(|| "unknown".to_string());
    let _ = app
        .notification()
        .builder()
        .title("Out of quota")
        .body(format!(
            "Account {} is out of quota, resets at {}",
            account.name, reset
        ))
        .show();
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn is_same_account_retryable(kind: RecoveryErrorKind) -> bool {
    matches!(
        kind,
        RecoveryErrorKind::ServerOverloaded
            | RecoveryErrorKind::ResponseStreamDisconnected
            | RecoveryErrorKind::ResponseStreamConnectionFailed
            | RecoveryErrorKind::HttpConnectionFailed
    )
}

fn recovery_retry_delay(attempt: u32) -> std::time::Duration {
    let exponent = attempt.saturating_sub(2).min(3);
    std::time::Duration::from_secs(1_u64 << exponent)
}

fn recovery_attention_message(kind: RecoveryErrorKind) -> String {
    match kind {
        RecoveryErrorKind::ServerOverloaded => {
            "The selected model is temporarily at capacity. Retrying without switching accounts…".into()
        }
        RecoveryErrorKind::Unauthorized => {
            "Resume could not be verified: Codex authentication expired.".into()
        }
        RecoveryErrorKind::ContextWindowExceeded => {
            "Resume could not be verified: the thread context is exhausted; compact the same thread before retrying.".into()
        }
        RecoveryErrorKind::SandboxError => {
            "Resume could not be verified: the original sandbox or approval policy was rejected.".into()
        }
        RecoveryErrorKind::UserCancelled => {
            "Resume could not be verified: the interrupted Codex turn was cancelled by the user.".into()
        }
        RecoveryErrorKind::ResponseStreamDisconnected
        | RecoveryErrorKind::ResponseStreamConnectionFailed
        | RecoveryErrorKind::HttpConnectionFailed => {
            "Resume could not be verified: the Codex response stream ended before recovery could be acknowledged.".into()
        }
        RecoveryErrorKind::Unknown => {
            "Resume could not be verified: no structured Codex terminal failure was available.".into()
        }
        RecoveryErrorKind::UsageLimitExceeded => {
            "Resume could not be verified: the usage-limit recovery path was not accepted.".into()
        }
    }
}

/// Render an ISO 8601 instant as a human-friendly LOCAL time with an explicit UTC-offset label,
/// e.g. `16:32 (UTC+7)`. For user-facing notifications — a raw `…T11:20:00+00:00` reads as "11:20"
/// and gets mistaken for a local morning time when it's actually 18:20 in a +7 zone. Falls back to
/// the raw string if unparseable.
fn local_time_label_from_iso(iso: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(t) => {
            let local = t.with_timezone(&chrono::Local);
            // Offset in whole hours (e.g. +7); show minutes too only when the zone isn't on the hour.
            let secs = local.offset().local_minus_utc();
            let sign = if secs < 0 { '-' } else { '+' };
            let abs = secs.abs();
            let (h, m) = (abs / 3600, (abs % 3600) / 60);
            let off = if m == 0 {
                format!("UTC{sign}{h}")
            } else {
                format!("UTC{sign}{h}:{m:02}")
            };
            format!("{} ({off})", local.format("%H:%M"))
        }
        Err(_) => iso.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]

    use super::*;

    #[test]
    fn bulk_prefixes_are_normalized_and_reject_unsafe_values() {
        assert_eq!(
            bulk_prefix(Some("  Batch_"), "acc", "prefix").unwrap(),
            "batch_"
        );
        let error = match bulk_prefix(Some("batch name"), "acc", "prefix") {
            Ok(_) => panic!("spaces must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("lowercase"));
    }

    #[test]
    fn bulk_labels_skip_existing_names_and_commands() {
        let mut data = StoredState::default();
        data.accounts.push(Account {
            id: "existing".into(),
            tool_id: ToolId::Codex,
            name: "acc1".into(),
            state: AccountState::Idle,
            fingerprint: "profile:existing".into(),
            created_at: "now".into(),
            updated_at: "now".into(),
            last_used_at: None,
            launcher_command: Some("codex-bulk-1".into()),
            launcher_paths: None,
            is_default: false,
            avatar_url: None,
            account_email: None,
            api_provider: None,
            quota_status: None,
            codex_login_method: CodexLoginMethod::BrowserProfile,
            quota: None,
        });
        let data = Mutex::new(data);
        let mut next = 1;
        let mut names = HashSet::new();
        let mut launchers = HashSet::new();

        let (name, launcher) =
            next_bulk_account_labels(&data, "acc", "bulk-", &mut next, &mut names, &mut launchers)
                .expect("next safe bulk labels");
        assert_eq!(name, "acc2");
        assert_eq!(launcher, "codex-bulk-2");
    }

    fn test_resume_job(id: &str, status: ResumeJobStatus) -> ResumeJob {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "sessionId": "session",
            "eventKey": id,
            "cwd": "C:\\workspace",
            "fromAccountId": "acc1",
            "checkpointPath": "",
            "status": serde_json::to_value(status).unwrap(),
            "createdAt": "2026-07-01T00:00:00Z",
            "updatedAt": "2026-07-01T00:00:00Z"
        }))
        .unwrap()
    }

    #[test]
    fn startup_migration_retires_legacy_recovery_noise_and_bounds_history() {
        let mut data = StoredState::default();
        data.schema_version = 3;
        data.resume_jobs = (0..30)
            .map(|index| {
                let status = if index % 2 == 0 {
                    ResumeJobStatus::Resumed
                } else {
                    ResumeJobStatus::Failed
                };
                test_resume_job(&format!("job-{index}"), status)
            })
            .collect();
        data.resume_jobs
            .push(test_resume_job("active", ResumeJobStatus::Monitoring));

        migrate_resume_jobs(&mut data);

        assert_eq!(data.schema_version, 4);
        assert_eq!(
            data.resume_jobs
                .iter()
                .filter(|job| job.status == ResumeJobStatus::Monitoring)
                .count(),
            1
        );
        assert!(data.resume_jobs.len() <= 21);
        assert!(data.resume_jobs.iter().all(|job| {
            !matches!(
                job.status,
                ResumeJobStatus::Resumed | ResumeJobStatus::Failed
            )
        }));
    }

    #[test]
    fn codex_subscription_switch_reloads_the_ide_without_killing_its_app_server() {
        let plan = switch_runtime_plan(&ToolId::Codex, false);

        assert!(plan.sync_codex_ide_credentials);
        assert!(
            !plan.restart_codex_runtime,
            "Developer: Reload Window owns the graceful app-server restart"
        );
    }

    #[test]
    fn codex_api_switch_does_not_replace_subscription_credentials() {
        let plan = switch_runtime_plan(&ToolId::Codex, true);

        assert!(!plan.sync_codex_ide_credentials);
        assert!(!plan.restart_codex_runtime);
    }

    #[test]
    fn account_names_sort_by_numeric_segments() {
        let mut names = vec![
            "acc10".to_string(),
            "acc2".to_string(),
            "acc11".to_string(),
            "acc1".to_string(),
        ];

        names.sort_by(|left, right| natural_name_cmp(left, right));

        assert_eq!(names, ["acc1", "acc2", "acc10", "acc11"]);
    }

    #[test]
    fn exhausted_active_account_still_triggers_auto_switch() {
        let active = test_account("acc-1", AccountState::Exhausted, 100.0);
        let replacement = test_account("acc-2", AccountState::Idle, 20.0);
        let accounts = vec![active, replacement];

        let target = auto_switch_target(&accounts, &ToolId::Codex, 100.0, Some("acc-1"));

        assert_eq!(target.map(|account| account.id.as_str()), Some("acc-2"));
    }

    #[test]
    fn codex_handoff_without_a_rollout_allows_regular_auto_switch_fallback() {
        let root =
            std::env::temp_dir().join(format!("aisw-codex-fallback-{}", uuid::Uuid::new_v4()));
        let store = Store::for_test(root.clone()).unwrap();
        let exhausted = test_account("active", AccountState::Exhausted, 100.0);
        let replacement = test_account("healthy", AccountState::Idle, 20.0);
        let mut data = StoredState::default();
        data.accounts = vec![exhausted, replacement];

        let profile_dir = store.account_dir(&ToolId::Codex, "active");
        std::fs::create_dir_all(&profile_dir).unwrap();
        write_active_profile(&ToolId::Codex, &store, "active").unwrap();
        let vault = VaultService::new(&store);
        let state = ManagedState {
            store,
            data: Mutex::new(data),
            vault,
            api_server: Mutex::new(crate::api_gateway::ApiServerHandle::stopped(
                &ApiGatewayConfig::default(),
            )),
            codex_quota_watch: Mutex::new(BTreeMap::new()),
            codex_active_rollout: Mutex::new(None),
            codex_handoff_running: std::sync::atomic::AtomicBool::new(false),
        };

        assert!(
            !state.try_seamless_codex_handoff(95.0, false, None).unwrap(),
            "without an exact rollout, the caller must continue to the normal Codex switch"
        );

        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn detects_codex_quota_error_lines() {
        assert!(codex_quota_error_line(
            r#"{"type":"event_msg","payload":{"type":"error","message":"rate limit exceeded"}}"#
        ));
        assert!(codex_quota_error_line(
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","message":"usage limit reached for this account"}}"#
        ));
        assert!(codex_quota_error_line("HTTP 429 quota exhausted"));
        assert!(!codex_quota_error_line("refresh quota button clicked"));
        assert!(!codex_quota_error_line("normal assistant message"));
    }

    #[test]
    fn ignores_quota_words_inside_new_chat_rollout_content() {
        assert!(!codex_quota_error_line(
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"text":"Create a new chat that explains rate limit exceeded handling"}]}}"#
        ));
        assert!(!codex_quota_error_line(
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","output":"usage limit reached is mentioned in this document"}}"#
        ));
        assert!(!codex_quota_error_line(
            r#"{"type":"event_msg","payload":{"type":"task_complete","message":"normal task complete; quota limit was discussed"}}"#
        ));
    }

    #[test]
    fn codex_401_requires_reauthentication_but_unknown_quota_does_not() {
        let mut quota = QuotaInfo::with_message("Couldn't read quota: HTTP 401");
        assert!(quota_requires_reauth(&ToolId::Codex, &quota));
        quota.error = None;
        assert!(!quota_requires_reauth(&ToolId::Codex, &quota));
        assert!(!quota_requires_reauth(
            &ToolId::Claude,
            &QuotaInfo::with_message("HTTP 401")
        ));
    }

    #[test]
    fn transient_quota_timeout_retries_then_returns_success() {
        let mut attempts = 0;
        let mut waits = 0;
        let quota = retry_quota_read(
            || {
                attempts += 1;
                if attempts == 1 {
                    QuotaInfo::with_message(
                        "Couldn't read quota: network error: operation timed out",
                    )
                } else {
                    test_account("success", AccountState::Idle, 10.0)
                        .quota
                        .expect("test quota")
                }
            },
            |_| waits += 1,
        );

        assert_eq!(attempts, 2);
        assert_eq!(waits, 1);
        assert!(quota.error.is_none());
    }

    #[test]
    fn quota_401_is_not_retried() {
        let mut attempts = 0;
        let mut waits = 0;
        let quota = retry_quota_read(
            || {
                attempts += 1;
                QuotaInfo::with_message("Couldn't read quota: HTTP 401")
            },
            |_| waits += 1,
        );

        assert_eq!(attempts, 1);
        assert_eq!(waits, 0);
        assert!(quota_requires_reauth(&ToolId::Codex, &quota));
    }

    #[test]
    fn transient_refresh_failure_keeps_last_known_quota() {
        let mut previous = QuotaInfo::with_message("unused");
        previous.error = None;
        previous.weekly.percent_used = Some(42.0);
        previous.plan = Some("Plus".to_string());
        let failure =
            QuotaInfo::with_message("Couldn't read quota: network error: operation timed out");

        let merged = keep_last_quota_on_transient_failure(Some(&previous), failure);

        assert_eq!(merged.weekly.percent_used, Some(42.0));
        assert_eq!(merged.plan.as_deref(), Some("Plus"));
        assert_eq!(merged.updated_at, previous.updated_at);
        assert_eq!(
            merged.error.as_deref(),
            Some("Network issue; showing last known quota")
        );
    }

    #[test]
    fn auth_failure_replaces_last_known_quota() {
        let mut previous = QuotaInfo::with_message("unused");
        previous.error = None;
        previous.weekly.percent_used = Some(42.0);
        let failure = QuotaInfo::with_message("Couldn't read quota: HTTP 401");

        let merged = keep_last_quota_on_transient_failure(Some(&previous), failure);

        assert_eq!(merged.weekly.percent_used, None);
        assert_eq!(
            merged.error.as_deref(),
            Some("Couldn't read quota: HTTP 401")
        );
    }

    #[test]
    fn token_refresh_network_timeout_retries_without_forcing_reauth() {
        let quota = QuotaInfo::with_message(
            "Couldn't read quota: couldn't get Codex's access_token: network error refreshing Codex token: operation timed out",
        );

        assert!(quota_error_is_transient(&quota));
        assert!(!quota_requires_reauth(&ToolId::Codex, &quota));
    }

    #[test]
    fn rejected_refresh_token_requires_reauthentication() {
        let quota =
            QuotaInfo::with_message("Couldn't read quota: Codex token refresh rejected: HTTP 400");

        assert!(!quota_error_is_transient(&quota));
        assert!(quota_requires_reauth(&ToolId::Codex, &quota));
    }

    #[test]
    fn quota_refresh_batches_limit_parallel_requests() {
        assert_eq!(
            quota_refresh_batch_ranges(0),
            Vec::<std::ops::Range<usize>>::new()
        );
        assert_eq!(quota_refresh_batch_ranges(7), vec![0..3, 3..6, 6..7]);
    }

    #[test]
    fn startup_migration_marks_stale_codex_401_as_needs_login() {
        let mut account = test_account("acc-1", AccountState::Idle, 10.0);
        account.quota = Some(QuotaInfo::with_message("Couldn't read quota: HTTP 401"));
        let mut accounts = vec![account];

        migrate_codex_reauth_states(&mut accounts);

        assert_eq!(accounts[0].state, AccountState::NeedsLogin);
    }

    #[test]
    fn startup_migration_does_not_take_over_machine_default_auth() {
        let mut account = test_account("default-codex", AccountState::Idle, 10.0);
        account.is_default = true;
        account.quota = Some(QuotaInfo::with_message("Couldn't read quota: HTTP 401"));
        let mut accounts = vec![account];

        migrate_codex_reauth_states(&mut accounts);

        assert_eq!(accounts[0].state, AccountState::Idle);
    }

    #[test]
    fn startup_migration_promotes_k12_import_to_browser_subscription_login() {
        let root =
            std::env::temp_dir().join(format!("aisw-k12-migration-{}", uuid::Uuid::new_v4()));
        let store = Store::for_test(root.clone()).unwrap();
        let profile = store.account_dir(&ToolId::Codex, "k12");
        std::fs::create_dir_all(&profile).unwrap();
        crate::tools::mark_codex_imported_session(&profile).unwrap();

        let mut account = test_account("k12", AccountState::NeedsLogin, 100.0);
        account.launcher_command = Some("codex-k12".to_string());
        let mut accounts = vec![account];

        migrate_codex_imported_login_methods(&store, &mut accounts);

        assert_eq!(
            accounts[0].codex_login_method,
            CodexLoginMethod::BrowserProfile
        );
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn retired_auto_ping_is_disabled_and_cleared_on_startup() {
        let mut data = StoredState::default();
        data.auto_ping_enabled = true;
        data.auto_ping.insert(
            "acc-1".to_string(),
            crate::models::AutoPingSetting {
                enabled: true,
                ..crate::models::AutoPingSetting::default()
            },
        );

        retire_auto_ping_settings(&mut data);

        assert!(!data.auto_ping_enabled);
        assert!(data.auto_ping.is_empty());
    }

    /// A transient quota-read failure (network blip) shows the last-known numbers with an error
    /// flag. An exhausted account must stay Exhausted through that — not flip to Idle, which would
    /// suppress auto-switch and refire the notification on the next clean poll.
    #[test]
    fn transient_failure_keeps_an_exhausted_account_exhausted() {
        // Retained 100% quota + error (unread this cycle) → keep Exhausted.
        assert_eq!(
            state_after_refresh(AccountState::Exhausted, false, false, true, false, true),
            AccountState::Exhausted
        );
        // A clean read showing recovery (percent < 100, no error) → Idle.
        assert_eq!(
            state_after_refresh(AccountState::Exhausted, false, false, false, false, true),
            AccountState::Idle
        );
        // A clean read still at 100% → Exhausted.
        assert_eq!(
            state_after_refresh(AccountState::Idle, false, false, false, true, false),
            AccountState::Exhausted
        );
        // Reauth needed on a non-default account wins over everything.
        assert_eq!(
            state_after_refresh(AccountState::Active, false, true, false, false, false),
            AccountState::NeedsLogin
        );
        // The machine default never goes to NeedsLogin.
        assert_eq!(
            state_after_refresh(AccountState::Active, true, true, false, false, false),
            AccountState::Active
        );
    }

    /// The watcher polls files Codex is still writing, so the tail regularly stops inside a line
    /// and inside a multi-byte character. That must not fail the read or lose the bytes.
    #[test]
    fn tail_read_stops_at_line_boundaries_and_survives_split_characters() {
        let root = std::env::temp_dir().join(format!("aisw-tail-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("rollout.jsonl");

        let complete = "{\"a\":\"đầy đủ\"}\n";
        let second = "{\"b\":\"chưa xong ề\"}\n";
        // Write the second record only partially, cut inside the multi-byte 'ề'.
        let split_at = second.find('ề').unwrap() + 1;
        let mut bytes = complete.as_bytes().to_vec();
        bytes.extend_from_slice(&second.as_bytes()[..split_at]);
        std::fs::write(&path, &bytes).unwrap();

        let (text, consumed) = read_file_tail_from(&path, 0).unwrap();
        assert_eq!(text, complete, "only the finished line should be returned");
        assert_eq!(
            consumed,
            complete.len() as u64,
            "the cursor must stop at the line break, not at the file length"
        );

        // Codex finishes writing the record: resuming from the committed cursor yields it whole.
        std::fs::write(&path, format!("{complete}{second}")).unwrap();
        let (text, consumed) = read_file_tail_from(&path, complete.len() as u64).unwrap();
        assert_eq!(text, second);
        assert_eq!(consumed as usize, second.len());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A cursor seeded from the file length lands mid-line. That fragment is not a record and
    /// must never be evaluated as one — judging fragments is how stale text reads as a live error.
    #[test]
    fn tail_read_discards_the_fragment_before_a_mid_line_offset() {
        let root = std::env::temp_dir().join(format!("aisw-tail-mid-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("rollout.jsonl");
        let first = "you have hit your usage limit, quota exceeded\n";
        let second = "{\"type\":\"plain\"}\n";
        std::fs::write(&path, format!("{first}{second}")).unwrap();

        // Start inside the first line, as a length-seeded cursor would.
        let (text, consumed) = read_file_tail_from(&path, 10).unwrap();
        assert_eq!(
            text, second,
            "the partial first line must be dropped, not returned"
        );
        assert!(!text.lines().any(codex_quota_error_line));
        assert_eq!(consumed as usize, first.len() - 10 + second.len());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `delete_account` decides whether to clear `active/<tool>.profile` from this helper. An
    /// account that ran out of quota is still the one the bare `codex` command runs as, so the
    /// answer must not depend on the account's state.
    #[test]
    fn active_account_is_read_from_the_active_file_not_the_account_state() {
        let root = std::env::temp_dir().join(format!("aisw-active-{}", uuid::Uuid::new_v4()));
        let store = Store::for_test(root.clone()).unwrap();
        let mut exhausted = test_account("a1", AccountState::Exhausted, 100.0);
        exhausted.is_default = false;
        let mut machine_default = test_account("default", AccountState::Active, 0.0);
        machine_default.is_default = true;
        let accounts = vec![exhausted, machine_default];

        let profile_dir = store.account_dir(&ToolId::Codex, "a1");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let active_path = store.active_profile_path(&ToolId::Codex);
        std::fs::create_dir_all(active_path.parent().unwrap()).unwrap();
        std::fs::write(&active_path, profile_dir.to_string_lossy().as_bytes()).unwrap();

        assert_eq!(
            active_account_id_for(&store, &ToolId::Codex, &accounts).as_deref(),
            Some("a1"),
            "the exhausted account is still the one the plain command uses"
        );

        // With no active file the plain command falls back to the machine default.
        std::fs::remove_file(&active_path).unwrap();
        assert_eq!(
            active_account_id_for(&store, &ToolId::Codex, &accounts).as_deref(),
            Some("default")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    fn test_account(id: &str, state: AccountState, percent_used: f64) -> Account {
        Account {
            id: id.to_string(),
            tool_id: ToolId::Codex,
            name: id.to_string(),
            state,
            fingerprint: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            last_used_at: None,
            quota: Some(QuotaInfo {
                five_hour: crate::models::QuotaWindow {
                    label: "5-hour limit".to_string(),
                    percent_used: Some(percent_used),
                    reset_at: None,
                },
                weekly: crate::models::QuotaWindow {
                    label: "Weekly limit".to_string(),
                    percent_used: Some(percent_used),
                    reset_at: None,
                },
                models: None,
                plan: None,
                updated_at: None,
                error: None,
            }),
            launcher_command: None,
            launcher_paths: None,
            is_default: false,
            avatar_url: None,
            account_email: None,
            api_provider: None,
            quota_status: None,
            codex_login_method: CodexLoginMethod::DeviceCode,
        }
    }

    #[test]
    fn local_time_label_shows_local_hhmm_with_offset() {
        // A UTC instant renders as the LOCAL time plus a "(UTC±N)" label — never the raw UTC HH:MM,
        // which is what made "11:20+00:00" read as a morning time when it was 18:20 in a +7 zone.
        let got = local_time_label_from_iso("2026-06-18T11:20:00+00:00");
        let expected_local = chrono::DateTime::parse_from_rfc3339("2026-06-18T11:20:00+00:00")
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string();
        assert!(
            got.starts_with(&expected_local),
            "expected label to start with local time {expected_local}, got {got}"
        );
        assert!(
            got.contains("(UTC"),
            "expected a UTC-offset label, got {got}"
        );
    }

    #[test]
    fn local_time_label_passes_through_unparseable() {
        assert_eq!(local_time_label_from_iso("nope"), "nope");
    }
}
