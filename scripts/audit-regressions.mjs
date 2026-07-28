import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) => readFile(new URL(`../${path}`, import.meta.url), "utf8");

test("heavy Tauri commands are async and move blocking work off the UI thread", async () => {
  const source = await read("src-tauri/src/lib.rs");

  for (const command of ["fetch_gateway_models", "get_usage", "stop_api_gateway"]) {
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
