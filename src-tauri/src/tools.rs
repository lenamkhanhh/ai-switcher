use crate::models::{CodexLoginMethod, StartupIntegrationMode, ToolId};
use crate::store::Store;
use anyhow::{Context, Result};
use directories::ProjectDirs;
use keyring::Entry;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(windows)]
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::{AppHandle, Emitter};

const APP_KEYCHAIN_SERVICE: &str = "AI Account Switcher";
#[cfg(windows)]
static NEXT_LOGIN_ATTEMPT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn hide_command_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DeviceLoginPrompt {
    login_url: Option<String>,
    device_code: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AppServerLoginSignal {
    Initialized,
    Completed(bool),
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LoginPromptEvent {
    tool_id: ToolId,
    account_id: String,
    attempt_id: u64,
    login_url: Option<String>,
    device_code: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct LoginPromptFinishedEvent {
    tool_id: ToolId,
    account_id: String,
    attempt_id: u64,
    success: bool,
}

impl LoginPromptFinishedEvent {
    fn new(
        tool_id: ToolId,
        account_id: impl Into<String>,
        attempt_id: u64,
        success: bool,
    ) -> Self {
        Self {
            tool_id,
            account_id: account_id.into(),
            attempt_id,
            success,
        }
    }
}

/// The tool's default config dir (when the account isn't profile-mode, or is
/// the "Default" account pointing at the machine's original session).
pub fn default_config_dir(tool_id: &ToolId) -> PathBuf {
    let home = home_dir();
    match tool_id {
        ToolId::Claude => home.join(".claude"),
        ToolId::Codex => home.join(".codex"),
        // Antigravity IDE: the machine's default userData on macOS (original login).
        ToolId::Antigravity => {
            if cfg!(windows) {
                home.join("AppData/Roaming/Antigravity IDE")
            } else {
                home.join("Library/Application Support/Antigravity IDE")
            }
        }
    }
}

fn codex_auth_account_id(path: &Path) -> Option<String> {
    let value: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    value
        .get("tokens")?
        .get("account_id")?
        .as_str()
        .map(str::to_string)
}

fn parse_device_login_prompt(text: &str) -> DeviceLoginPrompt {
    let clean = strip_terminal_control_sequences(text);
    let mut prompt = DeviceLoginPrompt::default();
    for raw in clean.split_whitespace() {
        let token = raw.trim_matches(|ch: char| {
            matches!(
                ch,
                '"' | '\'' | '`' | ',' | ';' | ':' | ')' | '(' | '[' | ']'
            )
        });
        if token.starts_with("https://auth.openai.com/") {
            prompt.login_url = Some(token.to_string());
        } else if is_device_code_token(token) {
            prompt.device_code = Some(token.to_string());
        }
    }
    prompt
}

/// The Codex app-server exposes device-login details as JSON-RPC instead of
/// terminal text. Prefer it when available so the UI does not depend on ANSI
/// rendering or a shell wrapper forwarding stdout correctly.
fn parse_app_server_device_login_message(text: &str) -> DeviceLoginPrompt {
    let Ok(message) = serde_json::from_str::<Value>(text) else {
        return DeviceLoginPrompt::default();
    };
    let Some(result) = message.get("result") else {
        return DeviceLoginPrompt::default();
    };
    if result.get("type").and_then(Value::as_str) != Some("chatgptDeviceCode") {
        return DeviceLoginPrompt::default();
    }
    let login_url = result
        .get("verificationUrl")
        .and_then(Value::as_str)
        .filter(|url| url.starts_with("https://auth.openai.com/"))
        .map(str::to_string);
    let device_code = result
        .get("userCode")
        .and_then(Value::as_str)
        .filter(|code| is_device_code_token(code))
        .map(str::to_string);
    DeviceLoginPrompt {
        login_url,
        device_code,
    }
}

fn parse_app_server_login_signal(text: &str) -> Option<AppServerLoginSignal> {
    let message = serde_json::from_str::<Value>(text).ok()?;
    if message.get("id").and_then(Value::as_u64) == Some(1) && message.get("result").is_some() {
        return Some(AppServerLoginSignal::Initialized);
    }
    if message.get("method").and_then(Value::as_str) != Some("account/login/completed") {
        return None;
    }
    message
        .get("params")?
        .get("success")?
        .as_bool()
        .map(AppServerLoginSignal::Completed)
}

/// Codex CLI 0.141 implements the v1 acknowledgement as an empty notification.
/// Supplying a `params` object makes that version close the stdio connection
/// before it accepts the device-code request.
fn app_server_initialized_message() -> Value {
    serde_json::json!({ "method": "initialized" })
}

/// Codex renders its device-login instructions as an interactive terminal UI on
/// Windows. The device code can therefore be wrapped in ANSI styling rather
/// than appearing as a plain whitespace-delimited token.
fn strip_terminal_control_sequences(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            clean.push(ch);
            continue;
        }

        match chars.next() {
            Some('[') => {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) | None => {}
        }
    }
    clean
}

fn is_device_code_token(value: &str) -> bool {
    let mut has_dash = false;
    let mut alnum = 0;
    for ch in value.chars() {
        if ch == '-' {
            has_dash = true;
            continue;
        }
        if ch.is_ascii_uppercase() || ch.is_ascii_digit() {
            alnum += 1;
            continue;
        }
        return false;
    }
    has_dash && (6..=16).contains(&alnum)
}

fn copy_codex_auth(source: &Path, destination: &Path) -> Result<()> {
    let bytes = fs::read(source)
        .with_context(|| format!("Couldn't read Codex credentials at {}", source.display()))?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(destination, bytes).with_context(|| {
        format!(
            "Couldn't update Codex credentials at {}",
            destination.display()
        )
    })
}

fn save_machine_default_codex_auth(store: &Store, default_auth: &Path) -> Result<()> {
    let backup = store.codex_machine_default_auth_backup();
    let missing_marker = store.codex_machine_default_auth_missing_marker();
    if default_auth.exists() {
        copy_codex_auth(default_auth, &backup)?;
        let _ = fs::remove_file(missing_marker);
    } else {
        if let Some(parent) = missing_marker.parent() {
            fs::create_dir_all(parent)?;
        }
        let _ = fs::remove_file(backup);
        fs::write(missing_marker, b"")?;
    }
    Ok(())
}

/// Keep the default Codex credential file aligned with the selected account.
///
/// The VS Code extension and the normal CLI share `~/.codex/auth.json`; profile launchers use
/// `CODEX_HOME` and therefore do not affect an already-running IDE extension. Before replacing the
/// default file, save any refreshed token back to the outgoing profile and preserve the original
/// machine-default credential so selecting "Machine default" remains reversible.
pub fn sync_codex_ide_credentials(
    store: &Store,
    target_account_id: &str,
    target_is_default: bool,
    default_config_dir: &Path,
) -> Result<()> {
    let default_auth = default_config_dir.join("auth.json");
    let machine_backup = store.codex_machine_default_auth_backup();
    let missing_marker = store.codex_machine_default_auth_missing_marker();
    let active_profile = fs::read_to_string(store.active_profile_path(&ToolId::Codex))
        .ok()
        .map(|value| PathBuf::from(value.trim()))
        .filter(|path| !path.as_os_str().is_empty());

    if let Some(outgoing_profile) = active_profile.as_ref() {
        let outgoing_auth = outgoing_profile.join("auth.json");
        if codex_auth_account_id(&default_auth).is_some()
            && codex_auth_account_id(&default_auth) == codex_auth_account_id(&outgoing_auth)
        {
            copy_codex_auth(&default_auth, &outgoing_auth)?;
        }
    }

    if !machine_backup.exists() && !missing_marker.exists() {
        save_machine_default_codex_auth(store, &default_auth)?;
    } else if active_profile.is_none() && !target_is_default {
        // The machine-default account was active, so retain any token refresh before leaving it.
        save_machine_default_codex_auth(store, &default_auth)?;
    }

    if target_is_default && missing_marker.exists() {
        let _ = fs::remove_file(default_auth);
        return Ok(());
    }

    let target_auth = if target_is_default {
        // Returning to (or, on every app start, staying on) the machine default. If the live
        // `~/.codex/auth.json` already holds the machine-default account, the CLI/IDE may have
        // rotated its tokens since the backup was taken — restoring the backup here would roll
        // those away and eventually force a re-login. Keep the live file and refresh the backup
        // from it instead; only fall back to the backup when the live file belongs to a different
        // (profile) account that must be swapped out.
        let backup_account = codex_auth_account_id(&machine_backup);
        let live_account = codex_auth_account_id(&default_auth);
        if live_account.is_some() && live_account == backup_account {
            save_machine_default_codex_auth(store, &default_auth)?;
            return Ok(());
        }
        machine_backup
    } else {
        store
            .account_dir(&ToolId::Codex, target_account_id)
            .join("auth.json")
    };
    copy_codex_auth(&target_auth, &default_auth)
}

#[cfg(test)]
fn is_codex_runtime_process(name: &str, command_line: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let command_line = command_line.to_ascii_lowercase();
    if !command_line.contains("app-server") {
        return false;
    }
    name.contains("codex")
        || ((name == "node.exe" || name == "node")
            && (command_line.contains("@openai")
                || command_line.contains("\\codex")
                || command_line.contains("/codex")))
}

/// Stop stale Codex runtimes that cache the old `~/.codex/auth.json`.
///
/// VS Code will spawn a fresh app-server the next time the Codex pane/thread needs it. This is
/// intentionally best-effort and narrow: never stop VS Code itself, only Codex app-server workers.
pub fn restart_codex_runtime_after_switch() {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let script = r#"
$ErrorActionPreference = 'SilentlyContinue'
Get-CimInstance Win32_Process | Where-Object {
  $name = if ($_.Name) { $_.Name.ToLowerInvariant() } else { '' }
  $cmd = if ($_.CommandLine) { $_.CommandLine.ToLowerInvariant() } else { '' }
  $cmd.Contains('app-server') -and (
    $name.Contains('codex') -or
    (($name -eq 'node.exe' -or $name -eq 'node') -and ($cmd.Contains('@openai') -or $cmd.Contains('\codex') -or $cmd.Contains('/codex')))
  )
} | ForEach-Object {
  Stop-Process -Id $_.ProcessId -Force
}
"#;
        let _ = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                script,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }

    #[cfg(not(windows))]
    {
        let _ = Command::new("pkill")
            .args(["-f", "codex.*app-server|app-server.*codex"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Best-effort refresh for the visible Codex frontends after a subscription-account switch.
///
/// Switching credentials updates `~/.codex/auth.json`, but already-open VS Code/Codex desktop
/// windows can keep an error page or a stale app-server session. This helper intentionally runs
/// after the switch is persisted, throttles itself with a local stamp file, then:
/// - reloads the active VS Code window through the command palette
/// - restarts the ChatGPT Codex desktop app only if it is already running
#[cfg(windows)]
fn codex_frontend_refresh_script() -> &'static str {
    r#"
$ErrorActionPreference = 'SilentlyContinue'
$root = Join-Path $env:LOCALAPPDATA 'hoangphan\AI Account Switcher\data'
$stamp = Join-Path $root 'codex-frontend-refresh.stamp'
$result = Join-Path $root 'codex-frontend-refresh.result'
$maxReloadAttempts = 3
New-Item -ItemType Directory -Path $root -Force | Out-Null
if (Test-Path $stamp) {
  $age = (Get-Date) - (Get-Item $stamp).LastWriteTime
  # Coalesce duplicate events from the same click, but never suppress a legitimate
  # account change for several minutes.
  if ($age.TotalSeconds -lt 10) {
    Set-Content -LiteralPath $result -Value "skipped duplicate at $((Get-Date).ToString('o'))" -Encoding ASCII
    exit 0
  }
}
Set-Content -LiteralPath $stamp -Value (Get-Date).ToString('o') -Encoding ASCII
Start-Sleep -Milliseconds 1200

function Get-VsCodeChildIds([int]$parentId) {
  @(Get-CimInstance Win32_Process | Where-Object {
    $_.Name -eq 'Code.exe' -and $_.ParentProcessId -eq $parentId
  } | Select-Object -ExpandProperty ProcessId | Sort-Object)
}

function Wait-VsCodeReload([int]$parentId, [object[]]$beforeIds) {
  $deadline = (Get-Date).AddSeconds(12)
  do {
    $currentIds = Get-VsCodeChildIds $parentId
    if (@($currentIds | Where-Object { $beforeIds -notcontains $_ }).Count -gt 0) {
      return $true
    }
    Start-Sleep -Milliseconds 250
  } while ((Get-Date) -lt $deadline)
  return $false
}

function Paste-ExactVsCodeCommand {
  $commandText = 'Developer: Reload Window'
  $previousClipboard = $null
  $clipboardReady = $false
  for ($clipboardAttempt = 1; $clipboardAttempt -le 3 -and -not $clipboardReady; $clipboardAttempt++) {
    try {
      $previousClipboard = [System.Windows.Forms.Clipboard]::GetDataObject()
      [System.Windows.Forms.Clipboard]::SetText('Developer: Reload Window')
      $clipboardReady = $true
    } catch {
      Start-Sleep -Milliseconds 100
    }
  }
  if (-not $clipboardReady) { return $false }

  # Pasting bypasses Vietnamese IME key translation (for example, Window -> ưindow).
  [System.Windows.Forms.SendKeys]::SendWait('^v')
  Start-Sleep -Milliseconds 250

  # Restore only while the clipboard still contains our temporary command. If the user copied
  # something else during the short paste window, keep their newer clipboard value instead.
  try {
    $stillOurs = [System.Windows.Forms.Clipboard]::ContainsText() -and
      [System.Windows.Forms.Clipboard]::GetText() -eq $commandText
    if ($stillOurs) {
      if ($null -eq $previousClipboard) {
        [System.Windows.Forms.Clipboard]::Clear()
      } else {
        [System.Windows.Forms.Clipboard]::SetDataObject($previousClipboard, $true)
      }
    }
  } catch {}
  return $true
}

$code = Get-Process Code | Where-Object { $_.MainWindowHandle -ne 0 -and $_.MainWindowTitle } | Sort-Object StartTime -Descending | Select-Object -First 1
$reloadSucceeded = $false
if ($code) {
  Add-Type -AssemblyName Microsoft.VisualBasic
  Add-Type -AssemblyName System.Windows.Forms
  Add-Type -Namespace Aisw -Name Fg -MemberDefinition '[DllImport("user32.dll")] public static extern System.IntPtr GetForegroundWindow();'
  for ($attempt = 1; $attempt -le $maxReloadAttempts -and -not $reloadSucceeded; $attempt++) {
    $beforeIds = Get-VsCodeChildIds $code.Id
    [Microsoft.VisualBasic.Interaction]::AppActivate($code.Id) | Out-Null
    Start-Sleep -Milliseconds 600
    if ([Aisw.Fg]::GetForegroundWindow() -eq $code.MainWindowHandle) {
      # F1 opens the command palette directly. Longer waits avoid pressing Enter before
      # a busy extension host has produced the Reload Window search result.
      [System.Windows.Forms.SendKeys]::SendWait('{ESC}')
      [System.Windows.Forms.SendKeys]::SendWait('{F1}')
      Start-Sleep -Milliseconds 1000
      if (Paste-ExactVsCodeCommand) {
        Start-Sleep -Milliseconds 1500
        [System.Windows.Forms.SendKeys]::SendWait('{ENTER}')
        $reloadSucceeded = Wait-VsCodeReload $code.Id $beforeIds
      }
    }
    if (-not $reloadSucceeded) { Start-Sleep -Milliseconds 750 }
  }
}

if ($reloadSucceeded) {
  Set-Content -LiteralPath $result -Value "reloaded at $((Get-Date).ToString('o'))" -Encoding ASCII
} elseif ($code) {
  Set-Content -LiteralPath $result -Value "reload failed after $maxReloadAttempts attempts at $((Get-Date).ToString('o'))" -Encoding ASCII
} else {
  Set-Content -LiteralPath $result -Value "no VS Code window at $((Get-Date).ToString('o'))" -Encoding ASCII
}

$chat = Get-Process ChatGPT | Where-Object { $_.Path -like '*OpenAI.Codex_*' } | Select-Object -First 1
if ($chat) {
  Get-Process ChatGPT | Where-Object { $_.Path -like '*OpenAI.Codex_*' } | Stop-Process -Force
  Start-Sleep -Seconds 2
  Start-Process 'shell:AppsFolder\OpenAI.Codex_2p2nqsd0c76g0!App'
}
"#
}

pub fn refresh_codex_frontends_after_switch() {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let script = codex_frontend_refresh_script();
        let _ = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                script,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
}

/// Give the interactive `codex resume` TUI a moment to load, then request the same session in
/// the Codex desktop app. This is deliberately best-effort: if the desktop app is unavailable,
/// the visible terminal remains the fallback surface instead of losing the resumed task.
pub fn open_codex_session_in_app_after_resume(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let script = format!(
            r#"
$ErrorActionPreference = 'SilentlyContinue'
Start-Sleep -Seconds 4
Add-Type -AssemblyName Microsoft.VisualBasic
Add-Type -AssemblyName System.Windows.Forms
Add-Type -Namespace Aisw -Name Fg -MemberDefinition '[DllImport("user32.dll")] public static extern System.IntPtr GetForegroundWindow();'
$p = Get-Process -Id {pid}
if ($p -and $p.MainWindowHandle -ne 0) {{
  [Microsoft.VisualBasic.Interaction]::AppActivate($p.Id) | Out-Null
  Start-Sleep -Milliseconds 250
  # Only type into the resume TUI if it actually became foreground; otherwise the keystrokes land
  # in whatever window the user is currently focused on.
  if ([Aisw.Fg]::GetForegroundWindow() -eq $p.MainWindowHandle) {{
    [System.Windows.Forms.SendKeys]::SendWait('/app')
    [System.Windows.Forms.SendKeys]::SendWait('{{ENTER}}')
  }}
}}
"#
        );
        let _ = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &script,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
    }
}

#[cfg(test)]
mod codex_ide_credential_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("aisw-{name}-{unique}"))
    }

    fn auth(account_id: &str, marker: &str) -> Vec<u8> {
        format!(
            r#"{{"auth_mode":"chatgpt","tokens":{{"account_id":"{account_id}","access_token":"{marker}"}}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn switching_to_profile_updates_default_codex_auth_and_preserves_machine_default() {
        let root = temp_root("codex-ide-profile");
        let store = Store::for_test(root.clone()).unwrap();
        let default_dir = root.join("default");
        let profile = store.account_dir(&ToolId::Codex, "acc2");
        fs::create_dir_all(&default_dir).unwrap();
        fs::create_dir_all(&profile).unwrap();
        fs::write(
            default_dir.join("auth.json"),
            auth("machine", "machine-token"),
        )
        .unwrap();
        fs::write(profile.join("auth.json"), auth("acc2", "acc2-token")).unwrap();

        sync_codex_ide_credentials(&store, "acc2", false, &default_dir).unwrap();

        assert_eq!(
            fs::read(default_dir.join("auth.json")).unwrap(),
            auth("acc2", "acc2-token")
        );
        assert_eq!(
            fs::read(store.codex_machine_default_auth_backup()).unwrap(),
            auth("machine", "machine-token")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn switching_profiles_saves_refreshed_outgoing_auth_before_replacing_it() {
        let root = temp_root("codex-ide-refresh");
        let store = Store::for_test(root.clone()).unwrap();
        let default_dir = root.join("default");
        let acc1 = store.account_dir(&ToolId::Codex, "acc1");
        let acc2 = store.account_dir(&ToolId::Codex, "acc2");
        fs::create_dir_all(&default_dir).unwrap();
        fs::create_dir_all(&acc1).unwrap();
        fs::create_dir_all(&acc2).unwrap();
        fs::write(default_dir.join("auth.json"), auth("acc1", "refreshed")).unwrap();
        fs::write(acc1.join("auth.json"), auth("acc1", "stale")).unwrap();
        fs::write(acc2.join("auth.json"), auth("acc2", "target")).unwrap();
        fs::create_dir_all(store.active_profile_path(&ToolId::Codex).parent().unwrap()).unwrap();
        fs::write(
            store.active_profile_path(&ToolId::Codex),
            acc1.to_string_lossy().as_bytes(),
        )
        .unwrap();

        sync_codex_ide_credentials(&store, "acc2", false, &default_dir).unwrap();

        assert_eq!(
            fs::read(acc1.join("auth.json")).unwrap(),
            auth("acc1", "refreshed")
        );
        assert_eq!(
            fs::read(default_dir.join("auth.json")).unwrap(),
            auth("acc2", "target")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn switching_back_to_default_restores_backed_up_auth() {
        let root = temp_root("codex-ide-default");
        let store = Store::for_test(root.clone()).unwrap();
        let default_dir = root.join("default");
        fs::create_dir_all(&default_dir).unwrap();
        fs::create_dir_all(store.codex_machine_default_auth_backup().parent().unwrap()).unwrap();
        fs::write(default_dir.join("auth.json"), auth("acc2", "acc2-token")).unwrap();
        fs::write(
            store.codex_machine_default_auth_backup(),
            auth("machine", "machine-token"),
        )
        .unwrap();

        sync_codex_ide_credentials(&store, "default-codex", true, &default_dir).unwrap();

        assert_eq!(
            fs::read(default_dir.join("auth.json")).unwrap(),
            auth("machine", "machine-token")
        );
        let _ = fs::remove_dir_all(root);
    }

    /// On every app start the heal runs with the machine default selected. If the live default
    /// file still belongs to the machine-default account, its rotated tokens must be kept (and
    /// captured into the backup), never rolled back to a stale backup.
    #[test]
    fn staying_on_default_keeps_rotated_tokens_and_refreshes_the_backup() {
        let root = temp_root("codex-ide-rotate");
        let store = Store::for_test(root.clone()).unwrap();
        let default_dir = root.join("default");
        fs::create_dir_all(&default_dir).unwrap();
        fs::create_dir_all(store.codex_machine_default_auth_backup().parent().unwrap()).unwrap();
        // An older backup for the same machine account…
        fs::write(
            store.codex_machine_default_auth_backup(),
            auth("machine", "old-token"),
        )
        .unwrap();
        // …and the live file the CLI has since rotated to a new token.
        fs::write(
            default_dir.join("auth.json"),
            auth("machine", "rotated-token"),
        )
        .unwrap();

        sync_codex_ide_credentials(&store, "default-codex", true, &default_dir).unwrap();

        assert_eq!(
            fs::read(default_dir.join("auth.json")).unwrap(),
            auth("machine", "rotated-token"),
            "the rotated live token must be kept, not overwritten by the stale backup"
        );
        assert_eq!(
            fs::read(store.codex_machine_default_auth_backup()).unwrap(),
            auth("machine", "rotated-token"),
            "the backup must be refreshed from the live file"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn profile_switch_works_when_machine_default_has_never_logged_in() {
        let root = temp_root("codex-ide-no-default");
        let store = Store::for_test(root.clone()).unwrap();
        let default_dir = root.join("default");
        let profile = store.account_dir(&ToolId::Codex, "acc2");
        fs::create_dir_all(&default_dir).unwrap();
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("auth.json"), auth("acc2", "acc2-token")).unwrap();

        sync_codex_ide_credentials(&store, "acc2", false, &default_dir).unwrap();
        assert_eq!(
            fs::read(default_dir.join("auth.json")).unwrap(),
            auth("acc2", "acc2-token")
        );

        sync_codex_ide_credentials(&store, "default-codex", true, &default_dir).unwrap();
        assert!(!default_dir.join("auth.json").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn codex_runtime_matcher_only_targets_app_server_processes() {
        assert!(is_codex_runtime_process(
            r#"C:\Users\HP\AppData\Roaming\npm\node_modules\@openai\codex\bin\codex.exe"#,
            r#""C:\Users\HP\AppData\Roaming\npm\codex.cmd" app-server --stdio"#
        ));
        assert!(is_codex_runtime_process(
            "codex.exe",
            "codex app-server --stdio"
        ));
        assert!(!is_codex_runtime_process("codex.exe", "codex"));
        assert!(!is_codex_runtime_process(
            "Code.exe",
            r#""C:\Users\HP\AppData\Local\Programs\Microsoft VS Code\Code.exe""#
        ));
        assert!(!is_codex_runtime_process(
            "ChatGPT.exe",
            r#""C:\Program Files\WindowsApps\OpenAI.Codex_26.709.0.0_x64__2p2nqsd0c76g0\ChatGPT.exe""#
        ));
    }

    #[cfg(windows)]
    #[test]
    fn vscode_refresh_waits_for_a_real_reload_and_retries_when_the_palette_misses() {
        let script = codex_frontend_refresh_script();

        assert!(script.contains("$maxReloadAttempts = 3"));
        assert!(script.contains("Wait-VsCodeReload"));
        assert!(script.contains("{F1}"));
        assert!(script.contains("codex-frontend-refresh.result"));
        assert!(script.contains("$age.TotalSeconds -lt 10"));
        assert!(!script.contains("$age.TotalMinutes -lt 3"));
    }

    #[cfg(windows)]
    #[test]
    fn vscode_refresh_pastes_the_exact_command_without_ime_key_translation() {
        let script = codex_frontend_refresh_script();

        assert!(script.contains("[System.Windows.Forms.Clipboard]::GetDataObject()"));
        assert!(script.contains(
            "[System.Windows.Forms.Clipboard]::SetText('Developer: Reload Window')"
        ));
        assert!(script.contains("[System.Windows.Forms.SendKeys]::SendWait('^v')"));
        assert!(script.contains("[System.Windows.Forms.Clipboard]::SetDataObject("));
        assert!(!script.contains(
            "[System.Windows.Forms.SendKeys]::SendWait('Developer: Reload Window')"
        ));
    }
}

pub fn current_platform() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

pub fn default_startup_integration_mode(tool_id: &ToolId) -> Option<StartupIntegrationMode> {
    match tool_id {
        ToolId::Codex | ToolId::Claude => Some(if cfg!(windows) {
            StartupIntegrationMode::PowerShellProfile
        } else {
            StartupIntegrationMode::ShellProfile
        }),
        ToolId::Antigravity => None,
    }
}

fn app_data_local_dir() -> PathBuf {
    ProjectDirs::from("dev", "hoangphan", "AI Account Switcher")
        .map(|dirs| dirs.data_local_dir().to_path_buf())
        .unwrap_or_else(home_dir)
}

fn codex_device_auth_url() -> &'static str {
    "https://auth.openai.com/codex/device"
}

fn chrome_binary_path() -> Option<PathBuf> {
    let candidates = [
        PathBuf::from("C:/Program Files/Google/Chrome/Application/chrome.exe"),
        PathBuf::from("C:/Program Files (x86)/Google/Chrome/Application/chrome.exe"),
        home_dir().join("AppData/Local/Google/Chrome/Application/chrome.exe"),
    ];
    candidates
        .into_iter()
        .find(|path| path.exists())
        .or_else(|| command_path("chrome"))
}

fn isolated_codex_login_chrome_args(user_data_dir: &Path, url: &str) -> Vec<String> {
    vec![
        format!("--user-data-dir={}", user_data_dir.to_string_lossy()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--new-window".into(),
        url.into(),
    ]
}

fn open_codex_in_isolated_browser_profile(store: &Store, account_id: &str, url: &str) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let Some(chrome) = chrome_binary_path() else {
        return false;
    };
    let browser_profile = store
        .account_dir(&ToolId::Codex, account_id)
        .join("login-browser");
    if fs::create_dir_all(&browser_profile).is_err() {
        return false;
    }
    Command::new(chrome)
        .args(isolated_codex_login_chrome_args(&browser_profile, url))
        .spawn()
        .is_ok()
}

fn open_codex_device_auth_in_isolated_profile(store: &Store, account_id: &str) -> bool {
    open_codex_in_isolated_browser_profile(store, account_id, codex_device_auth_url())
}

/// Standard browser OAuth is controlled by the Codex CLI. This opens the
/// persistent per-account Chrome profile first without changing OS browser
/// associations or handling the OAuth callback ourselves.
fn open_codex_browser_login_profile(store: &Store, account_id: &str) -> bool {
    open_codex_in_isolated_browser_profile(store, account_id, "https://chatgpt.com/")
}

fn codex_login_args(method: CodexLoginMethod) -> Vec<&'static str> {
    match method {
        CodexLoginMethod::DeviceCode => vec!["login", "--device-auth"],
        CodexLoginMethod::BrowserProfile => vec!["login"],
    }
}

fn codex_reauth_marker(profile: &Path) -> PathBuf {
    profile.join(".reauth-pending")
}

pub fn mark_codex_reauth_pending(profile: &Path) -> Result<()> {
    let auth = fs::read(profile.join("auth.json")).unwrap_or_default();
    let digest = Sha256::digest(&auth);
    fs::write(codex_reauth_marker(profile), format!("{digest:x}"))?;
    Ok(())
}

pub fn clear_codex_reauth_pending(profile: &Path) {
    let _ = fs::remove_file(codex_reauth_marker(profile));
}

pub fn sync_reauthenticated_codex_credentials(profile: &Path, default_dir: &Path) -> Result<()> {
    copy_codex_auth(&profile.join("auth.json"), &default_dir.join("auth.json"))
}

fn codex_has_fresh_reauth(profile: &Path) -> bool {
    let auth = match fs::read(profile.join("auth.json")) {
        Ok(auth) if !auth.is_empty() => auth,
        _ => return false,
    };
    let marker = codex_reauth_marker(profile);
    let Ok(previous_digest) = fs::read_to_string(marker) else {
        return true;
    };
    format!("{:x}", Sha256::digest(auth)) != previous_digest.trim()
}

/// Path to the Antigravity IDE .app (to open with `open`).
fn antigravity_ide_app() -> Option<PathBuf> {
    if cfg!(windows) {
        return None;
    }
    let app = PathBuf::from("/Applications/Antigravity IDE.app");
    if app.exists() {
        Some(app)
    } else {
        None
    }
}

/// Whether Antigravity IDE can be opened (.app installed or a shim in PATH).
fn antigravity_ide_available() -> bool {
    if cfg!(windows) {
        false
    } else {
        antigravity_ide_app().is_some()
            || home_dir()
                .join(".antigravity-ide/antigravity-ide/bin/antigravity-ide")
                .exists()
            || command_path("antigravity-ide").is_some()
    }
}

// ---------------------------------------------------------------------------
// Antigravity IDE switch = copy-swap the identity token in the default state.vscdb.
//
// The Antigravity IDE login lives in key `antigravityUnifiedStateSync.oauthToken`
// (base64/protobuf plaintext) inside the default userData's state.vscdb. It CAN'T
// be isolated with --user-data-dir (the OAuth callback deep-link routes back to
// the default instance). So: sign in to each account one at a time in the IDE →
// Capture token; switch = quit IDE → write the chosen account's token → reopen IDE.
// ---------------------------------------------------------------------------

/// state.vscdb of the Antigravity IDE default userData.
fn antigravity_state_db() -> PathBuf {
    default_config_dir(&ToolId::Antigravity).join("User/globalStorage/state.vscdb")
}

/// Account identity keys to swap (token + avatar). Other preferences are left untouched.
const AG_TOKEN_KEY: &str = "antigravityUnifiedStateSync.oauthToken";
const AG_PROFILE_KEY: &str = "antigravity.profileUrl";

/// Read one state.vscdb key as a safe SQL literal (quote() handles both TEXT/BLOB).
/// None if the key is absent or empty.
fn ag_read_key_literal(db: &Path, key: &str) -> Option<String> {
    let out = Command::new("sqlite3")
        .arg(db)
        .arg(format!(
            "SELECT quote(value) FROM ItemTable WHERE key='{key}';"
        ))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if value.is_empty() || value == "NULL" {
        None
    } else {
        Some(value)
    }
}

/// Write (INSERT OR REPLACE) one key into state.vscdb with an already-quoted SQL literal.
fn ag_write_key(db: &Path, key: &str, literal: &str) -> Result<()> {
    let status = Command::new("sqlite3")
        .arg(db)
        .arg(format!(
            "INSERT OR REPLACE INTO ItemTable(key,value) VALUES('{key}',{literal});"
        ))
        .status()?;
    if !status.success() {
        anyhow::bail!("sqlite3 failed to write state.vscdb");
    }
    Ok(())
}

/// Save the account currently signed in to the IDE: copy token (+ avatar) from the
/// default state.vscdb into the account dir. Returns a fingerprint (token hash), or
/// an error if not signed in.
pub fn antigravity_capture(store: &Store, account_id: &str) -> Result<String> {
    let db = antigravity_state_db();
    let token = ag_read_key_literal(&db, AG_TOKEN_KEY)
        .context("Antigravity IDE isn't signed in — sign into the account you want to save first, then click Save")?;
    let dir = store.account_dir(&ToolId::Antigravity, account_id);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("oauthToken.sql"), &token)?;
    if let Some(profile) = ag_read_key_literal(&db, AG_PROFILE_KEY) {
        fs::write(dir.join("profileUrl.sql"), profile)?;
    }
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    Ok(fingerprint(hasher.finalize().as_slice()))
}

/// The account's saved token (literal) — used to detect the account in use.
pub fn antigravity_saved_token(store: &Store, account_id: &str) -> Option<String> {
    fs::read_to_string(
        store
            .account_dir(&ToolId::Antigravity, account_id)
            .join("oauthToken.sql"),
    )
    .ok()
    .map(|t| t.trim().to_string())
    .filter(|t| !t.is_empty())
}

/// Saved avatar (profileUrl literal) — identifies the Google account, used for dedup:
/// the same account has the same avatar even if the token blob differs (captured twice at different times).
pub fn antigravity_saved_profile(store: &Store, account_id: &str) -> Option<String> {
    fs::read_to_string(
        store
            .account_dir(&ToolId::Antigravity, account_id)
            .join("profileUrl.sql"),
    )
    .ok()
    .map(|p| p.trim().to_string())
    .filter(|p| !p.is_empty())
}

/// The real avatar URL (with SQL quoting removed) of the Antigravity account — for display in the UI.
pub fn antigravity_avatar_url(store: &Store, account_id: &str) -> Option<String> {
    let literal = antigravity_saved_profile(store, account_id)?;
    let inner = literal
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))?;
    Some(inner.replace("''", "'"))
}

/// The identity token currently in the IDE (to know which account is active).
pub fn antigravity_current_token() -> Option<String> {
    ag_read_key_literal(&antigravity_state_db(), AG_TOKEN_KEY)
}

/// Switch the IDE's account: write the account's saved token into the default state.vscdb.
pub fn antigravity_restore(store: &Store, account_id: &str) -> Result<()> {
    let db = antigravity_state_db();
    if !db.exists() {
        anyhow::bail!("Antigravity IDE's state.vscdb not found");
    }
    let dir = store.account_dir(&ToolId::Antigravity, account_id);
    let token = fs::read_to_string(dir.join("oauthToken.sql"))
        .context("Account has no saved token — save it again while signed into this account")?;
    ag_write_key(&db, AG_TOKEN_KEY, token.trim())?;
    if let Ok(profile) = fs::read_to_string(dir.join("profileUrl.sql")) {
        let _ = ag_write_key(&db, AG_PROFILE_KEY, profile.trim());
    }
    Ok(())
}

/// Quit Antigravity IDE (gracefully) then wait for the process to end — to keep it
/// from overwriting state.vscdb after the swap. Returns once it has quit or the wait times out (~5s).
pub fn antigravity_quit_ide() {
    let _ = Command::new("osascript")
        .arg("-e")
        .arg(r#"tell application "Antigravity IDE" to quit"#)
        .status();
    for _ in 0..25 {
        let running = Command::new("pgrep")
            .arg("-f")
            .arg("Antigravity IDE.app/Contents/MacOS")
            .output()
            .map(|out| !String::from_utf8_lossy(&out.stdout).trim().is_empty())
            .unwrap_or(false);
        if !running {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Reopen Antigravity IDE (default userData) after swapping the token.
pub fn antigravity_open_ide() -> Result<()> {
    let app = antigravity_ide_app().context("Antigravity IDE is not installed")?;
    Command::new("/usr/bin/open")
        .arg("-a")
        .arg(&app)
        .spawn()
        .context("Couldn't open Antigravity IDE")?;
    Ok(())
}

/// Delete one key from state.vscdb.
fn ag_delete_key(db: &Path, key: &str) -> Result<()> {
    let status = Command::new("sqlite3")
        .arg(db)
        .arg(format!("DELETE FROM ItemTable WHERE key='{key}';"))
        .status()?;
    if !status.success() {
        anyhow::bail!("sqlite3 failed to delete the key");
    }
    Ok(())
}

/// Send the IDE to its sign-in screen to sign in a NEW account (never signed in before):
/// quit IDE (flush) → if the currently signed-in account is NOT yet saved, refuse (to
/// avoid losing the session) → delete the identity token from state.vscdb (sign out) →
/// reopen IDE. `saved_tokens` are the literal tokens of saved accounts, used to check
/// whether the current session has been saved.
pub fn antigravity_new_login(saved_tokens: &[String]) -> Result<()> {
    antigravity_quit_ide();
    let db = antigravity_state_db();
    if !db.exists() {
        anyhow::bail!("Antigravity IDE's state.vscdb not found");
    }
    if let Some(current) = ag_read_key_literal(&db, AG_TOKEN_KEY) {
        if !saved_tokens.iter().any(|token| token == &current) {
            let _ = antigravity_open_ide();
            anyhow::bail!(
                "Save the signed-in account first (Save current account), then sign in the new account — to avoid losing the session"
            );
        }
    }
    let _ = ag_delete_key(&db, AG_TOKEN_KEY);
    let _ = ag_delete_key(&db, AG_PROFILE_KEY);
    antigravity_open_ide()
}

/// Whether the Antigravity IDE account is signed in: the userData's state.vscdb has a non-empty
/// OAuth token (`antigravityUnifiedStateSync.oauthToken`). Read via sqlite3 (available on macOS).
fn antigravity_logged_in(data_dir: &Path) -> bool {
    let db = data_dir.join("User/globalStorage/state.vscdb");
    if !db.exists() {
        return false;
    }
    Command::new("sqlite3")
        .arg(&db)
        .arg("SELECT length(value) FROM ItemTable WHERE key='antigravityUnifiedStateSync.oauthToken';")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8_lossy(&out.stdout).trim().parse::<i64>().ok())
        .is_some_and(|len| len > 0)
}

pub fn is_installed(tool_id: &ToolId) -> bool {
    match tool_id {
        ToolId::Claude => command_path("claude").is_some(),
        ToolId::Codex => command_path("codex").is_some(),
        ToolId::Antigravity => antigravity_ide_available(),
    }
}

pub fn command_name(tool_id: &ToolId) -> &'static str {
    match tool_id {
        ToolId::Claude => "claude",
        ToolId::Codex => "codex",
        ToolId::Antigravity => "antigravity-ide",
    }
}

pub fn launch_login(tool_id: &ToolId) -> Result<()> {
    match tool_id {
        // Claude 2.x does NOT have `claude login` — it must be `claude auth login`.
        ToolId::Claude => run_login_command("claude", &["auth", "login"]),
        ToolId::Codex => run_login_command("codex", &["login", "--device-auth"]),
        ToolId::Antigravity => {
            if Path::new("/Applications/Antigravity.app").exists() {
                Command::new("open")
                    .arg("-a")
                    .arg("Antigravity")
                    .status()
                    .context("open Antigravity failed")?;
            } else {
                Command::new("open")
                    .arg("-a")
                    .arg("Google Antigravity")
                    .status()
                    .context("open Google Antigravity failed")?;
            }
            Ok(())
        }
    }
}

pub fn create_profile_with_default(
    tool_id: &ToolId,
    store: &Store,
    account_id: &str,
    default_config_dir: &Path,
) -> Result<PathBuf> {
    let profile = store.account_dir(tool_id, account_id);
    fs::create_dir_all(&profile)?;
    link_shared_sessions_to(tool_id, &profile, default_config_dir);
    Ok(profile)
}

/// Chat-session dir/file names that should be shared across a tool's accounts.
/// The token (auth.json/keychain) is NOT among these, so quota stays per-account.
/// - Claude: per-project transcripts in `projects/` + prompt history `history.jsonl`.
/// - Codex: conversation rollouts in `sessions/` + prompt history `history.jsonl`.
fn shared_session_names(tool_id: &ToolId) -> &'static [&'static str] {
    match tool_id {
        ToolId::Claude => &["projects", "history.jsonl"],
        ToolId::Codex => &["sessions", "history.jsonl"],
        ToolId::Antigravity => &[],
    }
}

/// Config/preferences that should be shared by normal OAuth accounts of the same tool.
/// Credentials are deliberately excluded:
/// - Claude credentials live in macOS Keychain under the profile-dir hash.
/// - Codex credentials live in `auth.json` / `api_key`.
///
/// API/proxy accounts must NOT use this list because their `settings.json` / `config.toml`
/// contains account-specific gateway settings.
fn shared_config_names(tool_id: &ToolId) -> &'static [&'static str] {
    match tool_id {
        ToolId::Claude => &[
            ".claude.json",
            "settings.json",
            "settings.local.json",
            "plugins",
            "rules",
            "commands",
            "agents",
        ],
        // SQLite databases are intentionally excluded: sharing a live .sqlite (+ WAL/SHM) between
        // the default Codex process and a profile Codex process causes write-lock contention.
        // When both run simultaneously, the profile process blocks waiting for the WAL write lock
        // held by the default process, which causes the WebSocket initialisation to time out
        // ("Reconnecting... 4/5"). Each account keeps its own memories and goals DBs.
        ToolId::Codex => &["config.toml", "rules", "skills", "memories"],
        ToolId::Antigravity => &[],
    }
}

fn shared_target_to(tool_id: &ToolId, name: &str, default_config_dir: &Path) -> PathBuf {
    if matches!(tool_id, ToolId::Claude) && name == ".claude.json" {
        home_dir().join(".claude.json")
    } else {
        default_config_dir.join(name)
    }
}

/// Share session/history across accounts: symlink the profile's chat-session entries
/// to the original config dir (`~/.claude`, `~/.codex`). This lets any account resume
/// a session created by another account, even in the same project. The token stays
/// per-profile so quotas don't mix. Idempotent — safe to call again.
pub fn link_shared_sessions_to(tool_id: &ToolId, profile: &Path, default_config_dir: &Path) {
    let can_link = symlinks_supported(profile);
    for name in shared_session_names(tool_id) {
        link_shared_entry(tool_id, profile, name, true, default_config_dir, can_link);
    }
}

/// Share CLI config/memory across normal OAuth profiles. This keeps account switching focused on
/// credentials/quota while user settings, skills, rules, and Codex memory stay consistent.
pub fn link_shared_config_to(tool_id: &ToolId, profile: &Path, default_config_dir: &Path) {
    // Remove any stale symlinks that were previously created but are no longer in the shared list
    // (e.g. memories_1.sqlite was removed to fix SQLite lock contention).
    remove_stale_shared_symlinks(tool_id, profile, default_config_dir);
    let can_link = symlinks_supported(profile);
    for name in shared_config_names(tool_id) {
        link_shared_entry(tool_id, profile, name, false, default_config_dir, can_link);
    }
}

/// Remove symlinks in `profile` that point into `default_config_dir` but are no longer in the
/// shared-config list. Called before re-applying the list so old symlinks don't linger.
fn remove_stale_shared_symlinks(tool_id: &ToolId, profile: &Path, default_config_dir: &Path) {
    let still_shared: std::collections::HashSet<&str> =
        shared_config_names(tool_id).iter().copied().collect();
    // Previously-shared Codex SQLite files that were removed from the list.
    let formerly_shared: &[&str] = match tool_id {
        ToolId::Codex => &[
            "memories_1.sqlite",
            "memories_1.sqlite-wal",
            "memories_1.sqlite-shm",
            "goals_1.sqlite",
            "goals_1.sqlite-wal",
            "goals_1.sqlite-shm",
        ],
        _ => &[],
    };
    for name in formerly_shared {
        if still_shared.contains(name) {
            continue;
        }
        let link = profile.join(name);
        let target = shared_target_to(tool_id, name, default_config_dir);
        if fs::read_link(&link).is_ok_and(|t| t == target) {
            let _ = fs::remove_file(&link);
        }
    }
}

fn link_shared_entry(
    tool_id: &ToolId,
    profile: &Path,
    name: &str,
    merge_dirs: bool,
    default_config_dir: &Path,
    can_link: bool,
) {
    // Don't link to itself (Default account = ~/.claude, ~/.codex).
    if profile == default_config_dir {
        return;
    }
    // Everything below moves the profile's real entry into the shared store and replaces it with
    // a symlink. Without symlink support that is a one-way delete, so leave the account alone and
    // let it keep its own config.
    if !can_link {
        return;
    }

    let target = shared_target_to(tool_id, name, default_config_dir);
    let link = profile.join(name);
    if link == target || fs::read_link(&link).is_ok_and(|t| t == target) {
        return;
    }

    let is_real_dir =
        link.is_dir() && fs::symlink_metadata(&link).is_ok_and(|m| !m.file_type().is_symlink());
    if is_real_dir {
        if target.exists() || merge_dirs {
            merge_dir_into(&link, &target);
        } else {
            let _ = fs::rename(&link, &target);
        }
        let _ = fs::remove_dir_all(&link);
    } else if link.is_file()
        && fs::symlink_metadata(&link).is_ok_and(|m| !m.file_type().is_symlink())
    {
        if !target.exists() {
            if let Some(parent) = target.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::rename(&link, &target);
        }
        let _ = fs::remove_file(&link);
    } else if link.exists() || fs::symlink_metadata(&link).is_ok() {
        let _ = fs::remove_file(&link);
    }

    if !target.exists() {
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let looks_like_dir = !name.contains('.')
            || matches!(
                name,
                "plugins" | "rules" | "skills" | "memories" | "commands" | "agents"
            );
        if looks_like_dir {
            let _ = fs::create_dir_all(&target);
        }
    }
    if target.exists() {
        let _ = create_symlink(&target, &link);
    }
}

/// Recursively copy every file from `source` to `target`, WITHOUT overwriting existing
/// files (keep the shared-store copy on name collision). Used to preserve old sessions
/// when turning the account's real directory into a shared symlink.
fn merge_dir_into(source: &Path, target: &Path) {
    let _ = fs::create_dir_all(target);
    let Ok(entries) = fs::read_dir(source) else {
        return;
    };
    for entry in entries.flatten() {
        let from = entry.path();
        let to = target.join(entry.file_name());
        if from.is_dir() {
            merge_dir_into(&from, &to);
        } else if !to.exists() {
            let _ = fs::copy(&from, &to);
        }
    }
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Windows needs a separate call for file vs directory links, and both require either Developer
/// Mode or elevation. Failure is reported, never swallowed — `link_shared_entry` relies on the
/// error to leave the profile's real entry untouched.
#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlinks are not supported on this platform",
    ))
}

