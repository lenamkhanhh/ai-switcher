import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) => readFile(new URL(`../${path}`, import.meta.url), "utf8");

test("heavy Tauri commands are async and move blocking work off the UI thread", async () => {
  const source = await read("src-tauri/src/lib.rs");

  for (const command of [
    "fetch_gateway_models",
    "get_usage",
    "stop_api_gateway",
    "initialize_vault",
    "unlock_vault",
  ]) {
    assert.match(source, new RegExp(`async\\s+fn\\s+${command}\\s*\\(`));
  }

  const blockingCalls = source.match(/spawn_blocking/g) ?? [];
  assert.ok(blockingCalls.length >= 6, "blocking commands should use spawn_blocking");
});

test("gateway health response exposes readiness only", async () => {
  const source = await read("src-tauri/src/api_gateway.rs");
  const start = source.indexOf("async fn health");
  const end = source.indexOf("\nasync fn models", start);
  assert.ok(start >= 0 && end > start, "health handler must exist");

  const handler = source.slice(start, end);
  assert.doesNotMatch(handler, /api_gateway\.(?:combos|keys)/);
  assert.doesNotMatch(handler, /State\s*\(/);
  assert.match(handler, /"ok"\s*:\s*true/);
});

test("desktop security configuration is explicit and least-privilege", async () => {
  const config = JSON.parse(await read("src-tauri/tauri.conf.json"));
  const capability = JSON.parse(await read("src-tauri/capabilities/default.json"));

  assert.equal(typeof config.app.security.csp, "object");
  assert.match(config.app.security.csp["default-src"], /'self'/);
  assert.match(config.app.security.csp["connect-src"], /ipc:/);
  assert.ok(!capability.permissions.includes("clipboard-manager:allow-read-text"));
  assert.ok(capability.permissions.includes("clipboard-manager:allow-write-text"));
});

test("persisted state secrets request owner-only filesystem permissions", async () => {
  const source = await read("src-tauri/src/store.rs");
  const writeStart = source.indexOf("fn write_state");
  const writeEnd = source.indexOf("\n    #[cfg(test)]", writeStart);
  assert.ok(writeStart >= 0 && writeEnd > writeStart, "state writer must exist");

  const writer = source.slice(writeStart, writeEnd);
  assert.match(source, /fn set_owner_only_permissions/);
  assert.ok(
    (writer.match(/set_owner_only_permissions/g) ?? []).length >= 2,
    "staged and backup state files must both be hardened",
  );
});

test("secure credential vault is encrypted, scoped, and never part of normal app state", async () => {
  const vault = await read("src-tauri/src/vault.rs");
  const models = await read("src-tauri/src/models.rs");
  const store = await read("src-tauri/src/store.rs");
  const app = await read("src/App.tsx");
  const tauri = await read("src/tauri.ts");

  assert.match(vault, /CHACHA20_POLY1305/);
  assert.match(vault, /PBKDF2_ITERATIONS:\s*u32\s*=\s*600_000/);
  assert.match(vault, /atomic_write/);
  assert.match(vault, /set_owner_only_permissions/);
  assert.match(vault, /Secure vault is locked/);
  assert.match(vault, /record_key\(tool_id, account_id\)/);
  assert.match(store, /credentials\.vault/);
  assert.doesNotMatch(models, /pub struct StoredState[\s\S]*password/);
  assert.match(app, /Secure credentials/);
  assert.match(app, /CredentialsDialog/);
  assert.match(app, /Store details manually/);
  assert.match(tauri, /save_vault_credential/);
  assert.match(tauri, /get_vault_credential/);
  assert.match(tauri, /delete_vault_credential/);
});

test("web shell has a real favicon", async () => {
  const html = await read("index.html");
  assert.match(html, /<link[^>]+rel=["']icon["'][^>]+href=["'][^"']+["']/i);
});

test("Usage loads one backend bundle and labels its fixed activity window", async () => {
  const source = await read("src/UsageView.tsx");
  const calls = source.match(/api\.getUsage\s*\(/g) ?? [];

  assert.equal(calls.length, 1);
  assert.match(source, /Last 12 months/);
});

test("Settings switches and Usage activity states stay compact and legible", async () => {
  const css = await read("src/styles.css");
  const block = (selector) => {
    const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const match = css.match(new RegExp(`${escaped}\\s*\\{([^}]+)\\}`));
    assert.ok(match, `${selector} must have a CSS block`);
    return match[1];
  };

  assert.match(block(".switchTrack"), /width:\s*42px/);
  assert.match(block(".switchTrack"), /height:\s*24px/);
  assert.match(block(".switchThumb"), /width:\s*18px/);
  assert.match(block(".switchThumb"), /height:\s*18px/);

  const tooltip = block(".activityTooltip");
  assert.doesNotMatch(tooltip, /color:\s*var\(--text\)/);
  assert.match(tooltip, /color:\s*#f[0-9a-f]{5}/i);

  assert.match(css, /\.shell\.accountsTheme\s+\.activityCell\[data-level="0"\]/);
  assert.match(css, /\.shell\.accountsTheme--silver\s+\.activityCell\[data-level="0"\]/);
});

test("Accounts uses a responsive Tokyo workbench shell without changing the card contract", async () => {
  const source = await read("src/App.tsx");
  const css = await read("src/styles.css");

  assert.match(source, /<nav className="activityRail"/);
  assert.match(source, /<section className="accountSummary"/);
  assert.match(source, /<footer className="workbenchStatusBar"/);
  assert.match(source, /if \(quota\?\.error\) return true;/);

  const cardStart = source.indexOf("function AccountCard");
  const cardEnd = source.indexOf("\nfunction AutoSwitchBar", cardStart);
  assert.ok(cardStart >= 0 && cardEnd > cardStart, "AccountCard must remain independently reviewable");
  const card = source.slice(cardStart, cardEnd);
  assert.match(card, /accountName/);
  assert.match(card, /cmdChip/);
  assert.match(card, /<Quota quota=\{account\.quota\} \/>/);
  assert.match(card, /disabled=\{isActive \|\| needsLogin \|\| busy !== null\}/);
  assert.match(card, /<RefreshCw \/>/);
  assert.match(card, /<Terminal \/>/);
  assert.match(card, /<Pencil \/>/);
  assert.match(card, /<Trash2 \/>/);

  assert.match(css, /grid-template-columns:\s*48px 248px minmax\(0,\s*1fr\)/);
  assert.match(css, /@media \(max-width:\s*1480px\)[\s\S]*?grid-template-columns:\s*repeat\(2,\s*minmax\(0,\s*1fr\)\)/);
  assert.match(css, /@media \(max-width:\s*1040px\)[\s\S]*?grid-template-columns:\s*minmax\(0,\s*1fr\)/);
  assert.match(css, /\.shell\.accountsTheme\s+\.quotaReset\s*\{[^}]*font-size:\s*11px/s);
});

test("Accounts status uses the active source of truth and surfaces quota attention", async () => {
  const source = await read("src/App.tsx");
  const cardStart = source.indexOf("function AccountCard");
  const cardEnd = source.indexOf("\nfunction AutoSwitchBar", cardStart);
  assert.ok(cardStart >= 0 && cardEnd > cardStart, "AccountCard must remain independently reviewable");
  const card = source.slice(cardStart, cardEnd);

  assert.match(card, /const isActive = account\.id === tool\.activeAccountId;/);
  assert.doesNotMatch(card, /account\.id === tool\.activeAccountId\s*\|\|\s*account\.state === "active"/);
  assert.match(source, /currentTool\.installed \? \(attentionAccountCount > 0 \? "warn" : "ok"\) : "muted"/);
  assert.match(source, /attentionAccountCount > 0 \? "Needs attention" : "Ready"/);
});

test("Windows persisted-state permissions are explicit instead of a no-op", async () => {
  const source = await read("src-tauri/src/store.rs");
  assert.match(source, /icacls/);
  assert.match(source, /windows_current_principal/);
  assert.doesNotMatch(
    source,
    /#\[cfg\(not\(unix\)\)\]\s*fn set_owner_only_permissions\(_path: &std::path::Path\) -> Result<\(\)>\s*\{\s*Ok\(\(\)\)/s,
  );
});

test("The workbench exposes a real quit action and Ctrl+Q shortcut", async () => {
  const app = await read("src/App.tsx");
  const tauri = await read("src/tauri.ts");
  const rust = await read("src-tauri/src/lib.rs");

  assert.match(tauri, /quitApp:\s*\(\)\s*=>\s*invoke<void>\("quit_app"\)/);
  assert.match(rust, /#\[tauri::command\]\s*fn quit_app\(app: tauri::AppHandle\)/s);
  assert.match(rust, /quit_app,/);
  assert.match(app, /event\.key\.toLowerCase\(\) === "q"/);
  assert.match(app, /api\.quitApp\(\)/);
  assert.match(app, /statusBarQuit/);
});

test("Codex login prompts finish account-scoped after output is drained", async () => {
  const app = await read("src/App.tsx");
  const lifecycle = await read("src/loginPromptLifecycle.ts");
  const tools = await read("src-tauri/src/tools.rs");

  assert.match(app, /subscribeToTauriEvent<LoginPromptFinishedEvent>\(\s*"login-prompt-finished"/);
  assert.match(app, /finishLoginPrompt\(current,\s*event\.payload\)/);
  assert.match(app, /reconcileLoginPromptWithTools\(\s*loginPrompt,\s*snapshot\.tools/);
  assert.match(lifecycle, /current\.attemptId > next\.attemptId/);
  assert.match(lifecycle, /current\.attemptId < next\.attemptId/);
  assert.match(lifecycle, /finished\.attemptId >= current\.attemptId/);
  assert.match(tools, /attempt_id: u64/);
  assert.match(tools, /NEXT_LOGIN_ATTEMPT_ID\.fetch_add/);

  const waiterStart = tools.indexOf("let success = child.wait()");
  const drainStart = tools.indexOf("for reader in output_readers", waiterStart);
  const completionEmit = tools.indexOf('"login-prompt-finished"', waiterStart);
  assert.ok(waiterStart >= 0, "hidden login process must be awaited");
  assert.ok(drainStart > waiterStart, "login output readers must drain after process exit");
  assert.ok(
    completionEmit > drainStart,
    "completion must emit after output drains so a late prompt cannot reappear",
  );
});

test("K12 Codex import stays local and reduces the export to OAuth fields", async () => {
  const importer = await read("src-tauri/src/codex_import.rs");
  const importerProduction = importer.slice(0, importer.indexOf("#[cfg(test)]"));
  const appState = await read("src-tauri/src/app_state.rs");
  const app = await read("src/App.tsx");
  const tauri = await read("src/tauri.ts");

  assert.match(importerProduction, /MAX_IMPORT_BYTES/);
  assert.match(importerProduction, /accounts\.len\(\) != 1/);
  assert.match(importerProduction, /platform\.eq_ignore_ascii_case\("openai"\)/);
  assert.match(importerProduction, /"refresh_token"/);
  assert.doesNotMatch(importerProduction, /model_mapping/);
  assert.match(appState, /write_codex_auth\(&profile, &auth_json\)/);
  assert.match(appState, /codex_login_method: CodexLoginMethod::BrowserProfile/);
  assert.match(appState, /migrate_codex_imported_login_methods/);
  assert.match(appState, /Imported session/);
  assert.match(app, /Import K12/);
  assert.match(app, /Nothing is uploaded/);
  assert.match(tauri, /import_codex_account/);
});

test("Settings formats CLI paths for display without mutating stored setup paths", async () => {
  const app = await read("src/App.tsx");
  const formatter = await read("src/pathDisplay.ts");

  assert.match(app, /formatPathForDisplay\(setup!\.binaryPath!\)/);
  assert.match(app, /normalizePathForDisplay\(setup!\.binaryPath!\)/);
  assert.match(app, /<small title=\{fullPaths\}>/);
  assert.match(app, /<code title=\{normalizePathForDisplay\(value\)\}>/);
  assert.match(app, /formatPathForDisplay\(value\)/);
  assert.match(app, /title=\{normalizePathForDisplay\(item\.path\)\}/);
  assert.match(app, /formatPathForDisplay\(item\.path\)/);
  assert.ok(
    formatter.includes('path.replace(/\\//g, "\\\\")'),
    "Windows display paths must replace forward slashes only in the rendered copy",
  );
  assert.match(formatter, /compact\.slice\(-\(maxLength - 1\)\)/);
});

test("Recovery banners are dismissible and expired accounts are labelled truthfully", async () => {
  const app = await read("src/App.tsx");

  assert.match(app, /if \(state === "needsLogin"\) return "Needs login";/);
  assert.doesNotMatch(app, /if \(state === "needsLogin"\) return "Signing in";/);
  assert.match(app, /title="Dismiss recovery"/);
  assert.match(app, /api\.cancelResumeJob\(visibleRecoveryJob\.id\)/);
});
