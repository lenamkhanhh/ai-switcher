mod api_gateway;
mod app_state;
mod bulk_login;
mod codex_app_server;
mod codex_import;
mod codex_pet;
mod detection;
mod handoff;
mod models;
mod pricing;
mod quota;
mod seamless;
mod store;
mod tools;
mod tray;
mod usage;
mod vault;

use app_state::ManagedState;
use models::{
    AddAccountInput, AddApiAccountInput, ApiUsageReport, AppSnapshot, BulkImportCodexAccountsInput,
    BulkLoginCodexInput, CreateApiGatewayKeyInput, CreateApiGatewayKeyResult,
    CreateVirtualApiAccountInput,
    DeleteApiGatewayComboInput, DeleteApiGatewayKeyInput, DetectionReport, ImportCodexAccountInput,
    RenameAccountInput, SaveApiGatewayComboInput, SetApiGatewayAccountInput, SetLauncherInput,
    SetToolSetupInput, StartApiGatewayInput, SwitchAccountInput, ToolId, UsageReports,
    VaultCredential, VaultCredentialInput, VaultCredentialSummary, VaultStatus,
};
use tauri::{Emitter, Manager, State};

/// Exit the whole desktop process. The window close button intentionally hides to the tray, so
/// explicit quit actions must use the application exit path instead of closing the main window.
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

#[tauri::command]
async fn load_snapshot(app: tauri::AppHandle) -> Result<AppSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<ManagedState>();
        // App just opened: update accounts that finished logging in while it was closed.
        let _ = state.recheck_pending_logins();
        state.snapshot().map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn refresh_tool(app: tauri::AppHandle, tool_id: ToolId) -> Result<AppSnapshot, String> {
    let app2 = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        app2.state::<ManagedState>()
            .refresh_tool(tool_id, Some(&app2))
            .map_err(display_error)
    })
    .await
    .map_err(|e| e.to_string())?;
    tray::rebuild(&app);
    result
}

#[tauri::command]
async fn refresh_account(
    app: tauri::AppHandle,
    tool_id: ToolId,
    account_id: String,
) -> Result<AppSnapshot, String> {
    let app2 = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        app2.state::<ManagedState>()
            .refresh_single_account(&tool_id, &account_id, Some(&app2))
            .map_err(display_error)
    })
    .await
    .map_err(|e| e.to_string())?;
    tray::rebuild(&app);
    result
}