/// Whether this process may create symlinks under `probe_dir`. On Windows that takes Developer
/// Mode or elevation, so it is probed once per run and cached. Without it, config sharing must
/// stay off entirely: `link_shared_entry` removes the profile's real entry before linking, so a
/// silent link failure would delete the account's own config and put nothing back.
fn symlinks_supported(probe_dir: &Path) -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        if fs::create_dir_all(probe_dir).is_err() {
            return false;
        }
        let stamp = std::process::id();
        let target = probe_dir.join(format!(".symlink-probe-{stamp}"));
        let link = probe_dir.join(format!(".symlink-probe-link-{stamp}"));
        let _ = fs::remove_file(&link);
        if fs::write(&target, b"probe").is_err() {
            return false;
        }
        let ok = create_symlink(&target, &link).is_ok();
        let _ = fs::remove_file(&link);
        let _ = fs::remove_file(&target);
        ok
    })
}

/// Seed the "onboarding done" flag into the profile's `.claude.json` so interactive `claude`
/// does NOT run the first-time wizard (the wizard includes a "Select login method" step that
/// opens the browser and forces a re-login even when a token exists, plus "Choose text style").
/// Call this AFTER login finishes, because `claude auth login` overwrites `.claude.json` without this flag.
pub fn seed_onboarding(tool_id: &ToolId, profile: &Path) {
    if !matches!(tool_id, ToolId::Claude) {
        return;
    }
    let target = profile.join(".claude.json");
    let mut doc: serde_json::Value = fs::read_to_string(&target)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(obj) = doc.as_object_mut() else {
        return;
    };

    let home_cfg: Option<serde_json::Value> = fs::read_to_string(home_dir().join(".claude.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let last_version = home_cfg
        .as_ref()
        .and_then(|c| c.get("lastOnboardingVersion"))
        .cloned()
        .or_else(|| crate::quota::claude_version().map(serde_json::Value::from))
        .unwrap_or_else(|| serde_json::Value::from("2.0.0"));
    let theme = home_cfg
        .as_ref()
        .and_then(|c| c.get("theme"))
        .cloned()
        .unwrap_or_else(|| serde_json::Value::from("dark"));

    obj.insert(
        "hasCompletedOnboarding".into(),
        serde_json::Value::Bool(true),
    );
    obj.insert("lastOnboardingVersion".into(), last_version);
    obj.entry("theme").or_insert(theme);

    if let Ok(text) = serde_json::to_string_pretty(&doc) {
        let _ = fs::write(&target, text);
    }
}

pub fn launch_profile_login(
    tool_id: &ToolId,
    store: &Store,
    account_id: &str,
    default_config_dir: &Path,
    binary_path: &Path,
    app: Option<AppHandle>,
    codex_login_method: CodexLoginMethod,
) -> Result<()> {
    let profile = create_profile_with_default(tool_id, store, account_id, default_config_dir)?;
    link_shared_config_to(tool_id, &profile, default_config_dir);
    // Login only writes the token into the account's own config dir. It does NOT touch the
    // original `claude`/`codex` binaries. The account is used via its own command `claude-<name>`
    // (created by write_launcher); the original `claude` command is always the machine Default.
    match tool_id {
        ToolId::Claude => run_profile_login_command(
            binary_path,
            "CLAUDE_CONFIG_DIR",
            &profile,
            &["auth", "login"],
            tool_id,
            account_id,
            app,
        ),
        ToolId::Codex => {
            match codex_login_method {
                CodexLoginMethod::DeviceCode => {
                    let _ = open_codex_device_auth_in_isolated_profile(store, account_id);
                }
                CodexLoginMethod::BrowserProfile => {
                    let _ = open_codex_browser_login_profile(store, account_id);
                }
            }
            let args = codex_login_args(codex_login_method);
            run_profile_login_command(
                binary_path,
                "CODEX_HOME",
                &profile,
                &args,
                tool_id,
                account_id,
                app,
            )
        }
        ToolId::Antigravity => launch_login(tool_id),
    }
}

/// Whether the account has finished logging in — a token already exists for this config dir.
/// Used for the background poll after opening the Terminal login.
pub fn profile_has_credentials(tool_id: &ToolId, config_dir: &Path) -> bool {
    match tool_id {
        // macOS keeps the token in the Keychain; Windows/Linux keep it in the profile's
        // `.credentials.json`. `claude_has_credentials` checks both — a keychain-only check here
        // left every Claude profile stuck in NeedsLogin on Windows.
        ToolId::Claude => crate::quota::claude_has_credentials(config_dir),
        // Codex stores the token in a file inside CODEX_HOME.
        ToolId::Codex => codex_has_fresh_reauth(config_dir),
        // Antigravity IDE: the token lives in the account's own userData state.vscdb.
        ToolId::Antigravity => antigravity_logged_in(config_dir),
    }
}

// ---------------------------------------------------------------------------
// Per-account launchers (Claude/Codex).
//
// Do NOT wrap the original `claude`/`codex` binaries (that once broke PATH). Instead
// each account has its OWN command `claude-<name>` / `codex-<name>` placed in
// ~/.local/bin, which hard-codes the config dir then execs the real binary:
//
//   #!/bin/sh
//   # ai-account-switcher-launcher v1
//   export CLAUDE_CONFIG_DIR='/.../accounts/claude/<id>'
//   exec '/Users/.../.local/bin/claude' "$@"
//
// The original `claude` command is never touched → it's always the machine Default.
// ---------------------------------------------------------------------------

const LAUNCHER_MARKER: &str = "# ai-account-switcher-launcher v1";

fn launcher_dir() -> PathBuf {
    if cfg!(windows) {
        app_data_local_dir().join("bin")
    } else {
        home_dir().join(".local/bin")
    }
}

pub fn launcher_path(name: &str) -> PathBuf {
    if cfg!(windows) {
        launcher_dir().join(format!("{name}.cmd"))
    } else {
        launcher_dir().join(name)
    }
}

pub fn launcher_paths(name: &str) -> Vec<PathBuf> {
    if cfg!(windows) {
        vec![
            launcher_path(name),
            launcher_dir().join(format!("{name}.ps1")),
        ]
    } else {
        vec![launcher_path(name)]
    }
}

fn launcher_prefix(tool_id: &ToolId) -> Result<&'static str> {
    match tool_id {
        ToolId::Claude => Ok("claude-"),
        ToolId::Codex => Ok("codex-"),
        ToolId::Antigravity => anyhow::bail!("Antigravity is a GUI app and has no custom command"),
    }
}

/// Normalize + validate the command name the user entered. Enforce the `claude-`/`codex-`
/// prefix, allow only `a-z 0-9 - _`. Returns the full name (with prefix).
pub fn full_launcher_name(tool_id: &ToolId, raw: &str) -> Result<String> {
    let prefix = launcher_prefix(tool_id)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Command name is empty");
    }
    // Allow the user to type it with or without the prefix.
    let body = trimmed.strip_prefix(prefix).unwrap_or(trimmed);
    if body.is_empty() {
        anyhow::bail!("Command name is empty");
    }
    if !body
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        anyhow::bail!("Command name may only contain lowercase a-z, digits, - or _");
    }
    if body.len() > 40 {
        anyhow::bail!("Command name is limited to 40 characters");
    }
    Ok(format!("{prefix}{body}"))
}

