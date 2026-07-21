use crate::models::{
    Account, AccountState, AddAccountInput, AddApiAccountInput, ApiGatewayAccount, ApiGatewayCombo,
    ApiGatewayConfig, ApiGatewayKey, ApiGatewayServerState, ApiGatewaySnapshot, ApiProvider,
    ApiUsageReport, AppSnapshot, AutoSwitchSetting, CreateApiGatewayKeyInput,
    CreateApiGatewayKeyResult, CreateVirtualApiAccountInput, DeleteApiGatewayComboInput,
    DeleteApiGatewayKeyInput, DetectionReport, QuotaInfo, RenameAccountInput,
    SaveApiGatewayComboInput, SetApiGatewayAccountInput, SetLauncherInput, SetToolSetupInput,
    StartApiGatewayInput, SwitchAccountInput, ToolId, ToolStatus, UsageReport,
};
use crate::quota::{codex_account_email, read_quota};
use crate::store::{normalize_account_states, Store, StoredState};
use crate::tools::{
    antigravity_capture, antigravity_current_token, antigravity_new_login, antigravity_open_ide,
    antigravity_quit_ide, antigravity_restore, antigravity_saved_profile, antigravity_saved_token,
    clear_active_profile, create_profile_with_default, current_platform, default_config_dir,
    delete_account_files, full_launcher_name, install_shell_hook, is_installed,
    launch_profile_login, launcher_name_collides_with_system, launcher_paths,
    link_shared_config_to, link_shared_sessions_to, refresh_codex_frontends_after_switch,
    remove_launcher, restart_codex_runtime_after_switch, sync_codex_ide_credentials,
    sync_reauthenticated_codex_credentials, write_active_profile, write_api_key_file,
    write_api_launcher, write_claude_proxy_settings, write_codex_proxy_config, write_launcher,
};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;
use uuid::Uuid;

pub struct ManagedState {
    pub store: Store,
    pub data: Mutex<StoredState>,
    pub api_server: Mutex<crate::api_gateway::ApiServerHandle>,
    /// Per-rollout byte offsets for the Codex quota-error watcher.
    pub codex_quota_watch: Mutex<BTreeMap<String, u64>>,
    pub codex_active_rollout: Mutex<Option<std::path::PathBuf>>,
    /// True while a prime batch is running, so the per-minute scheduler tick doesn't start a
    /// second overlapping batch (a single attempt can block for minutes on send retries).
    pub priming: std::sync::atomic::AtomicBool,
}