#[tauri::command]
async fn reauthenticate_account(
    app: tauri::AppHandle,
    tool_id: ToolId,
    account_id: String,
) -> Result<AppSnapshot, String> {
    let worker_app = app.clone();
    let snapshot = tauri::async_runtime::spawn_blocking(move || {
        worker_app
            .state::<ManagedState>()
            .reauthenticate_account(&worker_app, tool_id, &account_id)
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())??;
    tray::rebuild(&app);
    Ok(snapshot)
}

#[tauri::command]
async fn add_account(app: tauri::AppHandle, input: AddAccountInput) -> Result<AppSnapshot, String> {
    let worker_app = app.clone();
    let snapshot = tauri::async_runtime::spawn_blocking(move || {
        worker_app
            .state::<ManagedState>()
            .add_account(&worker_app, input)
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())??;
    tray::rebuild(&app);
    Ok(snapshot)
}

#[tauri::command]
async fn import_codex_account(
    app: tauri::AppHandle,
    input: ImportCodexAccountInput,
) -> Result<AppSnapshot, String> {
    let worker_app = app.clone();
    let snapshot = tauri::async_runtime::spawn_blocking(move || {
        worker_app
            .state::<ManagedState>()
            .import_codex_account(input)
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())??;
    tray::rebuild(&app);
    Ok(snapshot)
}

#[tauri::command]
async fn bulk_import_codex_accounts(
    app: tauri::AppHandle,
    input: BulkImportCodexAccountsInput,
) -> Result<AppSnapshot, String> {
    let worker_app = app.clone();
    let snapshot = tauri::async_runtime::spawn_blocking(move || {
        worker_app
            .state::<ManagedState>()
            .import_codex_accounts_bulk(input)
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())??;
    tray::rebuild(&app);
    Ok(snapshot)
}

#[tauri::command]
async fn bulk_login_codex_staging(
    app: tauri::AppHandle,
    input: BulkLoginCodexInput,
) -> Result<AppSnapshot, String> {
    let resource_dir = app.path().resource_dir().ok();
    let worker_app = app.clone();
    let snapshot = tauri::async_runtime::spawn_blocking(move || {
        worker_app
            .state::<ManagedState>()
            .bulk_login_codex_staging(input, resource_dir)
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())??;
    tray::rebuild(&app);
    Ok(snapshot)
}

#[tauri::command]
async fn add_api_account(
    app: tauri::AppHandle,
    input: AddApiAccountInput,
) -> Result<AppSnapshot, String> {
    let worker_app = app.clone();
    let snapshot = tauri::async_runtime::spawn_blocking(move || {
        worker_app
            .state::<ManagedState>()
            .add_api_account(input)
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())??;
    tray::rebuild(&app);
    Ok(snapshot)
}

/// List the gateway's models (`{base_url}/models`) so the Add dialog can offer a default model +
/// mapping targets. Stateless — just proxies the HTTP call.
#[tauri::command]
async fn fetch_gateway_models(base_url: String, api_key: String) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        tools::fetch_gateway_models(&base_url, &api_key).map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn rename_account(
    app: tauri::AppHandle,
    input: RenameAccountInput,
) -> Result<AppSnapshot, String> {
    let worker_app = app.clone();
    let snapshot = tauri::async_runtime::spawn_blocking(move || {
        worker_app
            .state::<ManagedState>()
            .rename_account(input)
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())??;
    tray::rebuild(&app);
    Ok(snapshot)
}

#[tauri::command]
async fn switch_account(
    app: tauri::AppHandle,
    input: SwitchAccountInput,
) -> Result<AppSnapshot, String> {
    let worker_app = app.clone();
    let snapshot = tauri::async_runtime::spawn_blocking(move || {
        worker_app
            .state::<ManagedState>()
            .switch_account(input)
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())??;
    tray::rebuild(&app);
    Ok(snapshot)
}

#[tauri::command]
async fn retry_resume_job(app: tauri::AppHandle, job_id: String) -> Result<AppSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<ManagedState>()
            .retry_resume_job(&app, &job_id)
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn cancel_resume_job(
    state: State<'_, ManagedState>,
    job_id: String,
) -> Result<AppSnapshot, String> {
    state.cancel_resume_job(&job_id).map_err(display_error)
}

#[tauri::command]
fn open_resume_job(state: State<'_, ManagedState>, job_id: String) -> Result<AppSnapshot, String> {
    state.open_resume_job(&job_id).map_err(display_error)
}

#[tauri::command]
async fn set_launcher(
    app: tauri::AppHandle,
    input: SetLauncherInput,
) -> Result<AppSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<ManagedState>()
            .set_launcher(input)
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn delete_account(
    app: tauri::AppHandle,
    tool_id: ToolId,
    account_id: String,
) -> Result<AppSnapshot, String> {
    let worker_app = app.clone();
    let snapshot = tauri::async_runtime::spawn_blocking(move || {
        worker_app
            .state::<ManagedState>()
            .delete_account(tool_id, account_id)
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())??;
    tray::rebuild(&app);
    Ok(snapshot)
}

#[tauri::command]
fn get_vault_status(state: State<'_, ManagedState>) -> Result<VaultStatus, String> {
    state.vault_status().map_err(display_error)
}

#[tauri::command]
async fn initialize_vault(app: tauri::AppHandle, passkey: String) -> Result<VaultStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<ManagedState>()
            .initialize_vault(passkey)
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn unlock_vault(app: tauri::AppHandle, passkey: String) -> Result<VaultStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<ManagedState>()
            .unlock_vault(passkey)
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn lock_vault(state: State<'_, ManagedState>) {
    state.lock_vault();
}

#[tauri::command]
fn list_vault_credentials(
    state: State<'_, ManagedState>,
) -> Result<Vec<VaultCredentialSummary>, String> {
    state.list_vault_credentials().map_err(display_error)
}

#[tauri::command]
fn get_vault_credential(
    state: State<'_, ManagedState>,
    tool_id: ToolId,
    account_id: String,
) -> Result<Option<VaultCredential>, String> {
    state
        .get_vault_credential(tool_id, account_id)
        .map_err(display_error)
}

#[tauri::command]
fn save_vault_credential(
    state: State<'_, ManagedState>,
    input: VaultCredentialInput,
) -> Result<VaultCredentialSummary, String> {
    state.save_vault_credential(input).map_err(display_error)
}

#[tauri::command]
fn delete_vault_credential(
    state: State<'_, ManagedState>,
    tool_id: ToolId,
    account_id: String,
) -> Result<(), String> {
    state
        .delete_vault_credential(tool_id, account_id)
        .map_err(display_error)
}

#[tauri::command]
fn accept_disclaimer(state: State<'_, ManagedState>) -> Result<AppSnapshot, String> {
    state.accept_disclaimer().map_err(display_error)
}

#[tauri::command]
fn antigravity_new_login(state: State<'_, ManagedState>) -> Result<AppSnapshot, String> {
    state.antigravity_new_login().map_err(display_error)
}

#[tauri::command]
fn set_auto_switch(
    state: State<'_, ManagedState>,
    enabled: bool,
    threshold: f64,
) -> Result<AppSnapshot, String> {
    state
        .set_auto_switch(enabled, threshold)
        .map_err(display_error)
}

#[tauri::command]
fn set_auto_switch_setting(
    state: State<'_, ManagedState>,
    tool_id: ToolId,
    enabled: bool,
    threshold: f64,
) -> Result<AppSnapshot, String> {
    state
        .set_auto_switch_setting(tool_id, enabled, threshold)
        .map_err(display_error)
}

#[tauri::command]
fn set_launch_with_windows(app: tauri::AppHandle, enabled: bool) -> Result<AppSnapshot, String> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    if enabled {
        manager.enable()
    } else {
        manager.disable()
    }
    .map_err(|e| e.to_string())?;
    app.state::<ManagedState>()
        .set_launch_with_windows(enabled)
        .map_err(display_error)
}

#[tauri::command]
fn detect_tool_setup(state: State<'_, ManagedState>, tool_id: ToolId) -> DetectionReport {
    state.detect_tool_setup(tool_id)
}

#[tauri::command]
fn validate_tool_setup(
    state: State<'_, ManagedState>,
    input: SetToolSetupInput,
) -> DetectionReport {
    state.validate_tool_setup(input)
}

#[tauri::command]
async fn set_tool_setup(
    app: tauri::AppHandle,
    input: SetToolSetupInput,
) -> Result<AppSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<ManagedState>()
            .set_tool_setup(input)
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Token usage + cost report for the Usage tab (Claude + Codex, aggregated per tool).
/// `range_days` limits the totals to the last N local days (0 = all time).
#[tauri::command]
async fn get_usage(app: tauri::AppHandle, range_days: u32) -> Result<UsageReports, String> {
    tauri::async_runtime::spawn_blocking(move || {
        Ok(app.state::<ManagedState>().usage_reports(range_days))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn get_api_usage(state: State<'_, ManagedState>) -> ApiUsageReport {
    state.api_usage_report()
}

#[tauri::command]
async fn start_api_gateway(
    app: tauri::AppHandle,
    input: StartApiGatewayInput,
) -> Result<AppSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<ManagedState>()
            .start_api_gateway(input)
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn stop_api_gateway(app: tauri::AppHandle) -> Result<AppSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<ManagedState>()
            .stop_api_gateway()
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn create_api_gateway_key(
    state: State<'_, ManagedState>,
    input: CreateApiGatewayKeyInput,
) -> Result<CreateApiGatewayKeyResult, String> {
    state.create_api_gateway_key(input).map_err(display_error)
}

#[tauri::command]
fn delete_api_gateway_key(
    state: State<'_, ManagedState>,
    input: DeleteApiGatewayKeyInput,
) -> Result<AppSnapshot, String> {
    state.delete_api_gateway_key(input).map_err(display_error)
}

#[tauri::command]
fn reveal_api_gateway_key(
    state: State<'_, ManagedState>,
    key_id: String,
) -> Result<String, String> {
    state.reveal_api_gateway_key(key_id).map_err(display_error)
}

#[tauri::command]
fn save_api_gateway_combo(
    state: State<'_, ManagedState>,
    input: SaveApiGatewayComboInput,
) -> Result<AppSnapshot, String> {
    state.save_api_gateway_combo(input).map_err(display_error)
}

#[tauri::command]
fn delete_api_gateway_combo(
    state: State<'_, ManagedState>,
    input: DeleteApiGatewayComboInput,
) -> Result<AppSnapshot, String> {
    state.delete_api_gateway_combo(input).map_err(display_error)
}

#[tauri::command]
fn set_api_gateway_account(
    state: State<'_, ManagedState>,
    input: SetApiGatewayAccountInput,
) -> Result<AppSnapshot, String> {
    state.set_api_gateway_account(input).map_err(display_error)
}

#[tauri::command]
async fn refresh_api_gateway_models(app: tauri::AppHandle) -> Result<AppSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<ManagedState>()
            .refresh_api_gateway_models()
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn create_virtual_api_account(
    app: tauri::AppHandle,
    input: CreateVirtualApiAccountInput,
) -> Result<AppSnapshot, String> {
    let worker_app = app.clone();
    let snapshot = tauri::async_runtime::spawn_blocking(move || {
        worker_app
            .state::<ManagedState>()
            .create_virtual_api_account(input)
            .map_err(display_error)
    })
    .await
    .map_err(|error| error.to_string())??;
    tray::rebuild(&app);
    Ok(snapshot)
}

pub fn run() {
    let state = ManagedState::new().expect("failed to initialize app state");
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            load_snapshot,
            quit_app,
            refresh_tool,
            refresh_account,
            reauthenticate_account,
            add_account,
            import_codex_account,
            bulk_import_codex_accounts,
            bulk_login_codex_staging,
            add_api_account,
            fetch_gateway_models,
            rename_account,
            switch_account,
            retry_resume_job,
            cancel_resume_job,
            open_resume_job,
            set_launcher,
            delete_account,
            get_vault_status,
            initialize_vault,
            unlock_vault,
            lock_vault,
            list_vault_credentials,
            get_vault_credential,
            save_vault_credential,
            delete_vault_credential,
            accept_disclaimer,
            antigravity_new_login,
            set_auto_switch,
            set_auto_switch_setting,
            set_launch_with_windows,
            detect_tool_setup,
            validate_tool_setup,
            set_tool_setup,
            get_usage,
            get_api_usage,
            start_api_gateway,
            stop_api_gateway,
            create_api_gateway_key,
            delete_api_gateway_key,
            reveal_api_gateway_key,
            save_api_gateway_combo,
            delete_api_gateway_combo,
            set_api_gateway_account,
            refresh_api_gateway_models,
            create_virtual_api_account
        ])
        .setup(|app| {
            // Complete only the read-before-retry portion of any recovery that survived a
            // process restart. No new turn is sent until a fresh exact ACK can be observed.
            let startup_handle = app.handle().clone();
            let _ = startup_handle
                .state::<ManagedState>()
                .heal_resume_jobs(Some(&startup_handle));
            // Menu-bar (tray) icon for quick account switching without opening the window.
            tray::create(app.handle())?;
            // Codex's optional desktop pet is a separate overlay window. Repair only its
            // hit-test/layout state; this does not touch account state, auth files, or reload flow.
            codex_pet::repair_codex_pet_input();
            codex_pet::start_pet_input_monitor();
            // On Windows the WebView process can start while the main window remains hidden.
            // Explicitly restore it so the first Desktop-shortcut click always opens the UI.
            #[cfg(target_os = "windows")]
            if let Some(window) = app.get_webview_window("main") {
                // Set the window icon so the taskbar shows the custom logo
                // (bundle.icon only applies to the built .exe, not dev mode).
                if let Ok(icon) =
                    tauri::image::Image::from_bytes(include_bytes!("../icons/icon.png"))
                {
                    let _ = window.set_icon(icon);
                }
                let startup_hidden = std::env::args().any(|arg| arg == "--hidden");
                if startup_hidden {
                    let _ = window.hide();
                } else {
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
            }

            // Background poller: periodically refresh quota + auto-switch if enabled.
            // Refresh every 5 minutes (Claude's quota endpoint is rate-limited hard;
            // the 5h/weekly quota changes slowly, so no need to poll more often).
            let handle = app.handle().clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(300));
                let state = handle.state::<ManagedState>();
                for tool_id in [ToolId::Claude, ToolId::Codex] {
                    let _ = state.refresh_tool(tool_id, Some(&handle));
                }
                // Keep the token-usage cache warm and nudge any open Usage tab to refetch
                // (with whatever range the user has selected).
                let _ = state.usage_report(0);
                let _ = handle.emit("usage-changed", ());
                // Refresh the tray menu's quota %/checkmarks with the new snapshot.
                tray::rebuild(&handle);
            });

            // Watch active Codex rollouts for quota errors so seamless auto-switch can react
            // without generating synthetic traffic.
            let codex_watch_handle = app.handle().clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(10));
                let state = codex_watch_handle.state::<ManagedState>();
                let _ = state.heal_resume_jobs(Some(&codex_watch_handle));
                let _ = state.check_codex_quota_error_and_auto_switch(Some(&codex_watch_handle));
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            // Close (✕) hides the main window to the tray instead of quitting — the poller and
            // tray stay alive so quick-switch keeps working. Quit is via the tray menu.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    window.app_handle().state::<ManagedState>().lock_vault();
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                let state = app.state::<ManagedState>();
                state.lock_vault();
                let _ = state.stop_api_gateway();
            }
            #[cfg(target_os = "macos")]
            // Clicking the Dock icon while the window is hidden (closed to tray) reopens it.
            if let tauri::RunEvent::Reopen { .. } = event {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        });
}

fn display_error(error: anyhow::Error) -> String {
    let text = error.to_string();
    if text.contains("Secure vault is locked") {
        "Secure vault is locked — unlock it first".to_string()
    } else if text.contains("Vault could not be unlocked") {
        "Secure vault could not be unlocked — check the passkey".to_string()
    } else if text.contains("Secure vault is already configured") {
        "Secure vault is already configured".to_string()
    } else if text.contains("Secure vault") {
        "Secure vault operation failed — try again".to_string()
    } else if text.contains("Failed to switch account") {
        "Failed to switch account — kept the previous account".to_string()
    } else if text.contains("Login not completed") {
        "Login not completed, account not added".to_string()
    } else if text.contains("No login") {
        "No login found for this tool".to_string()
    } else {
        text
    }
}