/// true if the file at path is a launcher created by the app (has the marker).
pub fn is_our_launcher(path: &Path) -> bool {
    fs::read_to_string(path)
        .map(|text| text.contains(LAUNCHER_MARKER))
        .unwrap_or(false)
}

/// Whether a command named `full_name` already exists in PATH that is NOT one of the
/// app's launchers (to avoid overwriting system binaries like `git`, `node`).
pub fn launcher_name_collides_with_system(full_name: &str) -> bool {
    match command_path(full_name) {
        Some(path) => !is_our_launcher(&path),
        None => false,
    }
}

/// Create/overwrite the launcher for the account. Execs the real binary by absolute path
/// (the ~/.local/bin/<binary> symlink stays stable across auto-update).
pub fn write_launcher(
    tool_id: &ToolId,
    store: &Store,
    account_id: &str,
    full_name: &str,
    real_binary: &Path,
) -> Result<()> {
    let env_name = match tool_id {
        ToolId::Claude => "CLAUDE_CONFIG_DIR",
        ToolId::Codex => "CODEX_HOME",
        ToolId::Antigravity => anyhow::bail!("Antigravity doesn't support custom commands"),
    };
    let profile = store.account_dir(tool_id, account_id);

    let dir = launcher_dir();
    fs::create_dir_all(&dir)?;
    if cfg!(windows) {
        write_windows_launcher_files(full_name, env_name, &profile, real_binary, None, None, None)?;
    } else {
        let path = dir.join(full_name);
        let script = format!(
            "#!/bin/sh\n{marker}\nexport {env}={dir}\nexec {bin} \"$@\"\n",
            marker = LAUNCHER_MARKER,
            env = env_name,
            dir = shell_quote(&profile.to_string_lossy()),
            bin = shell_quote(&real_binary.to_string_lossy()),
        );
        fs::write(&path, script)?;
        set_owner_executable_permissions(&path)?;
    }
    Ok(())
}