impl ManagedState {
    pub fn new() -> Result<Self> {
        let store = Store::new()?;
        let mut data = store.load()?;
        migrate_defaults(&mut data.accounts);
        migrate_codex_reauth_states(&mut data.accounts);
        migrate_auto_switch_settings(&mut data);
        retire_auto_ping_settings(&mut data);
        autodetect_missing_tool_setups(&store, &mut data);
        store.save(&data)?;
        let server = crate::api_gateway::ApiServerHandle::stopped(&data.api_gateway);
        let managed = Self {
            store,
            data: Mutex::new(data),
            api_server: Mutex::new(server),
            codex_quota_watch: Mutex::new(BTreeMap::new()),
            codex_active_rollout: Mutex::new(None),
            priming: std::sync::atomic::AtomicBool::new(false),
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
                    if path.is_dir() && !valid_dirs.iter().any(|d| *d == path) {
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
                    account.quota = Some(quota);
                    account.updated_at = timestamp.clone();
                    account.state = if requires_reauth && !account.is_default {
                        AccountState::NeedsLogin
                    } else if is_exhausted(account) {
                        AccountState::Exhausted
                    } else if was_exhausted {
                        AccountState::Idle
                    } else {
                        account.state.clone()
                    };
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
                account.quota = Some(quota);
                account.updated_at = now();
                account.state = if requires_reauth && !account.is_default {
                    AccountState::NeedsLogin
                } else if is_exhausted(account) {
                    AccountState::Exhausted
                } else if was_exhausted {
                    AccountState::Idle
                } else {
                    account.state.clone()
                };
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
                let entry = guard.entry(key).or_insert(len);
                if *entry > len {
                    *entry = 0;
                }
                let start = *entry;
                *entry = len;
                start
            };
            if start == len {
                continue;
            }
            let text = read_file_tail_from(&file, start)?;
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

    pub fn run_auto_ping_cycle(&self, app: Option<&AppHandle>) {
        use std::sync::atomic::Ordering;
        if self
            .priming
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        struct Clear<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Clear<'_> {
            fn drop(&mut self) {
                self.0.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _clear = Clear(&self.priming);

        #[derive(Clone)]
        struct Job {
            id: String,
            name: String,
            config: std::path::PathBuf,
            binary: Option<std::path::PathBuf>,
            event: String,
        }
        let jobs: Vec<Job> = {
            let mut data = match self.data.lock() {
                Ok(data) => data,
                Err(_) => return,
            };
            if !data.auto_ping_enabled {
                return;
            }
            let now = chrono::Utc::now();
            let accounts = data.accounts.clone();
            let mut jobs = Vec::new();
            for account in accounts
                .iter()
                .filter(|a| a.tool_id == ToolId::Codex && !a.is_default && a.api_provider.is_none())
            {
                let setting = data.auto_ping.entry(account.id.clone()).or_default();
                let decision = crate::auto_ping::should_ping(setting, account.quota.as_ref(), now);
                match decision {
                    crate::auto_ping::PingDecision::PingNow { event_key } => {
                        setting.status = Some("active".into());
                        setting.next_at = None;
                        let default_dir = resolved_default_config_dir(&data, &ToolId::Codex);
                        jobs.push(Job {
                            id: account.id.clone(),
                            name: account.name.clone(),
                            config: account_config_dir_with_default(
                                &self.store,
                                account,
                                &default_dir,
                            ),
                            binary: configured_binary_path(&data, &ToolId::Codex),
                            event: event_key,
                        });
                    }
                    crate::auto_ping::PingDecision::WaitUntil(next) => {
                        setting.status = Some("waiting".into());
                        setting.next_at = Some(next);
                    }
                    crate::auto_ping::PingDecision::Paused(reason) => {
                        setting.status = Some(
                            if reason == "quota unavailable" {
                                "failed"
                            } else {
                                "paused"
                            }
                            .into(),
                        );
                    }
                }
            }
            let _ = self.store.save(&data);
            jobs
        };

        for (index, job) in jobs.iter().enumerate() {
            if index > 0 {
                std::thread::sleep(std::time::Duration::from_secs(
                    crate::auto_ping::ACCOUNT_GAP_SECONDS,
                ));
            }
            let before = {
                let mut data = match self.data.lock() {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let value = {
                    let entry = data.auto_ping.entry(job.id.clone()).or_default();
                    entry.last_attempted_reset = Some(job.event.clone());
                    entry.last_at = Some(now());
                    entry.clone()
                };
                let _ = self.store.save(&data);
                value
            };
            let outcome = crate::prime::prime_account(
                &ToolId::Codex,
                &job.config,
                job.binary.as_deref(),
                self.store.root_dir(),
                crate::prime::SEND_MAX_ATTEMPTS,
                std::thread::sleep,
            );
            let mut data = match self.data.lock() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let entry = data.auto_ping.entry(job.id.clone()).or_insert(before);
            match outcome {
                crate::prime::PrimeOutcome::Success { new_reset_at } => {
                    entry.status = Some("waiting".into());
                    entry.last_confirmed_reset = Some(new_reset_at.clone());
                    entry.next_at = Some(new_reset_at);
                    entry.retry_count = 0;
                }
                crate::prime::PrimeOutcome::Hold { reset_at } => {
                    entry.status = Some("waiting".into());
                    entry.next_at = Some(reset_at);
                }
                crate::prime::PrimeOutcome::SkipNoToken => {
                    entry.status = Some("failed".into());
                }
                crate::prime::PrimeOutcome::FailSend { .. }
                | crate::prime::PrimeOutcome::FailUnconfirmed => {
                    entry.status = Some("failed".into());
                    entry.retry_count = entry.retry_count.saturating_add(1).min(5);
                    let delay_minutes = 1_i64 << entry.retry_count.saturating_sub(1).min(3);
                    entry.next_at = Some(
                        (chrono::Utc::now() + chrono::Duration::minutes(delay_minutes))
                            .to_rfc3339(),
                    );
                }
            }
            let confirmed = entry.last_confirmed_reset.clone();
            let _ = entry;
            let _ = self.store.save(&data);
            self.append_prime_log(&format!(
                "{} Auto Ping {}: before event={} after={:?}",
                now(),
                job.name,
                job.event,
                confirmed
            ));
        }
        if let Some(app) = app {
            if let Ok(snapshot) = self.snapshot() {
                let _ = app.emit("snapshot-changed", snapshot);
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Auto session prime
    // ---------------------------------------------------------------------------

    /// Set (or clear) the daily prime schedule for one account. Setting overwrites the previous
    /// time — there is exactly one prime time per account, and it persists until changed.
    pub fn set_auto_prime(
        &self,
        tool_id: ToolId,
        account_id: String,
        enabled: bool,
        time: String,
    ) -> Result<AppSnapshot> {
        let time = normalize_hhmm(&time)
            .ok_or_else(|| anyhow::anyhow!("Invalid time (expected 24-hour HH:MM format)"))?;
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            if !prime_eligible_in_state(&data, &tool_id, &account_id) {
                anyhow::bail!("This account does not support auto prime (subscription-based Claude/Codex accounts only)");
            }
            let entry = data.auto_prime.entry(account_id).or_default();
            let time_changed = entry.time != time;
            entry.enabled = enabled;
            entry.time = time;
            // Changing the time starts a fresh schedule: clear the "already primed today" guard
            // so the new time can run today even if the old time already primed.
            if time_changed {
                entry.last_primed_date = None;
                entry.last_primed_time = None;
                // A pending SCHEDULED hold-defer from the OLD time must not suppress the NEW time —
                // but DON'T clear a defer that belongs to an armed auto-extend (it's tied to the
                // running 5h window, not the daily anchor), or the extend would fire early.
                if !entry.extend_requested {
                    entry.deferred_until = None;
                }
            }
            // Disabling clears a SCHEDULED hold-defer so it can't linger as stale state and suppress
            // due-collection if the same schedule is re-enabled later. An extend's defer is left
            // alone (it's governed by extend_requested, not the enabled flag).
            if !enabled && !entry.extend_requested {
                entry.deferred_until = None;
            }
            let _ = tool_id;
            self.store.save(&data)?;
        }
        self.update_wake_schedule();
        self.snapshot()
    }

    /// Apply one time + enabled flag to EVERY prime-eligible account ("Apply all").
    pub fn set_auto_prime_all(&self, time: String, enabled: bool) -> Result<AppSnapshot> {
        let time = normalize_hhmm(&time)
            .ok_or_else(|| anyhow::anyhow!("Invalid time (expected 24-hour HH:MM format)"))?;
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let eligible: Vec<String> = data
                .accounts
                .iter()
                .filter(|a| {
                    !a.is_default
                        && crate::prime::is_prime_eligible(&a.tool_id, a.api_provider.is_some())
                })
                .map(|a| a.id.clone())
                .collect();
            for account_id in eligible {
                let entry = data.auto_prime.entry(account_id).or_default();
                let time_changed = entry.time != time;
                entry.enabled = enabled;
                entry.time = time.clone();
                if time_changed {
                    entry.last_primed_date = None;
                    entry.last_primed_time = None;
                    // Don't clear an armed auto-extend's defer (see set_auto_prime).
                    if !entry.extend_requested {
                        entry.deferred_until = None;
                    }
                }
                // Same as set_auto_prime: disabling clears a stale scheduled hold-defer (but not an
                // extend's defer).
                if !enabled && !entry.extend_requested {
                    entry.deferred_until = None;
                }
            }
            self.store.save(&data)?;
        }
        self.update_wake_schedule();
        self.snapshot()
    }

    /// Read the human-readable auto-prime activity log (newest content at the bottom).
    pub fn auto_prime_log(&self) -> String {
        std::fs::read_to_string(self.store.auto_prime_log_path()).unwrap_or_default()
    }

    /// Recompute the next pmset wake (earliest upcoming prime time, minus the lead) and write it to
    /// the helper's request file. No-op (clears the wake) if the helper isn't installed or nothing
    /// is scheduled. Safe to call after any schedule change. Derives the wake from persisted state
    /// (each account's anchor time + any pending hold-defer), so it's correct no matter which path
    /// triggers the recompute.
    pub fn update_wake_schedule(&self) {
        if !crate::wake::helper_installed() {
            return; // milestone-1 behavior: rely on the app being awake
        }
        let next = self.next_wake_time();
        let _ = crate::wake::write_wake_request(&self.store, next);
    }

    /// The earliest moment the Mac should wake: min over enabled accounts of (today/tomorrow's
    /// prime time − lead) AND any account's pending hold-defer (`deferred_until`, when a prime was
    /// held until the old window ends). Returns None if nothing is scheduled.
    fn next_wake_time(&self) -> Option<chrono::DateTime<chrono::Local>> {
        use chrono::TimeZone;
        let data = self.data.lock().ok()?;
        let now = chrono::Local::now();
        let mut earliest: Option<chrono::DateTime<chrono::Local>> = None;
        let mut consider = |candidate: chrono::DateTime<chrono::Local>| {
            // Ignore past candidates entirely. A wake we're already past (e.g. it's 10:55 and a
            // prime at 11:00 gives a 10:50 lead instant) must NOT become `earliest` — that would let
            // the final "in the future" filter discard a VALID future wake from another account,
            // leaving the Mac with no wake at all. Each candidate is judged on its own.
            if candidate <= now {
                return;
            }
            earliest = Some(match earliest {
                Some(current) if current <= candidate => current,
                _ => candidate,
            });
        };
        for account in &data.accounts {
            let Some(setting) = data.auto_prime.get(&account.id) else {
                continue;
            };
            if account.is_default
                || !crate::prime::is_prime_eligible(
                    &account.tool_id,
                    account.api_provider.is_some(),
                )
            {
                continue;
            }
            // A pending, ACTIONABLE defer (a held prime, or an armed auto-extend waiting for the
            // window to end) is a commitment the scheduler WILL act on regardless of the daily
            // `enabled` flag — `run_due_primes` fires `extend_requested` whether or not auto-prime is
            // enabled. So fold it into the wake REGARDLESS of `enabled`; otherwise an extend armed on
            // an account with daily auto-prime off would run only if the app happened to be awake,
            // never waking the Mac for it. `defer_is_actionable` filters out a stale leftover defer
            // on a disabled account with no extend armed. Wake `lead` minutes early.
            if defer_is_actionable(&account.tool_id, account.api_provider.is_some(), setting) {
                if let Some(until) = &setting.deferred_until {
                    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(until) {
                        let local = t.with_timezone(&chrono::Local)
                            - chrono::Duration::minutes(crate::wake::WAKE_LEAD_MIN);
                        if local > now {
                            consider(local);
                        }
                    }
                }
            }
            // The daily scheduled time only matters when auto-prime is enabled for this account.
            if !setting.enabled {
                continue;
            }
            let Some((h, m)) = parse_hhmm(&setting.time) else {
                continue;
            };
            // The next wake instant for this account = (prime time − lead). Roll the day over based
            // on the WAKE instant, not the prime time: at 10:55 for an 11:00 prime with a 10-min
            // lead, today's wake (10:50) is already past, so we must target TOMORROW's 10:50 — not
            // keep today's, which would be a past candidate that `consider` then discards, leaving
            // this account with no wake at all.
            let lead = chrono::Duration::minutes(crate::wake::WAKE_LEAD_MIN);
            let mut wake = now
                .date_naive()
                .and_hms_opt(h, m, 0)
                .and_then(|naive| chrono::Local.from_local_datetime(&naive).single())?
                - lead;
            if wake <= now {
                wake += chrono::Duration::days(1);
            }
            consider(wake);
        }
        // Never schedule a wake in the past.
        earliest.filter(|t| *t > now)
    }

    /// Minutes until the soonest ACTIONABLE pending defer (held prime or armed extend) that is due
    /// within the wake-lead window — INCLUDING ones already overdue (negative minutes), so a defer
    /// that lapsed while we were busy still counts as "there's work here". `None` when no actionable
    /// defer is in range. Used to (a) decide whether to keep the Mac awake and (b) drive the
    /// scheduler's wait-and-reprime loop. Capped at `+lead`: a defer further out isn't our concern
    /// yet (the pmset wake will bring us back ~lead minutes ahead of it).
    fn soonest_actionable_defer_mins(&self) -> Option<i64> {
        let data = self.data.lock().ok()?;
        data.accounts
            .iter()
            .filter_map(|account| {
                let setting = data.auto_prime.get(&account.id)?;
                if !defer_is_actionable(&account.tool_id, account.api_provider.is_some(), setting) {
                    return None;
                }
                let mins = minutes_until(setting.deferred_until.as_deref()?);
                // In range = anytime up to `lead` ahead, OR already overdue (negative) but not yet
                // acted on. The upper bound keeps far-future defers out; there's no lower bound.
                if mins <= crate::wake::WAKE_LEAD_MIN {
                    Some(mins)
                } else {
                    None
                }
            })
            .min()
    }

    /// Per-account auto-extend toggle: when on, the app extends without asking.
    pub fn set_auto_extend(
        &self,
        tool_id: ToolId,
        account_id: String,
        enabled: bool,
    ) -> Result<AppSnapshot> {
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            if !prime_eligible_in_state(&data, &tool_id, &account_id) {
                anyhow::bail!("This account does not support auto prime");
            }
            let entry = data.auto_prime.entry(account_id).or_default();
            entry.auto_extend = enabled;
            // Turning auto-extend OFF must also cancel any extend this toggle already armed for the
            // current window-ending (check_extend_reminders sets extend_requested + deferred_until).
            // Otherwise the scheduler would still fire the auto-extend the user just disabled.
            if !enabled {
                entry.extend_requested = false;
                entry.deferred_until = None;
                entry.extend_reminded_reset = None;
            }
            self.store.save(&data)?;
        }
        // The cancelled defer may have been the soonest wake — recompute so the Mac doesn't wake for
        // a prime that's no longer scheduled.
        self.update_wake_schedule();
        self.snapshot()
    }

    /// Tally auto-prime outcomes per local day from the activity log, newest day first, capped to
    /// the most recent `max_days`. Pure read of the log file; no state needed.
    pub fn auto_prime_stats(&self, max_days: usize) -> Vec<crate::models::AutoPrimeDayStat> {
        use crate::models::AutoPrimeDayStat;
        use std::collections::BTreeMap;
        let log = self.auto_prime_log();
        let mut by_day: BTreeMap<String, AutoPrimeDayStat> = BTreeMap::new();
        for line in log.lines() {
            // Lines look like: `[YYYY-MM-DD HH:MM:SS] <Tool> · account "X" — STATUS: ...`
            let Some(date) = line
                .strip_prefix('[')
                .and_then(|rest| rest.get(0..10))
                .filter(|d| d.len() == 10 && d.as_bytes()[4] == b'-')
            else {
                continue;
            };
            let entry = by_day
                .entry(date.to_string())
                .or_insert_with(|| AutoPrimeDayStat {
                    date: date.to_string(),
                    ..Default::default()
                });
            // Classify by the marker the wording uses (see brainstorm Mục 7.3).
            if line.contains("— SUCCESS") {
                entry.success += 1;
            } else if line.contains("— FAIL") {
                entry.failed += 1;
            } else if line.contains("— HOLD") || line.contains("— HOÃN") {
                entry.hold += 1;
            } else if line.contains("— SKIP") {
                entry.skip += 1;
            }
        }
        let mut days: Vec<AutoPrimeDayStat> = by_day.into_values().collect();
        days.sort_by(|a, b| b.date.cmp(&a.date)); // newest first
        days.truncate(max_days);
        days
    }

    /// On-demand extend (mechanism 2): record the user's answer to the "extend?" prompt.
    /// Accept → prime once the current window ends. Dismiss → clear the request.
    pub fn confirm_extend(
        &self,
        tool_id: ToolId,
        account_id: String,
        accept: bool,
    ) -> Result<AppSnapshot> {
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            if !prime_eligible_in_state(&data, &tool_id, &account_id) {
                anyhow::bail!("This account does not support auto prime");
            }
            // The window the user is answering about (to mark a dismiss against it).
            let current_reset = data
                .accounts
                .iter()
                .find(|a| a.id == account_id && a.tool_id == tool_id)
                .and_then(|a| a.quota.as_ref())
                .and_then(|q| q.five_hour.reset_at.clone());
            let entry = data.auto_prime.entry(account_id).or_default();
            entry.extend_requested = accept;
            if accept {
                entry.extend_dismissed_reset = None;
                // Defer the prime to just after the window actually ends (same grace as auto-extend),
                // so accepting mid-window doesn't trigger an immediate attempt that just HOLDs and
                // logs "HOÃN". If we don't know the current reset_at, fall through with no defer and
                // let prime_account's D2 HOLD handle it.
                entry.deferred_until = current_reset.as_deref().map(reset_at_plus_grace);
            } else {
                // Dismiss: remember it for THIS window so the UI hides the button and the poller
                // doesn't prompt again for the same window-ending.
                entry.extend_dismissed_reset = current_reset;
            }
            self.store.save(&data)?;
        }
        // Accepting may have set a defer that's now the soonest wake; recompute so the Mac wakes for it.
        self.update_wake_schedule();
        self.snapshot()
    }

    /// Detect prime-eligible accounts whose 5h window is about to end (≤30 min) and prompt the user
    /// once per window-ending to extend. Called from the background poller. Reminds via a system
    /// notification + flags the account so the UI shows an "extend?" button.
    pub fn check_extend_reminders(&self, app: Option<&AppHandle>) {
        let mut to_remind: Vec<(String, String)> = Vec::new(); // (tool_label, account_name)
                                                               // (tool_label, account_name, deferred_until ISO): accepted automatically. The defer instant
                                                               // is the reset boundary OR the daily anchor (when the schedule governs the next window) —
                                                               // carried so the log can say which it is.
        let mut auto_extended: Vec<(String, String, String)> = Vec::new();
        let mut no_response: Vec<String> = Vec::new(); // log lines
        let mut changed = false;
        {
            let mut data = match self.data.lock() {
                Ok(data) => data,
                Err(_) => return,
            };
            // Snapshot (account_id, tool_label, name, reset_at) first to avoid borrow conflicts.
            let candidates: Vec<(String, String, String, String)> = data
                .accounts
                .iter()
                .filter(|a| {
                    !a.is_default
                        && crate::prime::is_prime_eligible(&a.tool_id, a.api_provider.is_some())
                })
                .filter_map(|a| {
                    let reset = a.quota.as_ref()?.five_hour.reset_at.clone()?;
                    Some((
                        a.id.clone(),
                        a.tool_id.prime_label().to_string(),
                        a.name.clone(),
                        reset,
                    ))
                })
                .collect();
            for (account_id, tool_label, account_name, reset_at) in candidates {
                let mins = minutes_until(&reset_at);
                let entry = data.auto_prime.entry(account_id).or_default();

                if (0..=EXTEND_THRESHOLD_MIN).contains(&mins) {
                    // Window ending soon. Skip if already accepted, already reminded for this
                    // window, or the user explicitly dismissed this window.
                    if entry.extend_reminded_reset.as_deref() == Some(reset_at.as_str())
                        || entry.extend_dismissed_reset.as_deref() == Some(reset_at.as_str())
                        || entry.extend_requested
                    {
                        continue;
                    }
                    if entry.auto_extend {
                        // Auto-extend ON: accept on the user's behalf, no prompt. But DON'T prime
                        // now — the old window is still running (mins ≥ 0), so a prime would land
                        // inside it and HOLD, spamming "HOÃN" every tick for the whole final 30'.
                        // Instead, arm the request AND defer it: the scheduler skips the account
                        // (deferred_until is still future) until the defer instant, then primes once.
                        //
                        // Defer target: prefer the daily anchor if it falls inside the window this
                        // extend would open — the schedule, not the drifting boundary, should set
                        // when the fresh 5h starts (issue #7: a 3am extend must not steal the 11:00
                        // boundary the user actually wants). If the anchor doesn't apply, fall back
                        // to just AFTER the reset instant; the grace past reset_at absorbs provider
                        // clock skew (priming a beat early reads the old window as still-live → HOLD).
                        entry.extend_requested = true;
                        let defer = scheduled_anchor_within_window(&reset_at, &entry.time)
                            .unwrap_or_else(|| reset_at_plus_grace(&reset_at));
                        entry.deferred_until = Some(defer.clone());
                        entry.extend_reminded_reset = Some(reset_at.clone());
                        changed = true;
                        auto_extended.push((tool_label, account_name, defer));
                    } else {
                        // Default: prompt once per this window-ending.
                        entry.extend_reminded_reset = Some(reset_at.clone());
                        changed = true;
                        to_remind.push((tool_label, account_name));
                    }
                } else if mins < 0 {
                    // Window has ended. If we reminded for THIS window and the user never accepted,
                    // log the "no response" note once, then clear the reminder.
                    if entry.extend_reminded_reset.as_deref() == Some(reset_at.as_str())
                        && !entry.extend_requested
                    {
                        no_response.push(format!(
                            "{tool_label} · account \"{account_name}\" — extension reminder expired without a response; the session has ended"
                        ));
                        entry.extend_reminded_reset = None;
                        changed = true;
                    }
                }
            }
            if changed {
                let _ = self.store.save(&data);
            }
        }
        for line in &no_response {
            self.append_prime_log(line);
        }
        for (tool_label, account_name, defer) in &auto_extended {
            self.append_prime_log(&format!(
                "{tool_label} · account \"{account_name}\" — auto-extend ON: a new session will open at {at} (at the scheduled time if it falls within the next 5 hours, otherwise when the current session ends)",
                at = local_hhmm_from_iso(defer)
            ));
        }
        // An armed auto-extend just set a new `deferred_until` (= reset_at + grace). That instant may
        // be the soonest upcoming prime, so the Mac must be told to wake for it — otherwise it could
        // stay asleep through the window-ending and the extend would only fire late ("primed muộn").
        if !auto_extended.is_empty() {
            self.update_wake_schedule();
        }
        if let Some(app) = app {
            // Coalesce: when several accounts enter the final 30' together (common after "Apply
            // all"), fire ONE notification instead of one per account, so the OS isn't spammed.
            match to_remind.as_slice() {
                [] => {}
                [(tool_label, account_name)] => {
                    let _ = app
                        .notification()
                        .builder()
                        .title("Session ending soon")
                        .body(format!(
                            "The {tool_label} session for account \"{account_name}\" has {EXTEND_THRESHOLD_MIN} minutes remaining. Open the app to extend it for uninterrupted work."
                        ))
                        .show();
                }
                many => {
                    let names = many
                        .iter()
                        .map(|(_, name)| name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = app
                        .notification()
                        .builder()
                        .title(format!("{} sessions ending soon", many.len()))
                        .body(format!(
                            "{names} have ≤{EXTEND_THRESHOLD_MIN} minutes remaining. Open the app to extend them if needed."
                        ))
                        .show();
                }
            }
            if changed {
                let _ = app.emit("auto-prime-changed", ());
            }
        }
    }

    /// Append one event line to the auto-prime log. Wording is the brainstorm's exact strings.
    fn append_prime_log(&self, line: &str) {
        use std::io::Write;
        let stamped = format!("[{}] {}\n", local_log_timestamp(), line);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.store.auto_prime_log_path())
        {
            let _ = file.write_all(stamped.as_bytes());
        }
    }

    /// The scheduler tick: prime every account whose time has arrived and that hasn't yet primed
    /// for that time today. Runs sequentially with a gap between accounts. Called by the background
    /// thread (every minute) and once at startup (to catch a missed time → "primed muộn"). `late`
    /// marks the startup catch-up so the log says so.
    pub fn run_due_primes(&self, app: Option<&AppHandle>, late: bool) {
        use std::sync::atomic::Ordering;
        // Skip if a previous batch is still running (send retries can take minutes). The flag is
        // cleared at the end of this call; `_guard` ensures that even on an early return.
        if self
            .priming
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        struct ClearOnDrop<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for ClearOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _guard = ClearOnDrop(&self.priming);

        // Hold the Mac awake for the WHOLE tick — created up front and unconditionally, so it also
        // covers a defer that the first prime pass itself creates (a Hold sets `deferred_until`
        // within the lead window). `caffeinate -i` is cheap; if there's nothing to wait for, the
        // loop below exits immediately and the hold drops (kill + reap) on return.
        let _awake = CaffeinateHold::active();

        // Prime-then-bridge loop. Each iteration: prime everything due RIGHT NOW (so a scheduled
        // account whose time just arrived never waits behind another account's not-yet-due defer),
        // then look at the soonest actionable defer:
        //   - overdue / due now (mins <= 0): loop again immediately to prime it (this is the case
        //     Codex flagged — a defer that lapsed *during* the previous prime must not be skipped);
        //   - still ahead but within the lead window: sleep a short beat (Mac kept awake by the hold)
        //     and loop, so the new window opens the moment the old one ends, not a tick later;
        //   - nothing actionable in range: done.
        // Bounded by a wall-clock deadline (lead window) so a miscomputed/stuck defer can't pin the
        // scheduler. The `priming` overlap guard keeps concurrent ticks from starting a second batch.
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs((crate::wake::WAKE_LEAD_MIN as u64) * 60);
        let mut last_overdue: Option<i64> = None;
        loop {
            self.collect_and_prime_due(late, app);
            match self.soonest_actionable_defer_mins() {
                Some(mins) if mins > 0 => {
                    last_overdue = None;
                    if std::time::Instant::now() >= deadline {
                        break; // safety cap — don't wait past the lead window
                    }
                    std::thread::sleep(std::time::Duration::from_secs(20));
                }
                // Overdue (mins <= 0): the prime pass should have handled it. If the SAME overdue
                // defer is still here after a pass (collect_and_prime_due couldn't act on it — e.g.
                // an enabled account whose held defer lapsed but whose daily slot is already consumed
                // for today, so neither scheduled_due nor extend_due is true), don't spin: a tiny
                // sleep + one more look, then bail. This avoids a busy-loop on a defer nothing primes.
                Some(mins) => {
                    if last_overdue == Some(mins) {
                        break; // no progress on this overdue defer → stop, let a later tick retry
                    }
                    last_overdue = Some(mins);
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
                None => break,
            }
        }
    }

    /// Collect the accounts due to prime right now and prime each, sequentially with a short gap.
    /// "Due" = a scheduled prime whose time has arrived (once/day, enabled) OR an armed extend; an
    /// account whose `deferred_until` is still in the future is skipped (it isn't due yet).
    fn collect_and_prime_due(&self, late: bool, app: Option<&AppHandle>) {
        let today = local_today();
        let now_hhmm = local_now_hhmm();
        let due: Vec<PrimeJob> = {
            let data = match self.data.lock() {
                Ok(data) => data,
                Err(_) => return,
            };
            data.accounts
                .iter()
                .filter_map(|account| {
                    let setting = data.auto_prime.get(&account.id)?;
                    if account.is_default
                        || !crate::prime::is_prime_eligible(
                            &account.tool_id,
                            account.api_provider.is_some(),
                        )
                    {
                        return None;
                    }
                    // Held earlier (old window still active): wait until the defer instant before
                    // re-attempting, so we don't read quota + log HOÃN every single minute.
                    if let Some(until) = &setting.deferred_until {
                        if minutes_until(until) > 0 {
                            return None;
                        }
                    }
                    let scheduled_due = setting.enabled
                        && now_hhmm.as_str() >= setting.time.as_str()
                        && !(setting.last_primed_date.as_deref() == Some(today.as_str())
                            && setting.last_primed_time.as_deref() == Some(setting.time.as_str()));
                    let extend_due = setting.extend_requested;
                    if !scheduled_due && !extend_due {
                        return None;
                    }
                    let default_dir = resolved_default_config_dir(&data, &account.tool_id);
                    let config_dir =
                        account_config_dir_with_default(&self.store, account, &default_dir);
                    let binary = configured_binary_path(&data, &account.tool_id);
                    Some(PrimeJob {
                        tool_id: account.tool_id.clone(),
                        account_id: account.id.clone(),
                        account_name: account.name.clone(),
                        config_dir,
                        binary,
                        // An accepted extend request is an extend, even if the daily time also
                        // happens to have arrived (deferred extends can come due after `time`):
                        // log it as "gia hạn", not a scheduled prime.
                        is_extend: extend_due,
                        // Whether the daily anchor actually triggered this run. Used to decide if a
                        // terminal outcome should consume today's scheduled slot. If the daily time
                        // genuinely arrived (even alongside an extend), it IS the scheduled run.
                        scheduled: scheduled_due,
                        manual: false,
                    })
                })
                .collect()
        };

        for (index, job) in due.iter().enumerate() {
            if index > 0 {
                std::thread::sleep(std::time::Duration::from_secs(10)); // gap between accounts
            }
            let outcome = crate::prime::prime_account(
                &job.tool_id,
                &job.config_dir,
                job.binary.as_deref(),
                self.store.root_dir(),
                crate::prime::SEND_MAX_ATTEMPTS,
                std::thread::sleep,
            );
            self.record_prime_outcome(job, &outcome, late, app);
        }
    }

    /// Prime ONE account on demand ("Prime ngay" button) — open a fresh 5h window right now instead
    /// of waiting for the scheduled time or for the app to do it. Used when a window has already
    /// ended and the user wants the next one started without dropping to a terminal. Shares the
    /// scheduler's `priming` overlap guard (so it can't run concurrently with a batch) and the same
    /// `prime_account` flow + outcome recording. Returns a short human message describing what
    /// happened (success / still-held / failure), surfaced to the UI as a toast.
    pub fn prime_now(
        &self,
        tool_id: ToolId,
        account_id: String,
        app: Option<&AppHandle>,
    ) -> Result<crate::models::PrimeNowResult> {
        use crate::prime::PrimeOutcome;
        use std::sync::atomic::Ordering;

        // Build the job under a brief lock; reject early if the account isn't prime-eligible.
        let job = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let account = data
                .accounts
                .iter()
                .find(|a| a.id == account_id && a.tool_id == tool_id)
                .ok_or_else(|| anyhow::anyhow!("Account not found"))?;
            if account.is_default
                || !crate::prime::is_prime_eligible(
                    &account.tool_id,
                    account.api_provider.is_some(),
                )
            {
                anyhow::bail!(
                    "This account does not support priming (subscription-based Claude/Codex accounts only)"
                );
            }
            let default_dir = resolved_default_config_dir(&data, &account.tool_id);
            let config_dir = account_config_dir_with_default(&self.store, account, &default_dir);
            let binary = configured_binary_path(&data, &account.tool_id);
            PrimeJob {
                tool_id: account.tool_id.clone(),
                account_id: account.id.clone(),
                account_name: account.name.clone(),
                config_dir,
                binary,
                // Manual prime is its own thing — not a scheduled run, not an extend acceptance.
                is_extend: false,
                scheduled: false,
                manual: true,
            }
        };

        // Don't run on top of an in-flight scheduler batch (or another manual prime).
        if self
            .priming
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            anyhow::bail!("Another prime operation is already running — try again shortly.");
        }
        struct ClearOnDrop<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for ClearOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _guard = ClearOnDrop(&self.priming);
        // Keep the Mac awake for the duration (the prime itself also holds caffeinate; this covers
        // the whole on-demand op for symmetry).
        let _awake = CaffeinateHold::active();

        // On-demand prime fails fast: a SINGLE send attempt (no 5×5' backoff that the scheduler
        // uses), so a button press returns in seconds and the user can just tap again. D4's short
        // confirm polls (5 × 30s) still run with real sleeps — they're needed to see the new window,
        // and prime_account's per-attempt CLI timeout bounds the rest.
        let outcome = crate::prime::prime_account(
            &job.tool_id,
            &job.config_dir,
            job.binary.as_deref(),
            self.store.root_dir(),
            1,
            std::thread::sleep,
        );
        self.record_prime_outcome(&job, &outcome, false, app);

        let (kind, message) = match &outcome {
            PrimeOutcome::Success { new_reset_at } => (
                "success",
                format!("New session opened — resets at {}", local_hhmm_from_iso(new_reset_at)),
            ),
            // Not an error: the window is simply still running (or was just refreshed by another
            // account that shares this ChatGPT/Claude login), so a new one can't open yet.
            PrimeOutcome::Hold { reset_at } => (
                "info",
                format!(
                    "The current session is still active (until {}). A new session can open after it ends.",
                    local_hhmm_from_iso(reset_at)
                ),
            ),
            PrimeOutcome::SkipNoToken => (
                "error",
                "The token has expired — sign in to this account again.".to_string(),
            ),
            PrimeOutcome::FailSend { reason } => ("error", format!("Send failed: {reason}")),
            PrimeOutcome::FailUnconfirmed => (
                "error",
                "The message was sent, but the new session could not be confirmed. Try again shortly.".to_string(),
            ),
        };
        Ok(crate::models::PrimeNowResult {
            kind: kind.to_string(),
            message,
        })
    }

    /// Persist the per-account result + append the matching log line, and refresh the account's
    /// displayed quota on success so the new reset shows immediately.
    fn record_prime_outcome(
        &self,
        job: &PrimeJob,
        outcome: &crate::prime::PrimeOutcome,
        late: bool,
        app: Option<&AppHandle>,
    ) {
        use crate::prime::PrimeOutcome;
        let tool_id = &job.tool_id;
        let account_id = job.account_id.as_str();
        let account_name = job.account_name.as_str();
        let is_extend = job.is_extend;
        let is_manual = job.manual;
        let tool_label = tool_id.prime_label();
        let (result, line): (&str, String) = match outcome {
            PrimeOutcome::Success { new_reset_at } => {
                let reset = local_hhmm_from_iso(new_reset_at);
                if late {
                    (
                        "success",
                        format!(
                            "{tool_label} · account \"{account_name}\" — SUCCESS: primed late at {now} because the machine did not wake on schedule; new reset = {reset}",
                            now = local_now_hhmm()
                        ),
                    )
                } else if is_extend {
                    (
                        "success",
                        format!("{tool_label} · account \"{account_name}\" — SUCCESS: extended; new session started, new reset = {reset}"),
                    )
                } else {
                    (
                        "success",
                        format!("{tool_label} · account \"{account_name}\" — SUCCESS: primed, new reset = {reset}"),
                    )
                }
            }
            PrimeOutcome::Hold { reset_at } => (
                "hold",
                format!(
                    "{tool_label} · account \"{account_name}\" — HOLD: current session remains active until {reset}; retrying after it ends",
                    reset = local_hhmm_from_iso(reset_at)
                ),
            ),
            PrimeOutcome::SkipNoToken => (
                "skip",
                format!("{tool_label} · account \"{account_name}\" — SKIP: token expired; sign in again"),
            ),
            PrimeOutcome::FailSend { reason } => (
                "failed",
                if is_manual {
                    format!("{tool_label} · account \"{account_name}\" — FAIL: manual prime could not send ({reason})")
                } else {
                    format!("{tool_label} · account \"{account_name}\" — FAIL: send failed after 5 attempts ({reason})")
                },
            ),
            PrimeOutcome::FailUnconfirmed => (
                "failed",
                format!("{tool_label} · account \"{account_name}\" — FAIL: message sent, but the new session was not confirmed after 5 checks"),
            ),
        };

        // Update the per-account schedule record. A "terminal" outcome (we opened a window or gave
        // up after the full retry budget) consumes today's scheduled attempt so the scheduler does
        // NOT restart a fresh retry batch every minute. HOLD/SKIP are not terminal: HOLD defers,
        // SKIP just waits for a valid token.
        // Track whether anything user-visible changed, so we only nudge the UI to re-pull when it
        // matters — a SkipNoToken that repeats every minute (expired token) shouldn't trigger a
        // reload (and a flash of disabled controls) each time.
        let mut visible_change = false;
        if let Ok(mut data) = self.data.lock() {
            if let Some(setting) = data.auto_prime.get_mut(account_id) {
                visible_change = setting.last_result.as_deref() != Some(result);
                setting.last_result = Some(result.to_string());
                setting.last_attempt_at = Some(now());
                let succeeded = matches!(outcome, PrimeOutcome::Success { .. });
                let terminal = matches!(
                    outcome,
                    PrimeOutcome::Success { .. }
                        | PrimeOutcome::FailUnconfirmed
                        | PrimeOutcome::FailSend { .. }
                );
                // "Consume" today's scheduled slot (set last_primed_date/time so the daily prime
                // doesn't re-fire) ONLY for a job that the daily schedule actually triggered. A manual
                // "Prime ngay" or a one-time extend is NOT the daily anchor — letting their success
                // stamp last_primed would suppress a later daily prime (e.g. a 06:00 manual prime
                // cancelling the 11:00 anchor). A scheduled job consumes on any terminal outcome (it
                // used its full retry budget and is done for today).
                let consume_scheduled = job.scheduled && terminal;
                if consume_scheduled {
                    setting.last_primed_date = Some(local_today());
                    setting.last_primed_time = Some(setting.time.clone());
                }
                // Resolve the extend/defer state only when this attempt actually settled THIS
                // account's pending request — i.e. a terminal SCHEDULER/extend job, or a manual
                // SUCCESS. A manual FAILURE must leave any armed auto-extend untouched: the manual
                // job set is_extend=false and isn't the extend, so clearing here would silently
                // disarm an extend the user is still waiting on. (`resolve_pending` mirrors
                // `consume_scheduled`'s manual-fail carve-out, for the extend flags this time.)
                let resolve_pending = if job.manual { succeeded } else { terminal };
                if resolve_pending {
                    // The extend request is resolved (opened, or exhausted retries) — clear it so it
                    // doesn't re-fire next tick. Also clear the "reminded for this window" marker:
                    // it belongs to the window that just ended, and leaving it set is stale state.
                    // (The UI already hides the prompt via reset_at mismatch, but don't rely on that
                    // — a fresh window earns a fresh reminder from the poller.)
                    setting.extend_requested = false;
                    setting.deferred_until = None;
                    setting.extend_reminded_reset = None;
                }
                match outcome {
                    // Held: defer re-attempt until just after the old window ends, so we don't
                    // read quota + log HOÃN every minute. Skip for a MANUAL prime — it's a one-shot
                    // (the user taps again if they want), so it must not arm an automatic retry, and
                    // crucially must not stomp a `deferred_until` an auto-extend already set on this
                    // account. The manual attempt just reported "Hold" to the user via the toast.
                    PrimeOutcome::Hold { reset_at } if !job.manual => {
                        setting.deferred_until = chrono::DateTime::parse_from_rfc3339(reset_at)
                            .ok()
                            .map(|t| {
                                (t.with_timezone(&chrono::Utc) + chrono::Duration::minutes(5))
                                    .to_rfc3339()
                            });
                    }
                    // Manual success already cleared deferred_until via resolve_pending; a scheduler
                    // success clears it here.
                    PrimeOutcome::Success { .. } if !job.manual => setting.deferred_until = None,
                    _ => {}
                }
            }
            let _ = self.store.save(&data);
        }

        // Append the activity-log line only when something changed (new result), it's a manual
        // attempt, or it's a terminal outcome worth recording. A SkipNoToken/Hold that repeats
        // unchanged every minute (e.g. an offline token that won't refresh) must NOT spam the log.
        let terminal = matches!(
            outcome,
            PrimeOutcome::Success { .. }
                | PrimeOutcome::FailUnconfirmed
                | PrimeOutcome::FailSend { .. }
        );
        if visible_change || job.manual || terminal {
            self.append_prime_log(&line);
        }

        match outcome {
            // On success, refresh the displayed quota right away + recompute the next wake.
            PrimeOutcome::Success { .. } => {
                let _ = self.refresh_single_account(tool_id, account_id, app);
                self.update_wake_schedule();
            }
            // Held: `deferred_until` was just persisted above (= reset_at + 5'). Recompute the wake
            // so the Mac wakes right after the old window ends — next_wake_time folds it in.
            PrimeOutcome::Hold { .. } => {
                self.update_wake_schedule();
            }
            _ => {}
        }
        // Nudge the UI to re-pull only when something user-visible changed (new result), on success
        // (quota just moved), or for a manual prime (the user is watching for the button's result).
        // This avoids reloading + flashing controls every minute on a repeating SkipNoToken/Hold.
        let should_emit =
            visible_change || job.manual || matches!(outcome, PrimeOutcome::Success { .. });
        if should_emit {
            if let Some(app) = app {
                let _ = app.emit("auto-prime-changed", ());
            }
        }
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
        if tool_id == &ToolId::Codex {
            self.try_seamless_codex_handoff(
                threshold.min(crate::seamless::DEFAULT_HANDOFF_THRESHOLD),
                false,
                app,
            )?;
            return Ok(());
        }
        if !matches!(tool_id, ToolId::Claude) {
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
            .context("Auto-switch failed while refreshing the Codex runtime")?;
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
        use crate::models::{ResumeJob, ResumeJobStatus};
        let (current, event_key, used, explicit_quota_error) = {
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
            let event = account
                .quota
                .as_ref()
                .and_then(|q| q.five_hour.reset_at.clone())
                .unwrap_or_else(|| {
                    used.map(|value| format!("five-hour-{value:.2}"))
                        .unwrap_or_else(|| "quota-error-unknown-window".into())
                });
            (account.clone(), event, used, explicit_quota_error)
        };

        let rollout = self
            .codex_active_rollout
            .lock()
            .ok()
            .and_then(|path| path.clone());
        let checkpoint_result = match rollout {
            Some(path) => crate::handoff::checkpoint_from_rollout(
                &path,
                &self.store.handoff_dir(),
                &current.name,
            )?,
            None => return Ok(true),
        };
        let Some((mut checkpoint, checkpoint_path)) = checkpoint_result else {
            return Ok(true);
        };
        if !crate::seamless::should_checkpoint(
            used.or(explicit_quota_error.then_some(100.0)),
            checkpoint.idle,
        ) {
            return Ok(true);
        }

        let candidate = {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            for job in data.resume_jobs.iter_mut().filter(|job| {
                job.session_id == checkpoint.session_id
                    && matches!(
                        job.status,
                        ResumeJobStatus::Queued | ResumeJobStatus::Checkpointed
                    )
            }) {
                if checkpoint.created_at > job.updated_at {
                    job.status = ResumeJobStatus::Cancelled;
                    job.updated_at = now();
                    job.error = Some("session continued manually".into());
                }
            }
            if crate::seamless::is_duplicate(&data.resume_jobs, &checkpoint.session_id, &event_key)
            {
                return Ok(true);
            }
            let timestamp = now();
            data.resume_jobs.push(ResumeJob {
                id: Uuid::new_v4().to_string(),
                session_id: checkpoint.session_id.clone(),
                event_key: event_key.clone(),
                cwd: checkpoint.cwd.clone(),
                from_account_id: current.id.clone(),
                to_account_id: None,
                checkpoint_path: checkpoint_path.to_string_lossy().to_string(),
                status: ResumeJobStatus::Checkpointed,
                created_at: timestamp.clone(),
                updated_at: timestamp,
                error: None,
            });
            let selected = crate::seamless::select_candidate(&data.accounts, &current.id).cloned();
            if let Some(candidate) = &selected {
                let job = data.resume_jobs.last_mut().expect("just pushed");
                job.to_account_id = Some(candidate.id.clone());
                job.status = ResumeJobStatus::Switching;
                job.updated_at = now();
            }
            self.store.save(&data)?;
            selected
        };
        let Some(candidate) = candidate else {
            if let Ok(mut data) = self.data.lock() {
                if let Some(job) = data.resume_jobs.iter_mut().rev().find(|job| {
                    job.session_id == checkpoint.session_id && job.event_key == event_key
                }) {
                    job.status = ResumeJobStatus::Failed;
                    job.error =
                        Some("no eligible candidate with healthy auth and weekly quota".into());
                    job.updated_at = now();
                }
                let _ = self.store.save(&data);
            }
            if let Some(app) = app {
                let _ = app.emit(
                    "auto-switched",
                    "Checkpoint saved; no eligible Codex account is below 95%.",
                );
            }
            return Ok(true);
        };
        checkpoint.current_account = Some(candidate.name.clone());
        if let Ok(json) = serde_json::to_vec_pretty(&checkpoint) {
            let _ = std::fs::write(&checkpoint_path, json);
        }

        let failure = |message: String| -> Result<bool> {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            if let Some(job) = data
                .resume_jobs
                .iter_mut()
                .rev()
                .find(|j| j.session_id == checkpoint.session_id && j.event_key == event_key)
            {
                job.status = ResumeJobStatus::Failed;
                job.error = Some(message.clone());
                job.updated_at = now();
            }
            self.store.save(&data)?;
            if let Some(app) = app {
                let _ = app.emit(
                    "auto-switched",
                    format!("Seamless resume failed: {message}"),
                );
            }
            Ok(true)
        };

        if let Err(error) = self.switch_account(SwitchAccountInput {
            tool_id: ToolId::Codex,
            account_id: candidate.id.clone(),
        }) {
            return failure(error.to_string());
        }
        if let Err(error) = self.refresh_single_account(&ToolId::Codex, &candidate.id, app) {
            return failure(format!("auth/quota verification failed: {error}"));
        }
        {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let verified = data
                .accounts
                .iter()
                .find(|account| account.tool_id == ToolId::Codex && account.id == candidate.id)
                .and_then(|account| account.quota.as_ref())
                .is_some_and(|quota| {
                    quota.error.is_none()
                        && quota
                            .five_hour
                            .percent_used
                            .is_none_or(|value| value < 95.0)
                        && quota.weekly.percent_used.is_some_and(|value| value < 100.0)
                });
            if !verified {
                drop(data);
                return failure(
                    "auth/quota verification returned invalid or exhausted quota".into(),
                );
            }
        }
        let (config_dir, binary) = {
            let data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            let default_dir = resolved_default_config_dir(&data, &ToolId::Codex);
            (
                account_config_dir_with_default(&self.store, &candidate, &default_dir),
                configured_binary_path(&data, &ToolId::Codex),
            )
        };
        let Some(binary) = binary else {
            return failure("Codex CLI is not configured".into());
        };
        let prompt = crate::handoff::continuation_prompt(&checkpoint_path);
        if let Err(error) = crate::seamless::spawn_exact_resume(
            &binary,
            &config_dir,
            std::path::Path::new(&checkpoint.cwd),
            &checkpoint.session_id,
            &prompt,
        ) {
            return failure(error);
        }
        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            if let Some(job) = data
                .resume_jobs
                .iter_mut()
                .rev()
                .find(|j| j.session_id == checkpoint.session_id && j.event_key == event_key)
            {
                job.status = ResumeJobStatus::Resumed;
                job.updated_at = now();
                job.error = None;
            }
            self.store.save(&data)?;
        }
        if let Some(app) = app {
            let _ = app.emit(
                "auto-switched",
                format!(
                    "Resumed Codex session {} with {}.",
                    checkpoint.session_id, candidate.name
                ),
            );
        }
        Ok(true)
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
        let (default_dir, binary, profile) = {
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
                account.quota = Some(QuotaInfo::with_message(
                    "Codex sign-in expired — approve the new device code",
                ));
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
        let (launcher, was_active) = {
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
            (
                account.launcher_command.clone(),
                account.state == AccountState::Active,
            )
        };

        delete_account_files(&tool_id, &self.store, &account_id)?;
        if let Some(name) = launcher {
            remove_launcher(&name);
        }
        // Deleting the active account (the one the plain command uses) → clear the active file so
        // the plain command falls back to the machine Default, NOT leaving it pointing to a deleted profile.
        if was_active && matches!(tool_id, ToolId::Claude | ToolId::Codex) {
            let _ = clear_active_profile(&tool_id, &self.store);
            let _ = install_shell_hook(&self.store);
        }

        {
            let mut data = self
                .data
                .lock()
                .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
            data.accounts
                .retain(|account| !(account.tool_id == tool_id && account.id == account_id));
            self.store.save(&data)?;
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

fn retire_auto_ping_settings(data: &mut StoredState) {
    data.auto_ping_enabled = false;
    data.auto_ping.clear();
    data.schema_version = data.schema_version.max(2);
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
            // Default first, the rest by name.
            accounts.sort_by(|a, b| {
                b.is_default
                    .cmp(&a.is_default)
                    .then_with(|| a.name.cmp(&b.name))
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
        vec![ToolId::Codex]
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
        restart_codex_runtime: refresh_codex,
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

fn read_file_tail_from(path: &std::path::Path, offset: u64) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

fn codex_quota_error_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
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

/// Mechanism 2 prompts to extend when the 5h window has this many minutes (or fewer) left.
const EXTEND_THRESHOLD_MIN: i64 = 30;

/// Grace past a window's `reset_at` before an armed auto-extend actually primes. The provider's
/// clock can lag ours by tens of seconds at the boundary, so priming exactly at `reset_at` risks
/// reading the old window as still-live (→ HOLD → a wasted 5' retry). A small buffer makes the
/// first attempt land in the new window almost every time.
const EXTEND_RESET_GRACE_MIN: i64 = 2;

/// Length of a subscription window, in minutes (the "5-hour window"). Used to decide whether the
/// daily anchor falls inside the window an auto-extend would open — if it does, the schedule (not
/// the drifting boundary) should set when the fresh window starts.
const FIVE_HOUR_WINDOW_MIN: i64 = 5 * 60;

/// One unit of work for the prime scheduler.
struct PrimeJob {
    tool_id: ToolId,
    account_id: String,
    account_name: String,
    config_dir: std::path::PathBuf,
    /// The account's resolved CLI path (claude/codex), used to prime via the CLI when available.
    binary: Option<std::path::PathBuf>,
    /// True when this job was triggered by an on-demand extend request (mechanism 2) rather than
    /// the scheduled time — used to clear the request on success.
    is_extend: bool,
    /// True when the daily scheduled time actually triggered this job (`scheduled_due`). Only a
    /// scheduled job stamps `last_primed_date/time` on a terminal outcome — a manual or extend-only
    /// job must NOT, or it would suppress a later daily prime that's a separate anchor.
    scheduled: bool,
    /// True when this job came from the "Prime ngay" button. A manual job NEVER consumes the day's
    /// scheduled prime (only a `scheduled` job does) — manual is a separate, on-demand action, so a
    /// later daily anchor still runs. `manual` instead gates extend-state resolution: a manual
    /// FAILURE must not clear an armed auto-extend; a manual SUCCESS does (a fresh window opened).
    manual: bool,
}

/// Whether a pending `deferred_until` on this account is something the scheduler will actually act
/// on — used so the wake calc + the caffeinate wait don't fire on a STALE defer. A defer is
/// actionable when the account is prime-eligible AND either an extend is armed (`run_due_primes`
/// fires `extend_requested` regardless of `enabled`) or daily auto-prime is enabled (a held
/// scheduled prime will retry). A leftover `deferred_until` on a disabled account with no extend
/// armed is stale and must NOT keep the Mac awake or schedule a wake.
fn defer_is_actionable(
    tool_id: &ToolId,
    has_api_provider: bool,
    setting: &crate::models::AutoPrimeSetting,
) -> bool {
    crate::prime::is_prime_eligible(tool_id, has_api_provider)
        && (setting.extend_requested || setting.enabled)
}

/// True if the account exists and is prime-eligible (subscription Claude/Codex).
fn prime_eligible_in_state(data: &StoredState, tool_id: &ToolId, account_id: &str) -> bool {
    data.accounts.iter().any(|a| {
        a.id == account_id
            && a.tool_id == *tool_id
            && !a.is_default
            && crate::prime::is_prime_eligible(&a.tool_id, a.api_provider.is_some())
    })
}

/// Parse a `HH:MM` 24h string into `(hour, minute)`, validating ranges.
fn parse_hhmm(input: &str) -> Option<(u32, u32)> {
    let (h, m) = input.trim().split_once(':')?;
    let hour: u32 = h.parse().ok()?;
    let minute: u32 = m.parse().ok()?;
    (hour <= 23 && minute <= 59).then_some((hour, minute))
}

/// Validate + normalize a `HH:MM` 24h string. Returns `Some("HH:MM")` (zero-padded) or None.
fn normalize_hhmm(input: &str) -> Option<String> {
    let (hour, minute) = parse_hhmm(input)?;
    Some(format!("{hour:02}:{minute:02}"))
}

/// Local date `YYYY-MM-DD` (machine timezone) — the "once per day" key.
fn local_today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Local time `HH:MM` (machine timezone) — compared against the scheduled prime time.
fn local_now_hhmm() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}

/// Local timestamp for log lines: `YYYY-MM-DD HH:MM:SS` (machine timezone).
fn local_log_timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Minutes from now until an ISO 8601 instant. Negative if already past; `i64::MAX` if unparseable.
fn minutes_until(iso: &str) -> i64 {
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(t) => (t.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_minutes(),
        Err(_) => i64::MAX,
    }
}

/// An RAII `caffeinate -i -w <pid>` hold that prevents idle system sleep until dropped. Kept alive
/// for the duration of one scheduler tick that bridges the wait to a deferred prime, so the Mac
/// can't idle-sleep in the gap between a pmset wake and the prime. Drop kills + reaps the child (no
/// zombie); `-w <pid>` self-exits if the app dies before Drop runs (no orphan holding the Mac awake).
struct CaffeinateHold(Option<std::process::Child>);

impl CaffeinateHold {
    fn active() -> Self {
        let child = std::process::Command::new("caffeinate")
            .args(["-i", "-w", &std::process::id().to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok();
        CaffeinateHold(child)
    }
}

impl Drop for CaffeinateHold {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// `reset_at` shifted forward by `EXTEND_RESET_GRACE_MIN`, as an ISO 8601 string. Used to defer an
/// armed auto-extend to just after the window boundary. Returns the input unchanged if unparseable.
fn reset_at_plus_grace(reset_at: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(reset_at) {
        Ok(t) => (t.with_timezone(&chrono::Utc)
            + chrono::Duration::minutes(EXTEND_RESET_GRACE_MIN))
        .to_rfc3339(),
        Err(_) => reset_at.to_string(),
    }
}

/// When an auto-extend is about to arm, the daily schedule — not the drifting "extend the moment
/// the old window ends" timing — should govern when the next window opens, IF the anchor falls
/// inside the window the extend would cover. The schedule is the time the user actually wants their
/// fresh 5h to start (e.g. 11:00, after waking); auto-extend is only a fallback to keep sessions
/// rolling. Deferring to the anchor leaves a deliberate gap (no active session between the old
/// window ending and the anchor) — that's the point: the schedule, not 3am drift, sets the boundary.
///
/// Given the old window's `reset_at` (ISO) and the account's daily anchor `time` (`HH:MM`, local),
/// returns the next occurrence of that anchor as an ISO instant IF it lands strictly after the old
/// window ends AND within the next 5h (the span the extend's new window would otherwise cover).
/// Returns `None` otherwise — the anchor already passed before the window ends, or it's more than a
/// full window away — in which case the caller extends right at the boundary (the old behavior).
fn scheduled_anchor_within_window(reset_at: &str, time: &str) -> Option<String> {
    use chrono::TimeZone;
    let reset = chrono::DateTime::parse_from_rfc3339(reset_at)
        .ok()?
        .with_timezone(&chrono::Local);
    let (h, m) = parse_hhmm(time)?;
    // First occurrence of HH:MM strictly after the window-ending. An anchor exactly at `reset`
    // isn't "after" — there'd be no gap to wait out — so today's HH:MM is skipped in that case and
    // tomorrow's is tried (which will then be >5h away → None → boundary fallback).
    //
    // Build each candidate from its CALENDAR date (not `+ Duration::days(1)` on a DateTime): adding
    // a fixed 24h would drift the wall-clock anchor by ±1h across a DST transition, whereas
    // re-resolving `date+1` at HH:MM in Local keeps the user's chosen clock time. A DST-ambiguous or
    // nonexistent local hour (`.single()` → None) just isn't a candidate, so we fall through to None
    // and the caller extends at the boundary — fine, since this app's target TZ has no DST anyway.
    //
    // The anchor of interest is always within 5h of `reset`, so checking today + tomorrow suffices.
    let reset_date = reset.date_naive();
    let anchor = [
        reset_date,
        reset_date.checked_add_days(chrono::Days::new(1))?,
    ]
    .into_iter()
    .filter_map(|date| {
        chrono::Local
            .from_local_datetime(&date.and_hms_opt(h, m, 0)?)
            .single()
    })
    .find(|dt| *dt > reset)?;
    // Only prefer the anchor if it's within the 5h the extend's window would cover; past that the
    // anchor belongs to a later window and the user would sit too long with no session — extend now.
    if anchor <= reset + chrono::Duration::minutes(FIVE_HOUR_WINDOW_MIN) {
        Some(anchor.with_timezone(&chrono::Utc).to_rfc3339())
    } else {
        None
    }
}

/// Render an ISO 8601 instant as local `HH:MM` for log lines. Falls back to the raw string.
fn local_hhmm_from_iso(iso: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_else(|_| iso.to_string())
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
    use super::*;
    use chrono::{Duration, Local, TimeZone};

    #[test]
    fn codex_subscription_switch_refreshes_ide_runtime() {
        let plan = switch_runtime_plan(&ToolId::Codex, false);

        assert!(plan.sync_codex_ide_credentials);
        assert!(plan.restart_codex_runtime);
    }

    #[test]
    fn codex_api_switch_does_not_replace_subscription_credentials() {
        let plan = switch_runtime_plan(&ToolId::Codex, true);

        assert!(!plan.sync_codex_ide_credentials);
        assert!(!plan.restart_codex_runtime);
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
    fn detects_codex_quota_error_lines() {
        assert!(codex_quota_error_line(
            r#"{"type":"event_msg","payload":{"message":"rate limit exceeded"}}"#
        ));
        assert!(codex_quota_error_line(
            "usage limit reached for this account"
        ));
        assert!(codex_quota_error_line("HTTP 429 quota exhausted"));
        assert!(!codex_quota_error_line("refresh quota button clicked"));
        assert!(!codex_quota_error_line("normal assistant message"));
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
    fn machine_default_is_not_auto_prime_eligible() {
        let mut default_account = test_account("default-codex", AccountState::Idle, 10.0);
        default_account.is_default = true;
        let user_account = test_account("acc-1", AccountState::Idle, 10.0);

        let mut data = StoredState::default();
        data.accounts = vec![default_account, user_account];

        assert!(!prime_eligible_in_state(
            &data,
            &ToolId::Codex,
            "default-codex"
        ));
        assert!(prime_eligible_in_state(&data, &ToolId::Codex, "acc-1"));
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
        }
    }

    /// Build an ISO-8601 (RFC-3339) string for a local instant `offset` from a base, so the tests
    /// are timezone-independent (the helper interprets the anchor in `Local`).
    fn local_iso(base: chrono::DateTime<Local>) -> String {
        base.with_timezone(&chrono::Utc).to_rfc3339()
    }

    /// The local `HH:MM` `mins` minutes after `base` — the anchor a test wants to target.
    fn hhmm_after(base: chrono::DateTime<Local>, mins: i64) -> String {
        (base + Duration::minutes(mins)).format("%H:%M").to_string()
    }

    /// A fixed, unambiguous local base instant (noon avoids DST midnight edge cases).
    fn base() -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 6, 18, 12, 0, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn anchor_inside_window_is_preferred() {
        // Window ends at base; anchor is 90 min later (well within 5h) → prefer the anchor.
        let reset = local_iso(base());
        let anchor_time = hhmm_after(base(), 90);
        let got = scheduled_anchor_within_window(&reset, &anchor_time).expect("anchor within 5h");
        // Fires at the anchor instant, i.e. 90 min after reset.
        let expected = local_iso(base() + Duration::minutes(90));
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(&got).unwrap(),
            chrono::DateTime::parse_from_rfc3339(&expected).unwrap()
        );
    }

    #[test]
    fn anchor_just_before_five_hours_is_preferred() {
        // Anchor 4h59m after the window ends — still inside the 5h the extend would cover.
        let reset = local_iso(base());
        let anchor_time = hhmm_after(base(), 5 * 60 - 1);
        assert!(scheduled_anchor_within_window(&reset, &anchor_time).is_some());
    }

    #[test]
    fn anchor_beyond_five_hours_falls_back() {
        // Anchor 5h01m out → belongs to a later window; don't make the user wait that long.
        let reset = local_iso(base());
        let anchor_time = hhmm_after(base(), 5 * 60 + 1);
        assert!(scheduled_anchor_within_window(&reset, &anchor_time).is_none());
    }

    #[test]
    fn anchor_equal_to_reset_rolls_to_next_day() {
        // Anchor exactly at the window-ending: no gap to wait, so it rolls a full day forward and is
        // therefore >5h away → None (extend at the boundary now).
        let reset = local_iso(base());
        let anchor_time = base().format("%H:%M").to_string();
        assert!(scheduled_anchor_within_window(&reset, &anchor_time).is_none());
    }

    #[test]
    fn invalid_inputs_return_none() {
        assert!(scheduled_anchor_within_window("not-a-date", "11:00").is_none());
        assert!(scheduled_anchor_within_window(&local_iso(base()), "99:99").is_none());
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
