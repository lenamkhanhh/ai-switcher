import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";
import test from "node:test";
import ts from "typescript";

const repoRoot = path.resolve(import.meta.dirname, "..");

async function importTypeScript(relativePath) {
  const source = await readFile(path.join(repoRoot, relativePath), "utf8");
  const { outputText } = ts.transpileModule(source, {
    compilerOptions: {
      module: ts.ModuleKind.ESNext,
      target: ts.ScriptTarget.ES2020,
    },
    fileName: relativePath,
    reportDiagnostics: true,
  });
  const url = `data:text/javascript;base64,${Buffer.from(outputText).toString("base64")}#${encodeURIComponent(relativePath)}`;
  return import(url);
}

test("Codex login prompts stay account-scoped and clear when their lifecycle ends", async () => {
  const {
    finishLoginPrompt,
    mergeLoginPrompt,
    reconcileLoginPromptWithTools,
  } = await importTypeScript("src/loginPromptLifecycle.ts");

  const initial = {
    toolId: "codex",
    accountId: "acc-1",
    attemptId: 1,
    loginUrl: "https://auth.openai.com/codex/device",
    deviceCode: null,
  };

  assert.deepEqual(
    mergeLoginPrompt(initial, {
      ...initial,
      loginUrl: null,
      deviceCode: "ABCD-EFGH",
    }),
    {
      ...initial,
      deviceCode: "ABCD-EFGH",
    },
    "a partial update for the same account keeps the known login URL",
  );

  const otherAccount = {
    ...initial,
    accountId: "acc-2",
    attemptId: 2,
    deviceCode: "WXYZ-1234",
  };
  assert.deepEqual(
    mergeLoginPrompt(initial, otherAccount),
    otherAccount,
    "a new account replaces the visible prompt instead of merging credentials",
  );

  assert.equal(
    finishLoginPrompt(initial, {
      toolId: "codex",
      accountId: "acc-2",
      attemptId: 2,
      success: false,
    }),
    initial,
    "an unrelated login process cannot dismiss the current account prompt",
  );
  assert.equal(
    finishLoginPrompt(initial, {
      toolId: "codex",
      accountId: "acc-1",
      attemptId: 1,
      success: true,
    }),
    null,
    "the matching process completion clears its prompt",
  );
  assert.equal(
    finishLoginPrompt(
      { ...initial, attemptId: 2 },
      {
        toolId: "codex",
        accountId: "acc-1",
        attemptId: 1,
        success: false,
      },
    ).attemptId,
    2,
    "an older same-account process cannot dismiss a newer prompt",
  );
  assert.equal(
    finishLoginPrompt(
      { ...initial, attemptId: 1 },
      {
        toolId: "codex",
        accountId: "acc-1",
        attemptId: 2,
        success: false,
      },
    ),
    null,
    "a newer failed attempt clears a prompt from the superseded attempt",
  );
  assert.deepEqual(
    mergeLoginPrompt(
      { ...initial, attemptId: 2, deviceCode: "NEW-CODE" },
      { ...initial, attemptId: 1, deviceCode: "OLD-CODE" },
    ),
    { ...initial, attemptId: 2, deviceCode: "NEW-CODE" },
    "a late output event from an older process cannot replace a newer prompt",
  );
  assert.deepEqual(
    mergeLoginPrompt(
      {
        ...initial,
        accountId: "acc-2",
        attemptId: 2,
        deviceCode: "NEW-CODE",
      },
      {
        ...initial,
        accountId: "acc-1",
        attemptId: 1,
        deviceCode: "OLD-CODE",
      },
    ),
    {
      ...initial,
      accountId: "acc-2",
      attemptId: 2,
      deviceCode: "NEW-CODE",
    },
    "late output from an older account cannot replace the newest global prompt",
  );

  const pendingTools = [
    {
      id: "codex",
      accounts: [{ id: "acc-1", state: "needsLogin" }],
    },
  ];
  assert.equal(reconcileLoginPromptWithTools(initial, pendingTools), initial);

  const readyTools = [
    {
      id: "codex",
      accounts: [{ id: "acc-1", state: "idle" }],
    },
  ];
  assert.equal(
    reconcileLoginPromptWithTools(initial, readyTools),
    null,
    "a successful snapshot update removes a stale device-code banner",
  );

  assert.equal(
    reconcileLoginPromptWithTools(initial, []),
    initial,
    "an unloaded snapshot cannot incorrectly dismiss a prompt",
  );
  assert.equal(
    reconcileLoginPromptWithTools(initial, [
      {
        id: "codex",
        accounts: [],
      },
    ]),
    initial,
    "a stale snapshot that has not learned about a just-created account cannot dismiss its prompt",
  );
});

test("CLI path formatting normalizes display text without changing the stored path", async () => {
  const { formatPathForDisplay, normalizePathForDisplay } =
    await importTypeScript("src/pathDisplay.ts");

  const stored = "C:\\Users\\HP\\AppData/Roaming/npm\\claude.cmd";
  assert.equal(
    normalizePathForDisplay(stored),
    "C:\\Users\\HP\\AppData\\Roaming\\npm\\claude.cmd",
  );
  assert.equal(
    formatPathForDisplay(stored),
    "~\\AppData\\Roaming\\npm\\claude.cmd",
  );
  assert.equal(stored, "C:\\Users\\HP\\AppData/Roaming/npm\\claude.cmd");

  assert.equal(formatPathForDisplay("/Users/alice/.codex"), "~/.codex");
  assert.ok(
    formatPathForDisplay(`C:\\Users\\HP\\${"very-long-segment\\".repeat(6)}auth.json`).length <= 58,
    "long display paths stay within the Settings row budget",
  );
});

test("frontend AccountState matches Rust's camelCase needsLogin wire value", async () => {
  const [frontendTypes, backendModels] = await Promise.all([
    readFile(path.join(repoRoot, "src/types.ts"), "utf8"),
    readFile(path.join(repoRoot, "src-tauri/src/models.rs"), "utf8"),
  ]);

  assert.match(frontendTypes, /AccountState\s*=\s*[^;]*"needsLogin"/s);
  assert.doesNotMatch(frontendTypes, /"needs-login"/);
  assert.match(backendModels, /enum AccountState[\s\S]*NeedsLogin/);
  assert.match(backendModels, /enum AccountState[\s\S]*?\#\[serde\(rename_all = "camelCase"\)\]/);
});