/// Delete the launcher (only if it really is one of the app's launchers).
pub fn remove_launcher(name: &str) {
    for path in launcher_paths(name) {
        if path.exists() && is_our_launcher(&path) {
            let _ = fs::remove_file(path);
        }
    }
}

// ---------------------------------------------------------------------------
// API / proxy accounts (Codex) — run the CLI through an external gateway with an API key
// instead of a subscription OAuth login. The account is a normal profile dir, but instead
// of `auth.json` it holds a `config.toml` (model_provider = proxy) plus an `api_key` file.
// Switch/launcher reuse the same machinery; the only extra is exporting the API key.
// ---------------------------------------------------------------------------

/// The file holding the raw API key inside an account's profile dir (chmod 600).
fn api_key_path(profile: &Path) -> PathBuf {
    profile.join("api_key")
}

/// Normalize a gateway base URL: trim whitespace and any trailing slash.
fn normalize_base_url(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

/// List the gateway's available models, returning the `data[].id` strings. Tries `{base}/models`
/// first, then `{base}/v1/models` (Claude's base URL often omits the `/v1` that the models
/// endpoint needs). Reuses the app's internal HTTP helper (same as quota reads).
pub fn fetch_gateway_models(base_url: &str, api_key: &str) -> Result<Vec<String>> {
    let base = normalize_base_url(base_url);
    let mut candidates = vec![format!("{base}/models")];
    if !base.ends_with("/v1") {
        candidates.push(format!("{base}/v1/models"));
    }
    let auth = format!("Bearer {}", api_key.trim());

    let mut last_err: Option<anyhow::Error> = None;
    for url in candidates {
        match crate::quota::curl_get(&url, &[("Authorization", auth.as_str())]) {
            Ok(body) => {
                let value: serde_json::Value = serde_json::from_str(&body)
                    .context("The gateway returned an unexpected response")?;
                let ids = value
                    .get("data")
                    .and_then(|data| data.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| item.get("id").and_then(|id| id.as_str()))
                            .map(|id| id.to_string())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if ids.is_empty() {
                    anyhow::bail!("The gateway returned no models");
                }
                return Ok(ids);
            }
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Couldn't fetch models")))
        .context("Couldn't fetch models — check the gateway URL and API key")
}

/// Write the API key into the profile dir (owner read/write only).
pub fn write_api_key_file(profile: &Path, api_key: &str) -> Result<()> {
    fs::create_dir_all(profile)?;
    let path = api_key_path(profile);
    fs::write(&path, api_key.trim().as_bytes())?;
    set_owner_only_permissions(&path)?;
    Ok(())
}

/// Escape a value for a TOML basic string (between double quotes).
fn toml_basic_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Write the Codex `config.toml` for an API/proxy account: point the provider at the gateway
/// and pin the model. `provider_name` is shown by Codex `/status`. Written once at creation;
/// Codex may later append its own keys (project trust, etc.) — the launcher re-forces the model
/// via `-m`, so config drift doesn't break the account.
pub fn write_codex_proxy_config(
    profile: &Path,
    provider_name: &str,
    base_url: &str,
    model: &str,
) -> Result<()> {
    fs::create_dir_all(profile)?;
    let mut toml = String::new();
    toml.push_str("# Managed by AI Account Switcher — API/proxy account.\n");
    toml.push_str(&format!("model = {}\n", toml_basic_string(model)));
    toml.push_str("model_provider = \"proxy\"\n\n");
    toml.push_str("[model_providers.proxy]\n");
    toml.push_str(&format!("name = {}\n", toml_basic_string(provider_name)));
    toml.push_str(&format!(
        "base_url = {}\n",
        toml_basic_string(&normalize_base_url(base_url))
    ));
    toml.push_str("wire_api = \"responses\"\n");
    toml.push_str("env_key = \"OPENAI_API_KEY\"\n");
    toml.push_str("supports_websockets = false\n\n");
    toml.push_str("[notice]\n");
    toml.push_str("hide_full_access_warning = true\n");
    fs::write(profile.join("config.toml"), toml)?;
    Ok(())
}

/// Write the Claude Code `settings.json` for an API/proxy account: Claude reads `env` from the
/// config dir's settings and applies the gateway base URL, bearer token, and model. Codex uses a
/// config.toml + key file instead; Claude is env-driven, so everything lives here.
pub fn write_claude_proxy_settings(
    profile: &Path,
    base_url: &str,
    token: &str,
    model: &str,
) -> Result<()> {
    fs::create_dir_all(profile)?;
    let settings = serde_json::json!({
        "env": {
            "ANTHROPIC_BASE_URL": normalize_base_url(base_url),
            "ANTHROPIC_AUTH_TOKEN": token.trim(),
            "ANTHROPIC_MODEL": model,
        }
    });
    let path = profile.join("settings.json");
    fs::write(&path, serde_json::to_string_pretty(&settings)?)?;
    set_owner_only_permissions(&path)?;
    Ok(())
}

/// Create/overwrite the launcher for an API/proxy account.
///
/// - Codex: export the key + force the gateway model with `-m` (Codex rewrites `model` in config
///   when the user touches its picker, so pinning on the command line keeps it working).
/// - Claude: auth/base/model come from the profile's settings.json env, so the launcher only sets
///   the config dir.
///
/// `bypass` adds the tool's skip-approvals flag.
pub fn write_api_launcher(
    tool_id: &ToolId,
    store: &Store,
    account_id: &str,
    full_name: &str,
    model: &str,
    bypass: bool,
    real_binary: &Path,
) -> Result<()> {
    let env_name = match tool_id {
        ToolId::Codex => "CODEX_HOME",
        ToolId::Claude => "CLAUDE_CONFIG_DIR",
        ToolId::Antigravity => anyhow::bail!("Antigravity doesn't support API/proxy accounts"),
    };
    let profile = store.account_dir(tool_id, account_id);

    // Codex pins the key + model on the command line; Claude carries them in settings.json env.
    let key_export = match tool_id {
        ToolId::Codex => format!(
            "export OPENAI_API_KEY=\"$(cat {})\"\n",
            shell_quote(&api_key_path(&profile).to_string_lossy())
        ),
        _ => String::new(),
    };
    let model_flag = match tool_id {
        ToolId::Codex => format!(" -m {}", shell_quote(model)),
        _ => String::new(),
    };
    let bypass_flag = match (tool_id, bypass) {
        (ToolId::Codex, true) => " --dangerously-bypass-approvals-and-sandbox",
        (ToolId::Claude, true) => " --dangerously-skip-permissions",
        _ => "",
    };

    let dir = launcher_dir();
    fs::create_dir_all(&dir)?;
    if cfg!(windows) {
        let key_path = matches!(tool_id, ToolId::Codex).then(|| api_key_path(&profile));
        let model = if matches!(tool_id, ToolId::Codex) {
            Some(model)
        } else {
            None
        };
        let flag = (!bypass_flag.is_empty()).then_some(bypass_flag.trim());
        write_windows_launcher_files(
            full_name,
            env_name,
            &profile,
            real_binary,
            key_path.as_deref(),
            model,
            flag,
        )?;
    } else {
        let path = dir.join(full_name);
        let script = format!(
            "#!/bin/sh\n{marker}\nexport {env}={dir}\n{key}exec {bin}{model}{flag} \"$@\"\n",
            marker = LAUNCHER_MARKER,
            env = env_name,
            dir = shell_quote(&profile.to_string_lossy()),
            key = key_export,
            bin = shell_quote(&real_binary.to_string_lossy()),
            model = model_flag,
            flag = bypass_flag,
        );
        fs::write(&path, script)?;
        set_owner_executable_permissions(&path)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// "Active" account for the BARE `claude`/`codex` commands — via a shell hook, NOT by
// wrapping the binary. The app writes the selected profile's path into the active file;
// an idempotent hook block in ~/.zshrc (+ ~/.bashrc if present) reads that file and
// exports CLAUDE_CONFIG_DIR/CODEX_HOME for every new shell. A per-account launcher
// (claude-b) exports its own override so it's unaffected. Default account = delete the
// active file → the bare command uses ~/.claude.
// ---------------------------------------------------------------------------

const HOOK_BEGIN: &str = "# >>> ai-account-switcher >>>";
const HOOK_END: &str = "# <<< ai-account-switcher <<<";

/// Write the account's profile into the active file (the bare command points here).
pub fn write_active_profile(tool_id: &ToolId, store: &Store, account_id: &str) -> Result<()> {
    let profile = store.account_dir(tool_id, account_id);
    if !profile.exists() {
        anyhow::bail!("Profile doesn't exist yet — has the account finished logging in?");
    }
    let active_path = store.active_profile_path(tool_id);
    if let Some(parent) = active_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(active_path, profile.to_string_lossy().as_bytes())?;
    Ok(())
}

/// Delete the active file (selecting machine Default → the bare command uses the original config dir).
pub fn clear_active_profile(tool_id: &ToolId, store: &Store) -> Result<()> {
    let active_path = store.active_profile_path(tool_id);
    if active_path.exists() {
        fs::remove_file(active_path)?;
    }
    Ok(())
}

/// Install (idempotently) the hook block into the shell rc so the bare command follows the selected account.
/// Called on every switch — cheap and self-healing if the user accidentally deletes it.
pub fn install_shell_hook(store: &Store) -> Result<()> {
    if cfg!(windows) {
        let block = render_powershell_hook_block(store);
        for profile in powershell_profile_paths() {
            upsert_block(&profile, &block)?;
        }
        return Ok(());
    }
    let claude_active = store.active_profile_path(&ToolId::Claude);
    let codex_active = store.active_profile_path(&ToolId::Codex);
    // `aisw` is a shell function: it re-reads the active file on EVERY call then exports. Used to
    // sync the new account into an already-open terminal without needing `source ~/.zshrc`.
    // At shell startup we call it once → a new terminal lands on the right account automatically.
    let block = format!(
        "{begin}\n\
         aisw() {{\n\
        \x20 if [ -r {claude} ]; then export CLAUDE_CONFIG_DIR=\"$(cat {claude})\"; else unset CLAUDE_CONFIG_DIR; fi\n\
        \x20 if [ -r {codex} ]; then export CODEX_HOME=\"$(cat {codex})\"; [ -r \"$CODEX_HOME/api_key\" ] && export OPENAI_API_KEY=\"$(cat \"$CODEX_HOME/api_key\")\"; else unset CODEX_HOME; fi\n\
        \x20 [ -n \"$1\" ] && echo \"AI Account Switcher: synced the account for this terminal.\"\n\
         }}\n\
         aisw >/dev/null 2>&1\n\
         {end}\n",
        begin = HOOK_BEGIN,
        end = HOOK_END,
        claude = shell_quote(&claude_active.to_string_lossy()),
        codex = shell_quote(&codex_active.to_string_lossy()),
    );

    let home = home_dir();
    // zsh is the default shell on macOS; add bash if the user has ~/.bashrc.
    let mut targets = vec![home.join(".zshrc")];
    if home.join(".bashrc").exists() {
        targets.push(home.join(".bashrc"));
    }
    for rc in targets {
        upsert_block(&rc, &block)?;
    }
    Ok(())
}

fn render_powershell_hook_block(store: &Store) -> String {
    let claude_active =
        powershell_quote(&store.active_profile_path(&ToolId::Claude).to_string_lossy());
    let codex_active =
        powershell_quote(&store.active_profile_path(&ToolId::Codex).to_string_lossy());
    let launchers = powershell_quote(&launcher_dir().to_string_lossy());
    format!(
        "{begin}\n\
function aisw {{\n\
  $claudeActive = {claude}\n\
  $codexActive = {codex}\n\
  if (Test-Path $claudeActive) {{ $env:CLAUDE_CONFIG_DIR = (Get-Content $claudeActive -Raw).Trim() }} else {{ Remove-Item Env:CLAUDE_CONFIG_DIR -ErrorAction SilentlyContinue }}\n\
  if (Test-Path $codexActive) {{\n\
    $env:CODEX_HOME = (Get-Content $codexActive -Raw).Trim()\n\
    $apiKeyPath = Join-Path $env:CODEX_HOME 'api_key'\n\
    if (Test-Path $apiKeyPath) {{ $env:OPENAI_API_KEY = (Get-Content $apiKeyPath -Raw).Trim() }} else {{ Remove-Item Env:OPENAI_API_KEY -ErrorAction SilentlyContinue }}\n\
  }} else {{\n\
    Remove-Item Env:CODEX_HOME -ErrorAction SilentlyContinue\n\
    Remove-Item Env:OPENAI_API_KEY -ErrorAction SilentlyContinue\n\
  }}\n\
  if ($args.Count -gt 0) {{ Write-Host 'AI Account Switcher: synced the account for this terminal.' }}\n\
}}\n\
$aiswLauncherDir = {launchers}\n\
if (-not (($env:Path -split ';') -contains $aiswLauncherDir)) {{ $env:Path = \"$aiswLauncherDir;$env:Path\" }}\n\
aisw | Out-Null\n\
{end}\n",
        begin = HOOK_BEGIN,
        end = HOOK_END,
        claude = claude_active,
        codex = codex_active,
        launchers = launchers,
    )
}

/// Documents folders PowerShell might load `$PROFILE` from, most-likely first. OneDrive Known
/// Folder Move (default-on for many Windows setups) relocates Documents under the OneDrive folder,
/// so the plain `~/Documents` is often NOT where PowerShell looks.
fn documents_roots() -> Vec<PathBuf> {
    let home = home_dir();
    let mut roots = Vec::new();
    if let Ok(entries) = fs::read_dir(&home) {
        for entry in entries.flatten() {
            // Matches both "OneDrive" (personal) and "OneDrive - <Org>" (commercial).
            if entry.file_name().to_string_lossy().starts_with("OneDrive") {
                let docs = entry.path().join("Documents");
                if docs.is_dir() {
                    roots.push(docs);
                }
            }
        }
    }
    roots.push(home.join("Documents"));
    roots
}

fn powershell_profile_paths() -> Vec<PathBuf> {
    let roots = documents_roots();
    let mut out = Vec::new();
    for host in ["PowerShell", "WindowsPowerShell"] {
        // Prefer an existing profile — that's the exact file PowerShell loads. Writing our hook to
        // a ~/Documents path that OneDrive has redirected away means new shells never see it and
        // account switching silently never affects the bare `codex`/`claude` commands.
        let existing = roots
            .iter()
            .map(|root| root.join(host).join("Microsoft.PowerShell_profile.ps1"))
            .find(|path| path.exists());
        if let Some(path) = existing {
            out.push(path);
            continue;
        }
        // None exists yet: create it under the real (first existing) Documents root, OneDrive first.
        let target_root = roots
            .iter()
            .find(|root| root.is_dir())
            .cloned()
            .unwrap_or_else(|| home_dir().join("Documents"));
        out.push(target_root.join(host).join("Microsoft.PowerShell_profile.ps1"));
    }
    out
}

/// How an existing rc/profile file was encoded, so it can be written back the same way.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TextEncoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

/// Decode an rc/profile file, honouring the BOM. Windows PowerShell 5.1 writes `$PROFILE` as
/// UTF-16LE by default (`"..." > $PROFILE`, the ISE), which is not valid UTF-8 — reading such a
/// file as UTF-8 and falling back to an empty string would make `upsert_block` overwrite the
/// user's entire profile with just our hook.
fn decode_text_file(bytes: &[u8]) -> Option<(String, TextEncoding)> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8(bytes[3..].to_vec())
            .ok()
            .map(|text| (text, TextEncoding::Utf8Bom));
    }
    for (bom, encoding) in [
        ([0xFF, 0xFE], TextEncoding::Utf16Le),
        ([0xFE, 0xFF], TextEncoding::Utf16Be),
    ] {
        if bytes.starts_with(&bom) {
            let body = &bytes[2..];
            if !body.len().is_multiple_of(2) {
                return None;
            }
            let units: Vec<u16> = body
                .chunks_exact(2)
                .map(|pair| match encoding {
                    TextEncoding::Utf16Le => u16::from_le_bytes([pair[0], pair[1]]),
                    _ => u16::from_be_bytes([pair[0], pair[1]]),
                })
                .collect();
            return String::from_utf16(&units).ok().map(|text| (text, encoding));
        }
    }
    String::from_utf8(bytes.to_vec())
        .ok()
        .map(|text| (text, TextEncoding::Utf8))
}

fn encode_text_file(text: &str, encoding: TextEncoding) -> Vec<u8> {
    match encoding {
        TextEncoding::Utf8 => text.as_bytes().to_vec(),
        TextEncoding::Utf8Bom => {
            let mut out = vec![0xEF, 0xBB, 0xBF];
            out.extend_from_slice(text.as_bytes());
            out
        }
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            let mut out = if encoding == TextEncoding::Utf16Le {
                vec![0xFF, 0xFE]
            } else {
                vec![0xFE, 0xFF]
            };
            for unit in text.encode_utf16() {
                let pair = if encoding == TextEncoding::Utf16Le {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                };
                out.extend_from_slice(&pair);
            }
            out
        }
    }
}

/// Replace the block between the markers (if present) or append; create the file if it doesn't exist.
fn upsert_block(rc: &Path, block: &str) -> Result<()> {
    if let Some(parent) = rc.parent() {
        fs::create_dir_all(parent)?;
    }
    let existing = fs::read(rc).ok();
    let (current, encoding) = match existing {
        Some(bytes) if !bytes.is_empty() => decode_text_file(&bytes).with_context(|| {
            // Refuse rather than replace: the file holds the user's aliases, prompt and module
            // imports, and there is no backup to restore from.
            format!(
                "Couldn't read {} as text, so it was left untouched",
                rc.display()
            )
        })?,
        _ => (String::new(), TextEncoding::Utf8),
    };
    let next = if let (Some(start), Some(end)) = (current.find(HOOK_BEGIN), current.find(HOOK_END))
    {
        let end = end + HOOK_END.len();
        let mut out = String::with_capacity(current.len());
        out.push_str(&current[..start]);
        out.push_str(block.trim_end());
        out.push_str(&current[end..]);
        out
    } else {
        let mut out = current;
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(block);
        out
    };
    fs::write(rc, encode_text_file(&next, encoding))?;
    Ok(())
}

pub fn delete_account_files(tool_id: &ToolId, store: &Store, account_id: &str) -> Result<()> {
    let account_dir = store.account_dir(tool_id, account_id);

    // On macOS Claude stores the token in the Keychain (keyed by the config dir's hash) → delete
    // it too so no orphaned credential remains. On Windows/Linux the token is the profile's
    // `.credentials.json`, removed with the account dir below, and there is no `security` binary.
    #[cfg(target_os = "macos")]
    if matches!(tool_id, ToolId::Claude) {
        let suffix = crate::quota::claude_keychain_suffix(&account_dir);
        let _ = Command::new("security")
            .args([
                "delete-generic-password",
                "-s",
                &format!("Claude Code-credentials-{suffix}"),
            ])
            .output();
    }

    if account_dir.exists() {
        fs::remove_dir_all(account_dir)?;
    }
    // Antigravity (copy-swap) stores its secret in the app's own keychain.
    for key in keychain_entries(tool_id) {
        let username = account_secret_username(tool_id, account_id, &key);
        if let Ok(entry) = Entry::new(APP_KEYCHAIN_SERVICE, &username) {
            let _ = entry.delete_credential();
        }
    }
    Ok(())
}

fn run_login_command(binary: &str, args: &[&str]) -> Result<()> {
    let command = command_path(binary).unwrap_or_else(|| PathBuf::from(binary));
    if cfg!(windows) {
        let args = args
            .iter()
            .map(|arg| powershell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!(
            "& {cmd} {args}; Write-Host ''; Write-Host 'After signing in, return to AI Account Switcher.'",
            cmd = powershell_quote(&command.to_string_lossy()),
            args = args,
        );
        return open_terminal_script(&script);
    }
    let mut script = if cfg!(windows) {
        powershell_quote(&command.to_string_lossy())
    } else {
        shell_quote(&command.to_string_lossy())
    };
    for arg in args {
        script.push(' ');
        let quoted_arg = if cfg!(windows) {
            powershell_quote(arg)
        } else {
            shell_quote(arg)
        };
        script.push_str(&quoted_arg);
    }
    if cfg!(windows) {
        script.push_str(
            "; Write-Host ''; Write-Host 'After signing in, return to AI Account Switcher.'",
        );
    } else {
        script.push_str("; echo; echo 'After signing in, return to AI Account Switcher.'");
    }

    open_terminal_script(&script)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn launcher_name_enforces_prefix_and_charset() {
        // typed without the prefix → prefix gets added
        assert_eq!(
            full_launcher_name(&ToolId::Claude, "abc").unwrap(),
            "claude-abc"
        );
        // typed with the prefix → kept as-is
        assert_eq!(
            full_launcher_name(&ToolId::Codex, "codex-work").unwrap(),
            "codex-work"
        );
        // invalid characters → error
        assert!(full_launcher_name(&ToolId::Claude, "a b").is_err());
        assert!(full_launcher_name(&ToolId::Claude, "ABC").is_err());
        assert!(full_launcher_name(&ToolId::Claude, "").is_err());
        // antigravity is not supported
        assert!(full_launcher_name(&ToolId::Antigravity, "x").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_launcher_paths_create_cmd_and_ps1() {
        let paths = launcher_paths("codex-work");
        assert_eq!(paths.len(), 2);
        assert!(paths[0].to_string_lossy().ends_with("codex-work.cmd"));
        assert!(paths[1].to_string_lossy().ends_with("codex-work.ps1"));
    }

    #[cfg(windows)]
    #[test]
    fn powershell_hook_mentions_codex_home_and_launcher_dir() {
        let root = std::env::temp_dir().join(format!("ai-switcher-test-{}", std::process::id()));
        let store = Store::for_test(root).unwrap();
        let block = render_powershell_hook_block(&store);
        let launcher_dir = launcher_dir().to_string_lossy().into_owned();
        assert!(block.contains("CODEX_HOME"));
        assert!(block.contains("function aisw"));
        assert!(
            block.contains(&launcher_dir.replace('\\', "\\\\"))
                || block.contains(launcher_dir.as_str())
        );
    }

    #[test]
    fn codex_login_uses_an_isolated_chrome_user_data_dir() {
        let args = isolated_codex_login_chrome_args(
            Path::new("C:/isolated/acc8"),
            codex_device_auth_url(),
        );
        assert!(args
            .iter()
            .any(|arg| arg == "--user-data-dir=C:/isolated/acc8"));
        assert!(args.iter().any(|arg| arg == "--new-window"));
        assert!(args.iter().any(|arg| arg == codex_device_auth_url()));
        assert!(!args.iter().any(|arg| arg == "--profile-directory"));
    }

    #[test]
    fn browser_profile_login_uses_standard_browser_oauth_not_device_code() {
        assert_eq!(
            codex_login_args(CodexLoginMethod::DeviceCode),
            ["login", "--device-auth"]
        );
        assert_eq!(
            codex_login_args(CodexLoginMethod::BrowserProfile),
            ["login"]
        );
    }

    #[test]
    fn reauth_requires_auth_json_to_change() {
        let root = std::env::temp_dir().join(format!(
            "ai-switcher-reauth-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("auth.json"), b"old-auth").unwrap();
        mark_codex_reauth_pending(&root).unwrap();
        assert!(!codex_has_fresh_reauth(&root));
        fs::write(root.join("auth.json"), b"new-auth").unwrap();
        assert!(codex_has_fresh_reauth(&root));
        clear_codex_reauth_pending(&root);
        assert!(codex_has_fresh_reauth(&root));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_codex_device_login_prompt() {
        let text = r#"
Follow these steps to sign in with ChatGPT using device code authorization:
1. Open this link in your browser and sign in to your account
   https://auth.openai.com/codex/device
2. Enter this one-time code (expires in 15 minutes)
   4VVW-NA1NF
"#;
        let prompt = parse_device_login_prompt(text);
        assert_eq!(
            prompt.login_url.as_deref(),
            Some("https://auth.openai.com/codex/device")
        );
        assert_eq!(prompt.device_code.as_deref(), Some("4VVW-NA1NF"));
    }

    #[test]
    fn device_login_prompt_ignores_non_device_tokens() {
        let prompt = parse_device_login_prompt("Successfully logged in token not shown abc-def");
        assert_eq!(prompt, DeviceLoginPrompt::default());
    }

    #[test]
    fn parses_styled_device_code_without_a_newline() {
        let prompt = parse_device_login_prompt(
            "\u{1b}[1mEnter this code:\u{1b}[0m \u{1b}[38;5;14m4VVW-NA1NF\u{1b}[0m\r",
        );
        assert_eq!(prompt.device_code.as_deref(), Some("4VVW-NA1NF"));
    }

    #[test]
    fn parses_structured_device_code_from_codex_app_server() {
        let message = r#"{"id":2,"result":{"type":"chatgptDeviceCode","loginId":"login-1","verificationUrl":"https://auth.openai.com/codex/device","userCode":"ABCD-12345"}}"#;
        let prompt = parse_app_server_device_login_message(message);
        assert_eq!(
            prompt.login_url.as_deref(),
            Some("https://auth.openai.com/codex/device")
        );
        assert_eq!(prompt.device_code.as_deref(), Some("ABCD-12345"));
    }

    #[test]
    fn detects_structured_device_login_completion() {
        let message = r#"{"method":"account/login/completed","params":{"loginId":"login-1","success":true,"error":null}}"#;
        assert_eq!(
            parse_app_server_login_signal(message),
            Some(AppServerLoginSignal::Completed(true))
        );
    }

    #[test]
    fn app_server_initialized_notification_uses_the_v1_empty_shape() {
        assert_eq!(
            app_server_initialized_message(),
            serde_json::json!({ "method": "initialized" })
        );
    }

    #[test]
    fn login_finished_event_is_account_scoped() {
        let event = LoginPromptFinishedEvent::new(ToolId::Codex, "acc-2", 2, false);
        assert_eq!(event.tool_id, ToolId::Codex);
        assert_eq!(event.account_id, "acc-2");
        assert_eq!(event.attempt_id, 2);
        assert!(!event.success);
        let payload = serde_json::to_value(&event).expect("event serializes");
        assert_eq!(payload["attemptId"], 2);
    }
}

fn run_profile_login_command(
    command: &Path,
    env_name: &str,
    profile: &Path,
    login_args: &[&str],
    tool_id: &ToolId,
    account_id: &str,
    app: Option<AppHandle>,
) -> Result<()> {
    if cfg!(windows) && matches!(tool_id, ToolId::Codex) {
        if let Some(app) = app {
            return run_hidden_profile_login_command(
                app,
                command,
                env_name,
                profile,
                login_args,
                tool_id.clone(),
                account_id.to_string(),
            );
        }
    }
    if cfg!(windows) {
        let args = login_args
            .iter()
            .map(|arg| powershell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!(
            "Write-Host '=== Sign in to {tool}: follow the prompts, approve in your browser ==='; $env:{env} = {dir}; & {cmd} {args}; Write-Host ''; Write-Host 'Done — return to AI Account Switcher (it will detect it); you can close this window.'",
            tool = tool_id.display_name(),
            env = env_name,
            dir = powershell_quote(&profile.to_string_lossy()),
            cmd = powershell_quote(&command.to_string_lossy()),
            args = args,
        );
        return open_terminal_script(&script);
    }
    let args = login_args
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!(
        "echo '=== Sign in to {tool}: follow the prompts, approve in your browser ==='; export {env}={dir}; {cmd} {args}; echo; echo 'Done — return to AI Account Switcher (it will detect it); you can close this window.'",
        tool = tool_id.display_name(),
        env = env_name,
        dir = shell_quote(&profile.to_string_lossy()),
        cmd = shell_quote(&command.to_string_lossy()),
        args = args,
    );
    open_terminal_script(&script)
}

#[cfg(windows)]
fn run_hidden_profile_login_command(
    app: AppHandle,
    command: &Path,
    env_name: &str,
    profile: &Path,
    login_args: &[&str],
    tool_id: ToolId,
    account_id: String,
) -> Result<()> {
    if matches!(tool_id, ToolId::Codex) && login_args == ["login", "--device-auth"] {
        return run_hidden_codex_device_code_login_command(
            app, command, env_name, profile, tool_id, account_id,
        );
    }
    run_hidden_profile_login_command_legacy(
        app, command, env_name, profile, login_args, tool_id, account_id,
    )
}

/// Uses Codex's structured app-server login protocol for device-code sign-in.
/// The normal CLI writes the same code to a terminal, but npm's cmd/node
/// wrapper can make that output unreliable when the app launches it hidden.
#[cfg(windows)]
fn run_hidden_codex_device_code_login_command(
    app: AppHandle,
    command: &Path,
    env_name: &str,
    profile: &Path,
    tool_id: ToolId,
    account_id: String,
) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::sync::mpsc;
    use std::time::Duration;

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);
    const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
    let attempt_id = NEXT_LOGIN_ATTEMPT_ID.fetch_add(1, Ordering::Relaxed);

    let mut child = match Command::new(command)
        .args(["app-server", "--stdio"])
        .env(env_name, profile)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = app.emit(
                "login-prompt-finished",
                LoginPromptFinishedEvent::new(tool_id, account_id, attempt_id, false),
            );
            return Err(error).context("Couldn't start the Codex device-login service");
        }
    };
    let stdin = child
        .stdin
        .take()
        .context("Codex device-login service didn't expose stdin")?;
    let stdout = child
        .stdout
        .take()
        .context("Codex device-login service didn't expose stdout")?;

    std::thread::spawn(move || {
        let (signal_tx, signal_rx) = mpsc::channel();
        let reader_app = app.clone();
        let reader_tool = tool_id.clone();
        let reader_account = account_id.clone();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let prompt = parse_app_server_device_login_message(&line);
                if prompt.login_url.is_some() && prompt.device_code.is_some() {
                    let _ = reader_app.emit(
                        "login-prompt",
                        LoginPromptEvent {
                            tool_id: reader_tool.clone(),
                            account_id: reader_account.clone(),
                            attempt_id,
                            login_url: prompt.login_url,
                            device_code: prompt.device_code,
                        },
                    );
                }
                if let Some(signal) = parse_app_server_login_signal(&line) {
                    let _ = signal_tx.send(signal);
                }
            }
        });

        let mut stdin = stdin;
        let mut write_message = |message: Value| -> Result<()> {
            let encoded = serde_json::to_string(&message)?;
            stdin.write_all(encoded.as_bytes())?;
            stdin.write_all(b"\n")?;
            stdin.flush()?;
            Ok(())
        };
        let initialized = write_message(serde_json::json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "AI Account Switcher",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {},
            },
        }))
        .and_then(|_| match signal_rx.recv_timeout(INITIALIZE_TIMEOUT) {
            Ok(AppServerLoginSignal::Initialized) => Ok(()),
            Ok(_) => anyhow::bail!("Codex device-login service completed before initialization"),
            Err(_) => anyhow::bail!("Codex device-login service did not initialize"),
        });
        let success = if initialized.is_ok()
            && write_message(app_server_initialized_message()).is_ok()
            && write_message(serde_json::json!({
                "id": 2,
                "method": "account/login/start",
                "params": { "type": "chatgptDeviceCode" },
            }))
            .is_ok()
        {
            matches!(
                signal_rx.recv_timeout(LOGIN_TIMEOUT),
                Ok(AppServerLoginSignal::Completed(true))
            )
        } else {
            false
        };
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        let _ = app.emit(
            "login-prompt-finished",
            LoginPromptFinishedEvent::new(tool_id, account_id, attempt_id, success),
        );
    });
    Ok(())
}

#[cfg(windows)]
fn run_hidden_profile_login_command_legacy(
    app: AppHandle,
    command: &Path,
    env_name: &str,
    profile: &Path,
    login_args: &[&str],
    tool_id: ToolId,
    account_id: String,
) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let attempt_id = NEXT_LOGIN_ATTEMPT_ID.fetch_add(1, Ordering::Relaxed);

    let mut child = match Command::new(command)
        .args(login_args)
        .env(env_name, profile)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = app.emit(
                "login-prompt-finished",
                LoginPromptFinishedEvent::new(
                    tool_id,
                    account_id,
                    attempt_id,
                    false,
                ),
            );
            return Err(error).context("Couldn't start the hidden Codex login process");
        }
    };

    let mut output_readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        output_readers.push(spawn_login_output_reader(
            stdout,
            app.clone(),
            tool_id.clone(),
            account_id.clone(),
            attempt_id,
        ));
    }
    if let Some(stderr) = child.stderr.take() {
        output_readers.push(spawn_login_output_reader(
            stderr,
            app.clone(),
            tool_id.clone(),
            account_id.clone(),
            attempt_id,
        ));
    }

    std::thread::spawn(move || {
        let success = child.wait().is_ok_and(|status| status.success());
        // Drain both pipes before emitting completion so a late prompt cannot resurrect the
        // banner after the frontend has cleared it.
        for reader in output_readers {
            let _ = reader.join();
        }
        let _ = app.emit(
            "login-prompt-finished",
            LoginPromptFinishedEvent::new(tool_id, account_id, attempt_id, success),
        );
    });
    Ok(())
}

#[cfg(windows)]
fn spawn_login_output_reader<R: Read + Send + 'static>(
    reader: R,
    app: AppHandle,
    tool_id: ToolId,
    account_id: String,
    attempt_id: u64,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut seen = DeviceLoginPrompt::default();
        let mut reader = BufReader::new(reader);
        let mut output = String::new();
        let mut bytes = [0_u8; 1024];
        loop {
            let count = match reader.read(&mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(count) => count,
            };
            output.push_str(&String::from_utf8_lossy(&bytes[..count]));
            // Login instructions are tiny. Keep a finite tail in case a CLI
            // writes status output for a long time before browser approval.
            if output.len() > 16 * 1024 {
                output.drain(..output.len() - 16 * 1024);
            }
            let prompt = parse_device_login_prompt(&output);
            if prompt.login_url.is_none() && prompt.device_code.is_none() {
                continue;
            }
            let mut changed = false;
            if prompt.login_url.is_some() && prompt.login_url != seen.login_url {
                seen.login_url = prompt.login_url;
                changed = true;
            }
            if prompt.device_code.is_some() && prompt.device_code != seen.device_code {
                seen.device_code = prompt.device_code;
                changed = true;
            }
            if changed {
                let _ = app.emit(
                    "login-prompt",
                    LoginPromptEvent {
                        tool_id: tool_id.clone(),
                        account_id: account_id.clone(),
                        attempt_id,
                        login_url: seen.login_url.clone(),
                        device_code: seen.device_code.clone(),
                    },
                );
            }
        }
    })
}

fn open_terminal_script(script: &str) -> Result<()> {
    if cfg!(windows) {
        Command::new("powershell.exe")
            .args(["-NoExit", "-ExecutionPolicy", "Bypass", "-Command", script])
            .spawn()
            .context("Couldn't open PowerShell to sign in")?;
        return Ok(());
    }
    Command::new("osascript")
        .arg("-e")
        .arg(r#"tell application "Terminal" to activate"#)
        .arg("-e")
        .arg(format!(
            r#"tell application "Terminal" to do script "{}""#,
            script.replace('"', "\\\"")
        ))
        .spawn()
        .context("Couldn't open Terminal to sign in")?;
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn cmd_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn write_windows_launcher_files(
    full_name: &str,
    env_name: &str,
    profile: &Path,
    real_binary: &Path,
    api_key_path: Option<&Path>,
    model: Option<&str>,
    extra_flag: Option<&str>,
) -> Result<()> {
    let dir = launcher_dir();
    fs::create_dir_all(&dir)?;

    let cmd_path = dir.join(format!("{full_name}.cmd"));
    let ps1_path = dir.join(format!("{full_name}.ps1"));
    let profile_cmd = cmd_quote(&profile.to_string_lossy());
    let binary_cmd = cmd_quote(&real_binary.to_string_lossy());
    let binary_ps = powershell_quote(&real_binary.to_string_lossy());
    let key_cmd = api_key_path
        .map(|path| {
            format!(
                "if exist {} (set /p OPENAI_API_KEY=<{}) else (set \"OPENAI_API_KEY=\")\r\n",
                cmd_quote(&path.to_string_lossy()),
                cmd_quote(&path.to_string_lossy())
            )
        })
        .unwrap_or_else(|| "set \"OPENAI_API_KEY=\"\r\n".to_string());
    let key_ps = api_key_path
        .map(|path| {
            let path = powershell_quote(&path.to_string_lossy());
            format!(
                "$apiKeyPath = {path}\nif (Test-Path $apiKeyPath) {{ $env:OPENAI_API_KEY = (Get-Content $apiKeyPath -Raw).Trim() }} else {{ Remove-Item Env:OPENAI_API_KEY -ErrorAction SilentlyContinue }}\n"
            )
        })
        .unwrap_or_else(|| "Remove-Item Env:OPENAI_API_KEY -ErrorAction SilentlyContinue\n".to_string());
    let model_cmd = model
        .map(|value| format!(" -m {}", cmd_quote(value)))
        .unwrap_or_default();
    let model_ps = model
        .map(|value| format!(" -m {}", powershell_quote(value)))
        .unwrap_or_default();
    let flag = extra_flag
        .map(|value| format!(" {value}"))
        .unwrap_or_default();

    let cmd_script = format!(
        "@echo off\r\nREM {marker}\r\nset \"{env}={profile}\"\r\n{key}{binary}{model}{flag} %*\r\n",
        marker = LAUNCHER_MARKER,
        env = env_name,
        profile = profile_cmd.trim_matches('"'),
        key = key_cmd,
        binary = binary_cmd,
        model = model_cmd,
        flag = flag,
    );
    let ps1_script = format!(
        "# {marker}\n$env:{env} = {profile}\n{key}& {binary}{model}{flag} @args\n",
        marker = LAUNCHER_MARKER,
        env = env_name,
        profile = powershell_quote(&profile.to_string_lossy()),
        key = key_ps,
        binary = binary_ps,
        model = model_ps,
        flag = flag,
    );

    fs::write(&cmd_path, cmd_script)?;
    fs::write(&ps1_path, ps1_script)?;
    set_owner_executable_permissions(&cmd_path)?;
    set_owner_executable_permissions(&ps1_path)?;
    Ok(())
}

#[derive(Clone)]
struct KeychainKey {
    service: String,
    username: String,
}

fn keychain_entries(tool_id: &ToolId) -> Vec<KeychainKey> {
    match tool_id {
        ToolId::Claude => {
            let username = current_username();
            vec![
                KeychainKey {
                    service: "Claude Code-credentials".to_string(),
                    username,
                },
                KeychainKey {
                    service: "Claude Code-credentials".to_string(),
                    username: "Claude Code".to_string(),
                },
                KeychainKey {
                    service: "Claude Code-credentials".to_string(),
                    username: "default".to_string(),
                },
            ]
        }
        ToolId::Codex => vec![],
        // Antigravity IDE switches via --user-data-dir (the login lives in each userData's
        // state.vscdb) so there's NO need to swap the keychain.
        ToolId::Antigravity => vec![],
    }
}

fn account_secret_username(tool_id: &ToolId, account_id: &str, key: &KeychainKey) -> String {
    format!(
        "{}:{}:{}:{}",
        tool_id.as_str(),
        account_id,
        key.service,
        key.username
    )
}

pub(crate) fn binary_candidate_names(binary: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![
            format!("{binary}.exe"),
            format!("{binary}.cmd"),
            format!("{binary}.bat"),
            format!("{binary}.ps1"),
            binary.to_string(),
        ]
    } else {
        vec![binary.to_string()]
    }
}

pub fn command_path(binary: &str) -> Option<PathBuf> {
    let command = if cfg!(windows) {
        "where.exe"
    } else {
        "/usr/bin/which"
    };
    let mut command = Command::new(command);
    hide_command_window(&mut command);
    let path_output = command.arg(binary).output().ok();
    if let Some(output) = path_output {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let first = stdout.lines().map(str::trim).find(|line| !line.is_empty());
            if let Some(text) = first {
                return Some(PathBuf::from(text));
            }
        }
    }

    common_bin_dirs()
        .into_iter()
        .flat_map(|dir| {
            binary_candidate_names(binary)
                .into_iter()
                .map(move |name| dir.join(name))
        })
        .find(|candidate| candidate.exists())
}

pub(crate) fn common_bin_dirs() -> Vec<PathBuf> {
    let home = home_dir();
    if cfg!(windows) {
        vec![
            launcher_dir(),
            home.join("AppData/Roaming/npm"),
            home.join(".cargo/bin"),
            home.join("scoop/shims"),
            PathBuf::from("C:/ProgramData/chocolatey/bin"),
            PathBuf::from("C:/Program Files/nodejs"),
            PathBuf::from("C:/Program Files/Git/usr/bin"),
            PathBuf::from("C:/Windows/System32"),
        ]
    } else {
        vec![
            home.join(".local/bin"),
            home.join(".npm-global/bin"),
            home.join("Library/pnpm"),
            home.join(".bun/bin"),
            home.join(".cargo/bin"),
            home.join(".antigravity-ide/antigravity-ide/bin"),
            home.join(".antigravity/antigravity/bin"),
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ]
    }
}

fn current_username() -> String {
    let env_name = if cfg!(windows) { "USERNAME" } else { "USER" };
    std::env::var(env_name)
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            let command = if cfg!(windows) {
                "whoami.exe"
            } else {
                "/usr/bin/whoami"
            };
            let mut command = Command::new(command);
            hide_command_window(&mut command);
            command
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

fn fingerprint(bytes: &[u8]) -> String {
    let hex = bytes
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("fp:{hex}")
}

pub(crate) fn home_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(unix)]
fn set_owner_executable_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_executable_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// Owner read/write only (0600) — for the stored API key.
#[cfg(unix)]
fn set_owner_only_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod shared_link_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("aisw-link-{name}-{unique}"))
    }

    /// Windows without Developer Mode cannot create symlinks. The profile must keep its own
    /// config instead of having it deleted with nothing put back in its place.
    #[test]
    fn profile_config_survives_when_symlinks_are_unavailable() {
        let root = temp_root("nolink");
        let profile = root.join("profile");
        let default_dir = root.join("default");
        fs::create_dir_all(&profile).unwrap();
        fs::create_dir_all(&default_dir).unwrap();
        fs::write(profile.join("config.toml"), b"model = \"profile\"").unwrap();
        fs::write(default_dir.join("config.toml"), b"model = \"default\"").unwrap();
        fs::create_dir_all(profile.join("skills")).unwrap();
        fs::write(profile.join("skills").join("own.md"), b"mine").unwrap();

        link_shared_entry(
            &ToolId::Codex,
            &profile,
            "config.toml",
            false,
            &default_dir,
            false,
        );
        link_shared_entry(
            &ToolId::Codex,
            &profile,
            "skills",
            true,
            &default_dir,
            false,
        );

        assert_eq!(
            fs::read(profile.join("config.toml")).unwrap(),
            b"model = \"profile\"",
            "profile config.toml must not be deleted when it cannot be replaced by a symlink"
        );
        assert!(
            profile.join("skills").join("own.md").exists(),
            "profile skills dir must not be emptied when it cannot be replaced by a symlink"
        );
        // The default config must be left exactly as it was, not overwritten by the profile's.
        assert_eq!(
            fs::read(default_dir.join("config.toml")).unwrap(),
            b"model = \"default\""
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// When symlinks do work, the profile entry is replaced by a link to the shared target.
    #[test]
    fn profile_config_is_linked_when_symlinks_work() {
        let root = temp_root("link");
        let profile = root.join("profile");
        let default_dir = root.join("default");
        fs::create_dir_all(&profile).unwrap();
        fs::create_dir_all(&default_dir).unwrap();

        // Skip on hosts that can't create symlinks at all (Windows without Developer Mode).
        let probe_target = root.join("probe");
        let probe_link = root.join("probe-link");
        fs::write(&probe_target, b"probe").unwrap();
        if create_symlink(&probe_target, &probe_link).is_err() {
            let _ = fs::remove_dir_all(&root);
            return;
        }

        fs::write(profile.join("config.toml"), b"model = \"profile\"").unwrap();
        fs::write(default_dir.join("config.toml"), b"model = \"default\"").unwrap();

        link_shared_entry(
            &ToolId::Codex,
            &profile,
            "config.toml",
            false,
            &default_dir,
            true,
        );

        let link = profile.join("config.toml");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "profile entry should become a symlink to the shared config"
        );
        assert_eq!(fs::read(&link).unwrap(), b"model = \"default\"");
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod shell_hook_encoding_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_file(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("aisw-hook-{name}-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        dir.join("Microsoft.PowerShell_profile.ps1")
    }

    fn utf16le(text: &str) -> Vec<u8> {
        let mut out = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out
    }

    /// Windows PowerShell 5.1 writes `$PROFILE` as UTF-16LE. Appending the hook must keep every
    /// line the user had, not replace the file with just our block.
    #[test]
    fn utf16_profile_keeps_user_content_and_encoding() {
        let path = temp_file("utf16");
        let original = "Set-Alias ll Get-ChildItem\r\nImport-Module posh-git\r\n";
        fs::write(&path, utf16le(original)).unwrap();

        upsert_block(&path, "# hook body\n").unwrap();

        let written = fs::read(&path).unwrap();
        assert_eq!(&written[..2], &[0xFF, 0xFE], "encoding must be preserved");
        let (text, _) = decode_text_file(&written).unwrap();
        assert!(
            text.contains("Set-Alias ll Get-ChildItem"),
            "user content was destroyed: {text:?}"
        );
        assert!(text.contains("Import-Module posh-git"));
        assert!(text.contains("# hook body"));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn utf8_profile_round_trips_and_replaces_an_existing_block() {
        let path = temp_file("utf8");
        let original = format!("Set-Alias g git\n\n{HOOK_BEGIN}\nold body\n{HOOK_END}\ntrailing\n");
        fs::write(&path, original.as_bytes()).unwrap();

        upsert_block(&path, &format!("{HOOK_BEGIN}\nnew body\n{HOOK_END}\n")).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("Set-Alias g git"));
        assert!(text.contains("new body"));
        assert!(!text.contains("old body"));
        assert!(text.contains("trailing"));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// Undecodable content must be left exactly as it was rather than overwritten.
    #[test]
    fn undecodable_profile_is_left_untouched() {
        let path = temp_file("binary");
        let original = vec![0x00, 0xFF, 0xFE_u8, 0x80, 0x81];
        fs::write(&path, &original).unwrap();

        assert!(upsert_block(&path, "# hook body\n").is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
