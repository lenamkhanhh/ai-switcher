import {
  AlertTriangle,
  ArrowDown,
  ArrowUp,
  BarChart3,
  Boxes,
  Check,
  ChevronDown,
  CircleHelp,
  Code2,
  Copy,
  Info,
  KeyRound,
  Layers,
  Loader2,
  Eye,
  EyeOff,
  LockKeyhole,
  LogIn,
  LogOut,
  Minus,
  Pencil,
  Plus,
  RefreshCw,
  RotateCcw,
  Server,
  Settings,
  ShieldAlert,
  Square,
  Sun,
  Terminal,
  Trash2,
  Users,
  X,
  Zap,
  Moon,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  finishLoginPrompt,
  mergeLoginPrompt,
  reconcileLoginPromptWithTools,
  type LoginPromptEvent,
  type LoginPromptFinishedEvent,
} from "./loginPromptLifecycle";
import {
  formatPathForDisplay,
  normalizePathForDisplay,
} from "./pathDisplay";
import { api, isTauri, subscribeToTauriEvent } from "./tauri";

/// `getCurrentWindow()` reads `window.__TAURI_INTERNALS__.metadata` synchronously and is not
/// guarded, so outside the Tauri shell it throws. Thrown from an effect with no error boundary
/// above it, that unmounts the whole React root — `npm run dev` in a plain browser rendered a
/// white screen. The mock layer in `./tauri` only covers `invoke`, not the window API.
// Runs an action at most once at a time. Modal submits are slow (creating an account spawns an
// interactive login), and the dialog only closes once the call resolves, so a second click on a
// button that looks idle would fire the whole request again.
function useSingleFlight(action: () => void | Promise<void>) {
  const [running, setRunning] = useState(false);
  const run = () => {
    if (running) return;
    setRunning(true);
    void Promise.resolve()
      .then(action)
      .finally(() => setRunning(false));
  };
  return [run, running] as const;
}

const nativeWindow = () => {
  if (!("__TAURI_INTERNALS__" in window)) return null;
  try {
    return getCurrentWindow();
  } catch {
    return null;
  }
};

async function openExternalUrl(url: string) {
  if (isTauri) {
    try {
      await openUrl(url);
      return;
    } catch {
      // Fall back to the browser path if the native opener plugin is unavailable.
    }
  }
  window.open(url, "_blank", "noopener,noreferrer");
}

type AccountTheme = "black" | "silver";

const accountThemeStorageKey = "ai-account-switcher:accounts-theme";

function initialAccountTheme(): AccountTheme {
  try {
    return window.localStorage.getItem(accountThemeStorageKey) === "silver" ? "silver" : "black";
  } catch {
    return "black";
  }
}

function AccountThemeToggle({
  value,
  onChange,
}: {
  value: AccountTheme;
  onChange: (theme: AccountTheme) => void;
}) {
  return (
    <div className="accountThemeToggle" role="group" aria-label="Application appearance">
      <button
        type="button"
        className={value === "silver" ? "selected" : ""}
        aria-label="Use Shiro light appearance"
        aria-pressed={value === "silver"}
        title="Shiro · light workbench"
        onClick={() => onChange("silver")}
      >
        <Sun />
      </button>
      <button
        type="button"
        className={value === "black" ? "selected" : ""}
        aria-label="Use Kuro dark appearance"
        aria-pressed={value === "black"}
        title="Kuro · dark workbench"
        onClick={() => onChange("black")}
      >
        <Moon />
      </button>
    </div>
  );
}
import { UsageView } from "./UsageView";
import logoUrl from "./assets/logo-luxury.png";
import type {
  Account,
  AddAccountInput,
  AddApiAccountInput,
  ApiGatewayCombo,
  ApiPoolAccountState,
  ApiRotationStrategy,
  ApiUsageReport,
  AppSnapshot,
  BinaryCandidate,
  BulkImportCodexAccountsInput,
  BulkLoginCodexInput,
  CodexLoginMethod,
  ConfigCandidate,
  CreateApiGatewayKeyInput,
  DetectionReport,
  ImportCodexAccountInput,
  SaveApiGatewayComboInput,
  SetApiGatewayAccountInput,
  SetToolSetupInput,
  StartApiGatewayInput,
  ToolSetup,
  ToolId,
  ToolStatus,
  ResumeJob,
  ResumeJobStatus,
  VaultCredential,
  VaultCredentialInput,
  VaultStatus,
} from "./types";

/** Host shown on an API account card (best-effort parse of the gateway URL). */
function gatewayHost(baseUrl: string) {
  try {
    return new URL(baseUrl).host;
  } catch {
    return baseUrl;
  }
}

const toolDescriptions: Record<ToolId, string> = {
  claude:
    "The bare `claude` command follows the selected account (Use button). Each account also has its own `claude-…` command to run in parallel in another terminal.",
  codex:
    "The bare `codex` command follows the selected account (Use button). Each account also has its own `codex-…` command to run in parallel in another terminal.",
  antigravity:
    "Save Antigravity IDE login sessions to switch between them. Click Use: the app quits the IDE, loads that account's token, then reopens the IDE.",
};

const emptySnapshot: AppSnapshot = {
  tools: [],
  disclaimerAccepted: true,
  autoSwitch: false,
  autoSwitchThreshold: 100,
  autoSwitchSettings: {},
  autoPrime: {},
  autoPingEnabled: false,
  launchWithWindows: false,
  autoPing: {},
  resumeJobs: [],
  toolSetups: {},
  apiGateway: {
    config: {
      bindHost: "127.0.0.1",
      port: 8783,
      quotaThreshold: 95,
      maxRetries: 3,
      rotationStrategy: "roundRobin",
      keys: [],
      combos: [],
      accounts: [],
      modelRegistry: [],
      virtualClaudeEnabled: false,
      virtualCodexEnabled: false,
    },
    status: { state: "stopped", baseUrl: "http://127.0.0.1:8783", error: null },
  },
};

const emptyVaultStatus: VaultStatus = {
  configured: false,
  unlocked: false,
  recordCount: 0,
};

function recoveryStatusLabel(status: ResumeJobStatus): string {
  switch (status) {
    case "awaitingTerminal": return "Waiting for turn";
    case "detected":
    case "candidateVerified":
    case "queued": return "Checking account";
    case "checkpointing":
    case "checkpointed": return "Checkpointing";
    case "switching": return "Switching";
    case "threadResuming":
    case "threadResumed":
    case "goalVerified": return "Resuming thread";
    case "turnStarting": return "Starting turn";
    case "turnStarted":
    case "recovered":
    case "monitoring":
    case "resumed": return "Running";
    case "retrying": return "Retrying";
    case "needsAttention":
    case "failed": return "Needs attention";
    case "completed": return "Completed";
    case "cancelled": return "Cancelled";
    default: return "Checking account";
  }
}

function recoveryDefaultMessage(job: ResumeJob): string {
  if (job.status === "awaitingTerminal") {
    return "Waiting for the current Codex turn to finish before switching accounts.";
  }
  if (job.status === "retrying") {
    return job.error ?? "The selected model is temporarily at capacity. Retrying without switching accounts…";
  }
  if (job.status === "needsAttention" || job.status === "failed") {
    return job.error ?? "Resume could not be verified.";
  }
  if (job.status === "recovered" || job.status === "monitoring" || job.status === "resumed") {
    return "Task resumed and is running.";
  }
  return "Codex is preparing a durable task recovery.";
}

interface AutoSwitchHandoffEvent {
  path: string;
  prompt: string;
}

export function App() {
  const [snapshot, setSnapshot] = useState<AppSnapshot>(emptySnapshot);
  const [selectedTool, setSelectedTool] = useState<ToolId>("codex");
  const [view, setView] = useState<"accounts" | "api" | "usage" | "settings">("accounts");
  const [accountTheme, setAccountTheme] = useState<AccountTheme>(initialAccountTheme);
  const [vaultStatus, setVaultStatus] = useState<VaultStatus>(emptyVaultStatus);
  const [windowFullscreen, setWindowFullscreen] = useState(false);
  const [windowMaximized, setWindowMaximized] = useState(false);
  // One unified notification channel for the whole app: success + error both render as a
  // top-right toast (see the Toasts renderer). `notify` is the single entry point.
  const [toasts, setToasts] = useState<
    { id: number; kind: "success" | "error" | "info"; text: string }[]
  >([]);
  const notify = useCallback((text: string, kind: "success" | "error" | "info" = "success") => {
    if (!text) return;
    const id = nextToastId();
    setToasts((current) => [...current.slice(-3), { id, kind, text }]);
    window.setTimeout(() => setToasts((current) => current.filter((item) => item.id !== id)), 4500);
  }, []);
  const dismissToast = useCallback(
    (id: number) => setToasts((current) => current.filter((item) => item.id !== id)),
    [],
  );
  const setToast = useCallback((text: string | null) => text && notify(text, "success"), [notify]);
  const setError = useCallback((text: string | null) => text && notify(text, "error"), [notify]);
  const [busy, setBusy] = useState<string | null>(null);
  const [refreshingAccounts, setRefreshingAccounts] = useState<Set<string>>(new Set());
  const [dialog, setDialog] = useState<"add" | "rename" | "launcher" | "setup" | "credentials" | null>(null);
  const [selectedAccount, setSelectedAccount] = useState<Account | null>(null);
  const [switchNotice, setSwitchNotice] = useState<string | null>(null);
  const [autoSwitchBanner, setAutoSwitchBanner] = useState<string | null>(null);
  const [loginPrompt, setLoginPrompt] = useState<LoginPromptEvent | null>(null);
  const [version, setVersion] = useState("");
  const [setupPrompted, setSetupPrompted] = useState<Set<ToolId>>(new Set());
  // Last Codex device code we raised a toast for, so a repeated event can't re-announce it.
  const announcedDeviceCode = useRef<string | null>(null);
  const visibleRecoveryJob = useMemo(
    () =>
      [...snapshot.resumeJobs]
        .reverse()
        .find((job) => job.status !== "cancelled" && job.status !== "completed"),
    [snapshot.resumeJobs],
  );

  useEffect(() => {
    try {
      window.localStorage.setItem(accountThemeStorageKey, accountTheme);
    } catch {
      // Theme preference is cosmetic; the Accounts tab still works when storage is unavailable.
    }
  }, [accountTheme]);

  useEffect(() => {
    getVersion()
      .then(setVersion)
      .catch(() => {});
  }, []);

  useEffect(() => {
    const appWindow = nativeWindow();
    if (!appWindow) return;
    let disposed = false;
    let unlistenResize: (() => void) | undefined;
    const syncWindowState = async () => {
      try {
        const [fullscreen, maximized] = await Promise.all([
          appWindow.isFullscreen(),
          appWindow.isMaximized(),
        ]);
        if (!disposed) {
          setWindowFullscreen(fullscreen);
          setWindowMaximized(maximized);
        }
      } catch {
        // Browser/dev fallback has no native window state.
      }
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (
        (event.ctrlKey || event.metaKey) &&
        event.key.toLowerCase() === "q" &&
        !event.repeat
      ) {
        event.preventDefault();
        void api.quitApp().catch(() => {});
        return;
      }
      if (event.key !== "F11" || event.repeat) return;
      event.preventDefault();
      void appWindow
        .isFullscreen()
        .then((fullscreen) => appWindow.setFullscreen(!fullscreen))
        .then(syncWindowState)
        .catch(() => {});
    };
    window.addEventListener("keydown", onKeyDown);
    void syncWindowState();
    void appWindow.onResized(() => void syncWindowState()).then((fn) => {
      if (disposed) fn();
      else unlistenResize = fn;
    });
    return () => {
      disposed = true;
      window.removeEventListener("keydown", onKeyDown);
      unlistenResize?.();
    };
  }, []);

  const toggleWindowMaximize = useCallback(async () => {
    const appWindow = nativeWindow();
    if (!appWindow) return;
    try {
      await appWindow.toggleMaximize();
      setWindowMaximized(await appWindow.isMaximized());
    } catch {
      // Native controls are unavailable in browser/dev fallback.
    }
  }, []);

  const load = useCallback(async () => {
    setBusy("load");
    setError(null);
    try {
      setSnapshot(await api.loadSnapshot());
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(null);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const refreshVaultStatus = useCallback(async () => {
    try {
      setVaultStatus(await api.getVaultStatus());
    } catch {
      // The browser fallback and older installed builds may not expose the vault command yet.
    }
  }, []);

  useEffect(() => {
    void refreshVaultStatus();
    const timer = window.setInterval(() => {
      void refreshVaultStatus();
    }, 30_000);
    return () => window.clearInterval(timer);
  }, [refreshVaultStatus]);

  // Backend pushes a fresh snapshot when a login finishes / a background auto-switch happens.
  useEffect(() => {
    return subscribeToTauriEvent<AppSnapshot>("snapshot-changed", (event) => {
      setSnapshot(event.payload);
    });
  }, []);

  // Stable identity for the set of CLI tools to poll. `snapshot` is replaced on every refresh, so
  // the poller below must not depend on `snapshot.tools` directly.
  const cliToolIdKey = useMemo(
    () =>
      snapshot.tools
        .filter((tool) => tool.id !== "antigravity")
        .map((tool) => tool.id)
        .join(","),
    [snapshot.tools],
  );

  // Auto-refresh real quota only while the app window is FOCUSED. Losing focus (typing in another
  // app, even if this window is still visible) stops the polling; refocusing refreshes immediately
  // and resumes. `refresh_tool` fetches live usage; an `inFlight` guard coalesces overlapping
  // refreshes and the 60s per-account backend cache absorbs the rest, so fast focus toggling can't
  // hammer the provider's usage endpoint.
  useEffect(() => {
    let timer: number | undefined;
    let cancelled = false; // set on cleanup: blocks any async callback that resolves post-unmount
    let inFlight = false; // skip a new tick while the previous refresh pair is still running
    let unlisten: (() => void) | undefined;

    const tick = () => {
      if (cancelled || inFlight) return;
      inFlight = true;
      const toolIds = cliToolIdKey.split(",").filter(Boolean) as ToolId[];
      Promise.allSettled(toolIds.map((toolId) => api.refreshTool(toolId).then(setSnapshot))).finally(() => {
        inFlight = false;
      });
    };
    const start = () => {
      if (cancelled || timer !== undefined) return;
      tick(); // refresh right away on (re)gaining focus
      timer = window.setInterval(tick, 150_000);
    };
    const stop = () => {
      if (timer !== undefined) {
        window.clearInterval(timer);
        timer = undefined;
      }
    };

    // Register the focus listener BEFORE reading the current focus, so we can't miss a blur that
    // happens between the isFocused() read and the listener being installed.
    const appWindow = nativeWindow();
    if (!appWindow) {
      // No native focus events in the browser fallback: poll on the plain interval instead.
      start();
      return () => {
        cancelled = true;
        stop();
      };
    }
    void appWindow
      .onFocusChanged(({ payload: focused }) => {
        if (cancelled) return;
        if (focused) start();
        else stop();
      })
      .then((fn) => {
        if (cancelled) {
          fn(); // effect already cleaned up while we were registering — unlisten immediately
          return;
        }
        unlisten = fn;
        // Now that the listener is live, sync to the current focus state.
        void appWindow.isFocused().then((focused) => {
          if (!cancelled && focused) start();
        });
      });

    return () => {
      cancelled = true;
      stop();
      unlisten?.();
    };
    // Keyed on the tool ids, NOT on `snapshot.tools`: every refresh_tool response is a brand-new
    // snapshot object, so depending on the array itself would tear down and re-run this effect on
    // data it produced, re-firing `tick()` immediately and refreshing in a tight loop.
  }, [cliToolIdKey]);

  // Banner when the app auto-switches an account because quota ran out.
  useEffect(() => {
    return subscribeToTauriEvent<string>("auto-switched", (event) => {
      setAutoSwitchBanner(event.payload);
    });
  }, []);

  useEffect(() => {
    return subscribeToTauriEvent<AutoSwitchHandoffEvent>("auto-switch-handoff", (event) => {
      void copyToClipboard(event.payload.prompt).then((ok) => {
        notify(
          ok
            ? "Auto-switch handoff copied. Open a new Codex task and paste to continue."
            : `Auto-switch handoff saved: ${event.payload.path}`,
          ok ? "success" : "info",
        );
      });
    });
  }, [notify]);

  useEffect(() => {
    return subscribeToTauriEvent<LoginPromptEvent>("login-prompt", (event) => {
      setLoginPrompt((current) => mergeLoginPrompt(current, event.payload));
      // Announced outside the updater: React double-invokes updaters under StrictMode, which
      // fired this toast twice in dev.
      const code = event.payload.deviceCode;
      if (code && code !== announcedDeviceCode.current) {
        announcedDeviceCode.current = code;
        notify("Codex login code is ready in the app.", "info");
      }
    });
  }, [notify]);

  useEffect(() => {
    return subscribeToTauriEvent<LoginPromptFinishedEvent>(
      "login-prompt-finished",
      (event) => {
        setLoginPrompt((current) =>
          finishLoginPrompt(current, event.payload),
        );
      },
    );
  }, []);

  useEffect(() => {
    if (!loginPrompt) return;
    const reconciled = reconcileLoginPromptWithTools(
      loginPrompt,
      snapshot.tools,
    );
    if (!reconciled) {
      setLoginPrompt(null);
    }
  }, [loginPrompt, snapshot.tools]);

  useEffect(() => {
    if (!loginPrompt) announcedDeviceCode.current = null;
  }, [loginPrompt]);

  useEffect(() => {
    if (!snapshot.disclaimerAccepted || dialog) return;
    const needsSetup = snapshot.tools.find(
      (tool) => tool.id !== "antigravity" && tool.installed && !snapshot.toolSetups[tool.id] && !setupPrompted.has(tool.id),
    );
    if (!needsSetup) return;
    setSelectedTool(needsSetup.id);
    setDialog("setup");
    setSetupPrompted((prev) => new Set(prev).add(needsSetup.id));
  }, [dialog, setupPrompted, snapshot]);

  useEffect(() => {
    if (!snapshot.tools.length) return;
    if (!snapshot.tools.some((tool) => tool.id === selectedTool)) {
      setSelectedTool(snapshot.tools[0].id);
    }
  }, [selectedTool, snapshot.tools]);

  const currentTool = useMemo(
    () => snapshot.tools.find((tool) => tool.id === selectedTool) ?? snapshot.tools[0],
    [selectedTool, snapshot.tools],
  );
  const visibleCurrentAccounts = useMemo(
    () =>
      currentTool?.accounts.filter(
        (account) => !(currentTool.id === "codex" && account.isDefault),
      ) ?? [],
    [currentTool],
  );
  const currentAutoSwitch = currentTool
    ? snapshot.autoSwitchSettings[currentTool.id] ?? {
        enabled: snapshot.autoSwitch,
        threshold: snapshot.autoSwitchThreshold,
      }
    : { enabled: false, threshold: 100 };
  const activeAccount = visibleCurrentAccounts.find(
    (account) => account.id === currentTool?.activeAccountId,
  );
  const reportingAccountCount = visibleCurrentAccounts.filter(
    (account) =>
      account.quota &&
      (account.quota.fiveHour.percentUsed !== null || account.quota.weekly.percentUsed !== null),
  ).length;
  const attentionAccountCount = visibleCurrentAccounts.filter((account) => {
    if (account.state === "needsLogin" || account.state === "exhausted") return true;
    const quota = account.quota;
    if (quota?.error) return true;
    return (
      (quota?.fiveHour.percentUsed ?? 0) >= 90 ||
      (quota?.weekly.percentUsed ?? 0) >= 90
    );
  }).length;
  const accountCountLabel = `${visibleCurrentAccounts.length} account${visibleCurrentAccounts.length === 1 ? "" : "s"}`;
  const attentionLabel =
    attentionAccountCount > 0
      ? `${attentionAccountCount} need${attentionAccountCount === 1 ? "s" : ""} attention`
      : "Healthy";
  const viewLabel =
    view === "accounts"
      ? `${currentTool?.name ?? "Accounts"} / Accounts`
      : view === "api"
        ? "Workspace / API Gateway"
        : view === "usage"
          ? "Workspace / Usage"
          : "System / Settings";

  useEffect(() => {
    if (currentTool && currentTool.id !== selectedTool) {
      setSelectedTool(currentTool.id);
    }
  }, [currentTool, selectedTool]);

  // Switching tool tab → clear the previous tool's switch notice (a Claude notice
  // must not leak onto Codex/Antigravity).
  useEffect(() => {
    setSwitchNotice(null);
  }, [selectedTool]);

  const run = async (
    key: string,
    task: () => Promise<AppSnapshot>,
    success?: string,
    notice?: string,
  ): Promise<boolean> => {
    setBusy(key);
    setError(null);
    try {
      const next = await task();
      setSnapshot(next);
      if (success) setToast(success);
      if (notice) setSwitchNotice(notice);
      return true;
    } catch (err) {
      setError(errorMessage(err));
      return false;
    } finally {
      setBusy(null);
    }
  };

  const vaultAction = async <T,>(task: () => Promise<T>, success?: string): Promise<T | null> => {
    setBusy("vault");
    setError(null);
    try {
      const result = await task();
      if (success) notify(success, "success");
      return result;
    } catch (err) {
      void refreshVaultStatus();
      notify(errorMessage(err), "error");
      return null;
    } finally {
      setBusy(null);
    }
  };

  const initializeVault = async (passkey: string) => {
    const next = await vaultAction(() => api.initializeVault(passkey), "Secure vault created");
    if (next) {
      setVaultStatus(next);
      return true;
    }
    return false;
  };

  const unlockVault = async (passkey: string) => {
    const next = await vaultAction(() => api.unlockVault(passkey), "Secure vault unlocked");
    if (next) {
      setVaultStatus(next);
      return true;
    }
    return false;
  };

  const lockVault = async () => {
    const ok = await vaultAction(async () => {
      await api.lockVault();
      return true;
    }, "Secure vault locked");
    if (ok) setVaultStatus((current) => ({ ...current, unlocked: false, recordCount: 0 }));
  };

  const loadVaultCredential = async (account: Account) =>
    vaultAction(
      () => api.getVaultCredential(account.toolId, account.id),
    );

  const saveVaultCredential = async (input: VaultCredentialInput) => {
    const saved = await vaultAction(() => api.saveVaultCredential(input), "Credentials saved securely");
    if (saved) {
      await refreshVaultStatus();
      return true;
    }
    return false;
  };

  const refreshOneAccount = async (toolId: ToolId, accountId: string) => {
    setRefreshingAccounts((prev) => new Set([...prev, accountId]));
    try {
      const updated = await api.refreshAccount(toolId, accountId);
      // Merge only the refreshed account into the current snapshot so concurrent
      // refreshes don't clobber each other's results.
      setSnapshot((prev) => {
        const updatedAccount = updated.tools
          .find((t) => t.id === toolId)
          ?.accounts.find((a) => a.id === accountId);
        if (!updatedAccount) return prev;
        return {
          ...prev,
          tools: prev.tools.map((t) =>
            t.id !== toolId
              ? t
              : { ...t, accounts: t.accounts.map((a) => (a.id !== accountId ? a : updatedAccount)) },
          ),
        };
      });
    } catch {
      // quota error is surfaced inside account.quota.error — no global toast needed
    } finally {
      setRefreshingAccounts((prev) => {
        const next = new Set(prev);
        next.delete(accountId);
        return next;
      });
    }
  };

  const refresh = () => {
    const tool = snapshot.tools.find((t) => t.id === selectedTool);
    if (!tool) return;
    // Fire all account refreshes in parallel so each card updates independently.
    for (const account of tool.accounts) {
      if (account.state !== "needsLogin" && !account.apiProvider) {
        void refreshOneAccount(selectedTool, account.id);
      }
    }
  };

  const switchAccount = (tool: ToolStatus, account: Account) =>
    run(
      `switch-${account.id}`,
      () => api.switchAccount({ toolId: tool.id, accountId: account.id }),
      tool.id === "antigravity"
        ? `Switched to: ${account.name}`
        : `Now using: ${account.name} (${tool.name})`,
      tool.id === "antigravity"
        ? `Loaded account ${account.name}. Antigravity IDE is restarting to apply.`
        : tool.id === "codex"
          ? `Selected ${account.name} for CLI and VS Code. Run "Developer: Reload Window" in VS Code, then open a new Codex thread.`
          : `Selected ${account.name}. A new terminal running \`${tool.id}\` uses it right away; in an open terminal run \`aisw\` to sync.`,
    );

  return (
    <main
      className={`shell accountsTheme accountsTheme--${accountTheme} ${windowFullscreen ? "isFullscreen" : ""}`}
    >
      {!windowFullscreen && (
        <header className="topbar">
          <div
            className="titlebarDrag"
            data-tauri-drag-region
            onDoubleClick={() => void toggleWindowMaximize()}
          >
            <img src={logoUrl} alt="" />
            <span>AI Account Switcher</span>
          </div>
          <div className="workbenchCommand" aria-label={`Current workspace: ${viewLabel}`}>
            <Code2 aria-hidden />
            <span>{viewLabel}</span>
            <kbd>F11</kbd>
          </div>
          <div className="windowControls">
            <AccountThemeToggle value={accountTheme} onChange={setAccountTheme} />
            <button
              type="button"
              aria-label="Minimize"
              title="Minimize"
              onClick={() => void nativeWindow()?.minimize().catch(() => {})}
            >
              <Minus />
            </button>
            <button
              type="button"
              aria-label={windowMaximized ? "Restore" : "Maximize"}
              title={windowMaximized ? "Restore" : "Maximize"}
              onClick={() => void toggleWindowMaximize()}
            >
              {windowMaximized ? <Copy /> : <Square />}
            </button>
            <button
              type="button"
              className="close"
              aria-label="Close"
              title="Close"
              onClick={() => void nativeWindow()?.close().catch(() => {})}
            >
              <X />
            </button>
          </div>
        </header>
      )}

      {!snapshot.disclaimerAccepted && (
        <section className="notice">
          <ShieldAlert />
          <span>
            Note: using multiple subscription accounts may violate the provider's terms. This app
            only manages logins — use at your own discretion.
          </span>
          <button onClick={() => run("disclaimer", api.acceptDisclaimer)}>Got it</button>
        </section>
      )}

      <div className="toastStack">
        {toasts.map((item) => (
          <section
            className={`toast ${item.kind}`}
            role={item.kind === "error" ? "alert" : "status"}
            key={item.id}
          >
            {item.kind === "error" ? (
              <AlertTriangle />
            ) : item.kind === "info" ? (
              <Info />
            ) : (
              <Check />
            )}
            <span>{item.text}</span>
            <button className="iconButton" onClick={() => dismissToast(item.id)} title="Dismiss">
              <X />
            </button>
          </section>
        ))}
      </div>

      {autoSwitchBanner && (
        <section className="autoBanner">
          <RotateCcw />
          <span>{autoSwitchBanner}</span>
          <button className="iconButton" onClick={() => setAutoSwitchBanner(null)} title="Dismiss">
            <X />
          </button>
        </section>
      )}

      {visibleRecoveryJob && (
        <section
          className={`recoveryBanner ${visibleRecoveryJob.status === "needsAttention" || visibleRecoveryJob.status === "failed" ? "attention" : ""}`}
          role={visibleRecoveryJob.status === "needsAttention" || visibleRecoveryJob.status === "failed" ? "alert" : "status"}
        >
          <RotateCcw />
          <div className="recoveryBannerText">
            <strong>{recoveryStatusLabel(visibleRecoveryJob.status)}</strong>
            <span>{recoveryDefaultMessage(visibleRecoveryJob)}</span>
          </div>
          {(visibleRecoveryJob.status === "turnStarted" ||
            visibleRecoveryJob.status === "recovered" ||
            visibleRecoveryJob.status === "monitoring" ||
            visibleRecoveryJob.status === "resumed") && (
            <button
              type="button"
              disabled={busy === `resume-open-${visibleRecoveryJob.id}`}
              onClick={() => void run(`resume-open-${visibleRecoveryJob.id}`, () => api.openResumeJob(visibleRecoveryJob.id))}
            >
              Open thread
            </button>
          )}
          {(visibleRecoveryJob.status === "needsAttention" || visibleRecoveryJob.status === "failed") && (
            <button
              type="button"
              disabled={busy === `resume-retry-${visibleRecoveryJob.id}`}
              onClick={() => void run(`resume-retry-${visibleRecoveryJob.id}`, () => api.retryResumeJob(visibleRecoveryJob.id))}
            >
              Retry
            </button>
          )}
          <button
            type="button"
            className="iconButton"
            disabled={busy === `resume-cancel-${visibleRecoveryJob.id}`}
            title="Dismiss recovery"
            onClick={() => void run(`resume-cancel-${visibleRecoveryJob.id}`, () => api.cancelResumeJob(visibleRecoveryJob.id))}
          >
            <X />
          </button>
        </section>
      )}

      {loginPrompt && (
        <section className="loginPromptPanel" role="status">
          <KeyRound />
          <div className="loginPromptText">
            <strong>Codex sign-in</strong>
            <span>
              Open the device page, then enter this one-time code:
              <code>{loginPrompt.deviceCode ?? "Waiting for code..."}</code>
            </span>
          </div>
          <button
            type="button"
            onClick={() => void openExternalUrl(loginPrompt.loginUrl ?? "https://auth.openai.com/codex/device")}
          >
            <LogIn /> Open
          </button>
          <button
            type="button"
            className="iconButton"
            disabled={!loginPrompt.deviceCode}
            title="Copy code"
            onClick={async () => {
              if (!loginPrompt.deviceCode) return;
              const ok = await copyToClipboard(loginPrompt.deviceCode);
              notify(ok ? "Copied login code." : "Couldn't access the clipboard.", ok ? "success" : "error");
            }}
          >
            <Copy />
          </button>
          <button className="iconButton" onClick={() => setLoginPrompt(null)} title="Dismiss">
            <X />
          </button>
        </section>
      )}

      <div className="workspace">
        <nav className="activityRail" aria-label="Primary views">
          <button
            type="button"
            className={view === "accounts" ? "selected" : ""}
            aria-current={view === "accounts" ? "page" : undefined}
            aria-label="Accounts"
            title="Accounts"
            onClick={() => setView("accounts")}
          >
            <Users />
          </button>
          <button
            type="button"
            className={view === "api" ? "selected" : ""}
            aria-current={view === "api" ? "page" : undefined}
            aria-label="API gateway"
            title="API gateway"
            onClick={() => setView("api")}
          >
            <Server />
          </button>
          <button
            type="button"
            className={view === "usage" ? "selected" : ""}
            aria-current={view === "usage" ? "page" : undefined}
            aria-label="Usage"
            title="Usage"
            onClick={() => setView("usage")}
          >
            <BarChart3 />
          </button>
          <span className="activityRailSpacer" />
          <button
            type="button"
            className={view === "settings" ? "selected" : ""}
            aria-current={view === "settings" ? "page" : undefined}
            aria-label="Settings"
            title="Settings"
            onClick={() => setView("settings")}
          >
            <Settings />
          </button>
        </nav>
        <aside className="sidebar">
          <div className="sidebarBrand">
            <span className="sidebarBrandMark" aria-hidden>
              <img src={logoUrl} alt="" />
            </span>
            <span className="sidebarBrandCopy">
              <strong>AI SWITCHER</strong>
              <small>Tokyo workbench</small>
            </span>
            <span className="sidebarBrandRule" aria-hidden />
          </div>

          <div className="sidebarNav">
            <div className="sidebarNavGroup">
              <span className="sidebarNavLabel">Accounts</span>
              {snapshot.tools.map((tool, index) => (
                <button
                  aria-current={view === "accounts" && tool.id === selectedTool ? "page" : undefined}
                  className={`toolTab ${view === "accounts" && tool.id === selectedTool ? "selected" : ""}`}
                  key={tool.id}
                  onClick={() => {
                    setView("accounts");
                    setSelectedTool(tool.id);
                  }}
                >
                  <span className="sidebarNavIcon" aria-hidden>
                    {tool.id === "codex" ? <Code2 /> : <Boxes />}
                  </span>
                  <span className="sidebarNavCopy">
                    <strong>{tool.name}</strong>
                    <small>{tool.installed ? "Accounts" : "Tool not installed"}</small>
                  </span>
                  <span className="sidebarNavIndex" aria-hidden>
                    {String(index + 1).padStart(2, "0")}
                  </span>
                </button>
              ))}
            </div>
            <div className="sidebarNavGroup">
              <span className="sidebarNavLabel">Workspace</span>
              <button
                aria-current={view === "api" ? "page" : undefined}
                className={`toolTab ${view === "api" ? "selected" : ""}`}
                onClick={() => setView("api")}
              >
                <span className="sidebarNavIcon" aria-hidden>
                  <Server />
                </span>
                <span className="sidebarNavCopy">
                  <strong>API</strong>
                  <small>{snapshot.apiGateway.status.state === "running" ? "Server running" : "Local gateway"}</small>
                </span>
                <span className="sidebarNavIndex" aria-hidden>
                  {String(snapshot.tools.length + 1).padStart(2, "0")}
                </span>
              </button>
              <button
                aria-current={view === "usage" ? "page" : undefined}
                className={`toolTab ${view === "usage" ? "selected" : ""}`}
                onClick={() => setView("usage")}
              >
                <span className="sidebarNavIcon" aria-hidden>
                  <BarChart3 />
                </span>
                <span className="sidebarNavCopy">
                  <strong>Usage</strong>
                  <small>Token &amp; cost</small>
                </span>
                <span className="sidebarNavIndex" aria-hidden>
                  {String(snapshot.tools.length + 2).padStart(2, "0")}
                </span>
              </button>
            </div>
            <div className="sidebarNavGroup">
              <span className="sidebarNavLabel">System</span>
              <button
                aria-current={view === "settings" ? "page" : undefined}
                className={`toolTab ${view === "settings" ? "selected" : ""}`}
                onClick={() => setView("settings")}
              >
                <span className="sidebarNavIcon" aria-hidden>
                  <Settings />
                </span>
                <span className="sidebarNavCopy">
                  <strong>Settings</strong>
                  <small>CLI paths</small>
                </span>
                <span className="sidebarNavIndex" aria-hidden>
                  {String(snapshot.tools.length + 3).padStart(2, "0")}
                </span>
              </button>
            </div>
          </div>

          <div className="sidebarEtching" aria-hidden>
            <img src={logoUrl} alt="" />
          </div>

          <div className="sidebarFoot">
            <button
              className="sidebarCredit"
              onClick={() => void openExternalUrl("https://hoangphan.blog/")}
              title="https://hoangphan.blog/"
            >
              {version && <span>v{version}</span>}
            </button>
          </div>
        </aside>

        {view === "usage" && <UsageView />}

        {view === "api" && (
          <ApiGatewayView
            snapshot={snapshot}
            busy={busy !== null}
            onStart={(input) => run("api-start", () => api.startApiGateway(input), `API server is running at http://${input.bindHost}:${input.port}`)}
            onStop={() => run("api-stop", api.stopApiGateway)}
            onCreateKey={async (input) => {
              setBusy("api-key");
              try {
                const result = await api.createApiGatewayKey(input);
                setSnapshot(result.snapshot);
                const copied = await copyToClipboard(result.secret);
                notify(
                  copied
                    ? "API key created and copied to clipboard."
                    : "API key created — copy it now, it is shown only once.",
                  "success",
                );
                return result.secret;
              } catch (err) {
                notify(errorMessage(err), "error");
                return null;
              } finally {
                setBusy(null);
              }
            }}
            onDeleteKey={(keyId) => run("api-key-delete", () => api.deleteApiGatewayKey(keyId))}
            onCopyKey={async (keyId) => {
              try {
                const secret = await api.revealApiGatewayKey(keyId);
                const ok = await copyToClipboard(secret);
                notify(ok ? "API key copied to clipboard." : "Couldn't access the clipboard.", ok ? "success" : "error");
              } catch (err) {
                notify(errorMessage(err), "error");
              }
            }}
            onSaveCombo={(input) => run("api-combo", () => api.saveApiGatewayCombo(input))}
            onDeleteCombo={(comboId) => run("api-combo-delete", () => api.deleteApiGatewayCombo(comboId))}
            onSetAccount={(input) => run("api-account", () => api.setApiGatewayAccount(input))}
            onRefreshModels={() =>
              run("api-models", api.refreshApiGatewayModels, "Model registry updated")
            }
            onCreateVirtual={(toolId, model) =>
              run(`api-virtual-${toolId}`, () => api.createVirtualApiAccount(toolId, model))
            }
            onRefresh={async () => {
              setSnapshot(await api.loadSnapshot());
            }}
            onCopy={async (text) => {
              const ok = await copyToClipboard(text);
              notify(ok ? `Copied: ${text}` : "Couldn't access the clipboard.", ok ? "success" : "error");
            }}
          />
        )}

        {view === "settings" && (
          <SettingsView
            snapshot={snapshot}
            busy={busy !== null}
            vaultStatus={vaultStatus}
            onSetup={(toolId) => {
              setSelectedTool(toolId);
              setDialog("setup");
            }}
            onAutoSwitchChange={(toolId, enabled, threshold) =>
              run(`autoSwitch-${toolId}`, () => api.setAutoSwitchSetting(toolId, enabled, threshold))
            }
            onLaunchWithWindowsChange={(enabled) =>
              run("launchWithWindows", () => api.setLaunchWithWindows(enabled))
            }
            onInitializeVault={initializeVault}
            onUnlockVault={unlockVault}
            onLockVault={lockVault}
          />
        )}

        {view === "accounts" && currentTool && (
          <section className="panel">
            <div className="panelHead">
              <div>
                <div className="workbenchBreadcrumb">
                  <span>Accounts</span>
                  <span aria-hidden>/</span>
                  <strong>{currentTool.name}</strong>
                </div>
                <div className="titleRow">
                  <h2>{currentTool.name}</h2>
                  <span
                    className={`status ${
                      currentTool.installed ? (attentionAccountCount > 0 ? "warn" : "ok") : "muted"
                    }`}
                  >
                    {!currentTool.installed ? "Tool not installed" : attentionAccountCount > 0 ? "Needs attention" : "Ready"}
                  </span>
                  {currentTool.id !== "antigravity" && currentAutoSwitch.enabled && (
                    <span
                      className="autoChip"
                      title={`Auto-switch the bare ${currentTool.id} command at ${currentAutoSwitch.threshold}% quota`}
                    >
                      <Zap />
                      Auto {currentAutoSwitch.threshold}%
                    </span>
                  )}
                  <span className="helpDot" title={toolDescriptions[currentTool.id]}>
                    <CircleHelp />
                  </span>
                </div>
              </div>
              <div className="actions">
                {currentTool.id === "antigravity" && (
                  <button
                    onClick={() =>
                      run(
                        "agNewLogin",
                        api.antigravityNewLogin,
                        undefined,
                        "Antigravity IDE opened at the sign-in screen. After signing into the new account, click “Save current account”.",
                      )
                    }
                    disabled={!currentTool.installed || busy !== null}
                  >
                    <LogIn />
                    Sign in new account
                  </button>
                )}
                <button
                  className="primaryAction"
                  onClick={() => setDialog("add")}
                  disabled={!currentTool.installed || busy !== null}
                >
                  <LogIn />
                  {currentTool.id === "antigravity" ? "Save current account" : "Add account"}
                </button>
                <button
                  className="secondaryAction"
                  onClick={refresh}
                  disabled={busy !== null || refreshingAccounts.size > 0}
                >
                  {refreshingAccounts.size > 0 ? (
                    <Loader2 className="spin" />
                  ) : (
                    <RefreshCw />
                  )}
                  Refresh quota
                </button>
              </div>
            </div>

            <section className="accountSummary" aria-label={`${currentTool.name} account summary`}>
              <div className={`summaryMetric ${activeAccount ? "summaryMetric--active" : ""}`}>
                <span>Active profile</span>
                <strong>{activeAccount?.name ?? "No profile"}</strong>
                <small>{activeAccount?.launcherCommand ?? `${currentTool.id} default`}</small>
              </div>
              <div className="summaryMetric">
                <span>Workspace</span>
                <strong>{accountCountLabel}</strong>
                <small>{currentTool.installed ? "CLI connected" : "Setup required"}</small>
              </div>
              <div className={`summaryMetric ${attentionAccountCount > 0 ? "summaryMetric--attention" : ""}`}>
                <span>Quota health</span>
                <strong>{attentionLabel}</strong>
                <small>{reportingAccountCount}/{visibleCurrentAccounts.length || 0} reporting</small>
              </div>
              <div className="summaryMetric summaryMetric--secondary">
                <span>Auto-switch</span>
                <strong>{currentAutoSwitch.enabled ? `${currentAutoSwitch.threshold}% threshold` : "Off"}</strong>
                <small title={`Auto-switch the bare ${currentTool.id} command when quota reaches the threshold`}>
                  Bare {currentTool.id} command
                </small>
              </div>
            </section>

            {switchNotice && (
              <div className="drift">
                <CircleHelp />
                <span>{switchNotice}</span>
                <button className="iconButton" onClick={() => setSwitchNotice(null)} title="Dismiss">
                  <X />
                </button>
              </div>
            )}

            <div className="accountGrid">
              {visibleCurrentAccounts.length === 0 ? (
                <div className="empty">
                  <KeyRound />
                  <strong>No accounts yet</strong>
                  <span>Click Add account to log in a new account (each account gets its own command).</span>
                </div>
              ) : (
                visibleCurrentAccounts.map((account) => (
                  <AccountCard
                    key={account.id}
                    account={account}
                    tool={currentTool}
                    busy={busy}
                    onSwitch={() => switchAccount(currentTool, account)}
                    onRename={() => {
                      setSelectedAccount(account);
                      setDialog("rename");
                    }}
                    onSetLauncher={() => {
                      setSelectedAccount(account);
                      setDialog("launcher");
                    }}
                    onCopy={async (text) => {
                      const ok = await copyToClipboard(text);
                      notify(ok ? `Copied: ${text}` : "Couldn't access the clipboard.", ok ? "success" : "error");
                    }}
                    onCredentials={() => {
                      setSelectedAccount(account);
                      setDialog("credentials");
                    }}
                    onDelete={() =>
                      run("delete", () => api.deleteAccount(currentTool.id, account.id))
                    }
                    onRefreshQuota={() => void refreshOneAccount(currentTool.id, account.id)}
                    refreshingQuota={refreshingAccounts.has(account.id)}
                    onReauthenticate={() =>
                      run(
                        `reauth-${account.id}`,
                        () => api.reauthenticateAccount(currentTool.id, account.id),
                        undefined,
                        "A separate sign-in window was opened. Complete the Codex sign-in; quota will refresh automatically.",
                      )
                    }
                  />
                ))
              )}
            </div>
          </section>
        )}
      </div>

      <footer className="workbenchStatusBar" aria-label="Workbench status">
        <span className="statusBarBranch"><Code2 aria-hidden /> {viewLabel}</span>
        <span><Users aria-hidden /> {currentTool ? accountCountLabel : "No accounts"}</span>
        <span className={snapshot.apiGateway.status.state === "running" ? "statusBarOnline" : ""}>
          <Server aria-hidden /> Gateway {snapshot.apiGateway.status.state}
        </span>
        <span className="statusBarSpacer" />
        <span>{currentAutoSwitch.enabled ? `Auto-switch ${currentAutoSwitch.threshold}%` : "Auto-switch off"}</span>
        <span>{version ? `v${version}` : "dev"}</span>
        <button
          type="button"
          className="statusBarQuit"
          aria-label="Quit AI Account Switcher"
          title="Quit (Ctrl+Q)"
          onClick={() => void api.quitApp().catch(() => {})}
        >
          <LogOut aria-hidden />
          <span>Quit</span>
          <kbd>Ctrl Q</kbd>
        </button>
      </footer>

      {dialog === "add" && currentTool && (
        <AddDialog
          tool={currentTool}
          onClose={() => setDialog(null)}
          onSubmit={async (input) => {
            if (await run("add", () => api.addAccount(input))) {
              setDialog(null);
            }
          }}
          onSubmitImport={async (input) => {
            if (await run("add", () => api.importCodexAccount(input))) {
              setDialog(null);
            }
          }}
          onSubmitBulkImport={async (input) => {
            if (await run("bulk-import", () => api.bulkImportCodexAccounts(input))) {
              setDialog(null);
              // Model discovery is intentionally separate from profile staging; refresh it in the
              // background so a large batch never blocks the modal or navigation.
              void api.refreshApiGatewayModels().then(setSnapshot).catch(() => {});
            }
          }}
          onSubmitBulkLogin={async (input) => {
            if (
              await run(
                "bulk-login",
                () => api.bulkLoginCodexStaging(input),
                "Staging login completed; OAuth sessions were imported into the pool",
              )
            ) {
              setDialog(null);
              setView("api");
              void api.refreshApiGatewayModels().then(setSnapshot).catch(() => {});
            }
          }}
          onSubmitApi={async (input) => {
            if (await run("add", () => api.addApiAccount(input))) {
              setDialog(null);
            }
          }}
        />
      )}

      {dialog === "rename" && currentTool && selectedAccount && (
        <NameDialog
          title="Rename account"
          label="Account name"
          initialName={selectedAccount.name}
          maxLength={20}
          submitText="Save"
          onClose={() => setDialog(null)}
          onSubmit={async (name) => {
            if (
              await run("rename", () =>
                api.renameAccount({ toolId: currentTool.id, accountId: selectedAccount.id, name }),
              )
            ) {
              setDialog(null);
            }
          }}
        />
      )}

      {dialog === "launcher" && currentTool && selectedAccount && (
        <NameDialog
          title={`Custom command (${currentTool.id}-…)`}
          label="Command name"
          hint={`Forces the ${currentTool.id}- prefix, only a-z 0-9 - _`}
          initialName={selectedAccount.launcherCommand ?? ""}
          maxLength={47}
          submitText="Save command"
          onClose={() => setDialog(null)}
          onSubmit={async (name) => {
            if (
              await run("launcher", () =>
                api.setLauncher({ toolId: currentTool.id, accountId: selectedAccount.id, name }),
              )
            ) {
              setDialog(null);
            }
          }}
        />
      )}

      {dialog === "setup" && currentTool && currentTool.id !== "antigravity" && (
        <CliSetupDialog
          tool={currentTool}
          currentSetup={snapshot.toolSetups[currentTool.id]}
          onClose={() => setDialog(null)}
          onSave={async (input) => {
            if (await run("setup", () => api.setToolSetup(input), "CLI setup saved")) {
              setDialog(null);
            }
          }}
        />
      )}

      {dialog === "credentials" && selectedAccount && (
        <CredentialsDialog
          account={selectedAccount}
          vaultStatus={vaultStatus}
          onClose={() => setDialog(null)}
          onInitializeVault={initializeVault}
          onUnlockVault={unlockVault}
          onLoad={loadVaultCredential}
          onSave={async (input) => {
            const ok = await saveVaultCredential(input);
            if (ok) setDialog(null);
            return ok;
          }}
        />
      )}
    </main>
  );
}

function SettingsView({
  snapshot,
  busy,
  vaultStatus,
  onSetup,
  onAutoSwitchChange,
  onLaunchWithWindowsChange,
  onInitializeVault,
  onUnlockVault,
  onLockVault,
}: {
  snapshot: AppSnapshot;
  busy: boolean;
  vaultStatus: VaultStatus;
  onSetup: (toolId: ToolId) => void;
  onAutoSwitchChange: (toolId: ToolId, enabled: boolean, threshold: number) => void;
  onLaunchWithWindowsChange: (enabled: boolean) => void;
  onInitializeVault: (passkey: string) => Promise<boolean>;
  onUnlockVault: (passkey: string) => Promise<boolean>;
  onLockVault: () => Promise<void>;
}) {
  const cliTools = snapshot.tools.filter((tool) => tool.id !== "antigravity");
  return (
    <section className="panel">
      <div className="panelHead">
        <div>
          <div className="titleRow">
            <h2>Settings</h2>
            <span className="status ok">CLI paths</span>
          </div>
        </div>
      </div>

      <div className="settingsList">
        <VaultSettingsCard
          status={vaultStatus}
          busy={busy}
          onInitialize={onInitializeVault}
          onUnlock={onUnlockVault}
          onLock={onLockVault}
        />
        {cliTools.map((tool) => (
          <CliSetupBar
            key={tool.id}
            tool={tool}
            setup={snapshot.toolSetups[tool.id]}
            onOpen={() => onSetup(tool.id)}
          />
        ))}

        <div className="settingsSection">
          <div className="settingsSectionHead">
            <Zap />
            <div>
              <strong>Auto-switch</strong>
              <small>Configure quota fallback separately for each CLI.</small>
            </div>
          </div>
          {cliTools.map((tool) => {
            const setting = snapshot.autoSwitchSettings[tool.id] ?? {
              enabled: snapshot.autoSwitch,
              threshold: snapshot.autoSwitchThreshold,
            };
            return (
              <AutoSwitchBar
                key={tool.id}
                tool={tool}
                enabled={setting.enabled}
                threshold={setting.threshold}
                busy={busy}
                onChange={(enabled, threshold) => onAutoSwitchChange(tool.id, enabled, threshold)}
              />
            );
          })}
        </div>

        <div className="settingsSection">
          <div className="settingsSectionHead">
            <Settings />
            <div>
              <strong>Startup</strong>
              <small>Keep the account switcher available for quota monitoring and auto-switch.</small>
            </div>
            <Toggle
              checked={snapshot.launchWithWindows}
              disabled={busy}
              onChange={onLaunchWithWindowsChange}
              title="Launch with Windows"
            />
          </div>
        </div>
      </div>
    </section>
  );
}

function VaultSettingsCard({
  status,
  busy,
  onInitialize,
  onUnlock,
  onLock,
}: {
  status: VaultStatus;
  busy: boolean;
  onInitialize: (passkey: string) => Promise<boolean>;
  onUnlock: (passkey: string) => Promise<boolean>;
  onLock: () => Promise<void>;
}) {
  const [passkey, setPasskey] = useState("");
  const [confirm, setConfirm] = useState("");
  const [message, setMessage] = useState<string | null>(null);
  const setup = !status.configured;

  const submit = async () => {
    setMessage(null);
    if (passkey.length < 8) {
      setMessage("Passkey must be at least 8 characters");
      return;
    }
    if (setup && passkey !== confirm) {
      setMessage("Passkeys do not match");
      return;
    }
    const ok = setup ? await onInitialize(passkey) : await onUnlock(passkey);
    if (ok) {
      setPasskey("");
      setConfirm("");
    }
  };

  return (
    <div className="settingsSection vaultSettings" data-vault-state={status.unlocked ? "unlocked" : "locked"}>
      <div className="settingsSectionHead">
        <span className="settingsSectionIcon" aria-hidden>
          {status.unlocked ? <KeyRound /> : <LockKeyhole />}
        </span>
        <div>
          <strong>Secure vault</strong>
          <small>
            {setup
              ? "Create an encrypted vault for account email, password and 2FA secrets."
              : status.unlocked
                ? `${status.recordCount} saved credential${status.recordCount === 1 ? "" : "s"} · vault unlocked for this session.`
                : "Credentials stay encrypted on disk and require the vault passkey to view."}
          </small>
        </div>
        {!setup && status.unlocked && (
          <button type="button" className="secondaryAction vaultLockButton" onClick={() => void onLock()} disabled={busy}>
            <LockKeyhole /> Lock
          </button>
        )}
      </div>
      {!status.unlocked && (
        <div className="vaultUnlockRow">
          <label>
            <span>{setup ? "Create passkey" : "Vault passkey"}</span>
            <input
              type="password"
              value={passkey}
              onChange={(event) => setPasskey(event.target.value)}
              placeholder={setup ? "At least 8 characters" : "Enter passkey"}
              autoComplete="new-password"
            />
          </label>
          {setup && (
            <label>
              <span>Confirm passkey</span>
              <input
                type="password"
                value={confirm}
                onChange={(event) => setConfirm(event.target.value)}
                placeholder="Repeat passkey"
                autoComplete="new-password"
              />
            </label>
          )}
          <button type="button" className="primaryAction" onClick={() => void submit()} disabled={busy}>
            <KeyRound /> {setup ? "Create vault" : "Unlock vault"}
          </button>
        </div>
      )}
      {message && <p className="quotaError">{message}</p>}
    </div>
  );
}

/** A small on/off switch (replaces checkboxes across the API tab). */
function Toggle({
  checked,
  onChange,
  disabled,
  title,
}: {
  checked: boolean;
  onChange: (next: boolean) => void;
  disabled?: boolean;
  title?: string;
}) {
  return (
    <button
      type="button"
      className={`toggle ${checked ? "on" : ""}`}
      role="switch"
      aria-checked={checked}
      onClick={() => onChange(!checked)}
      disabled={disabled}
      title={title}
    >
      <span className="toggleKnob" />
    </button>
  );
}

function ApiGatewayView({
  snapshot,
  busy,
  onStart,
  onStop,
  onCreateKey,
  onDeleteKey,
  onCopyKey,
  onSaveCombo,
  onDeleteCombo,
  onSetAccount,
  onRefreshModels,
  onCreateVirtual,
  onRefresh,
  onCopy,
}: {
  snapshot: AppSnapshot;
  busy: boolean;
  onStart: (input: StartApiGatewayInput) => Promise<boolean>;
  onStop: () => Promise<boolean>;
  onCreateKey: (input: CreateApiGatewayKeyInput) => Promise<string | null>;
  onDeleteKey: (keyId: string) => Promise<boolean>;
  onCopyKey: (keyId: string) => Promise<void>;
  onSaveCombo: (input: SaveApiGatewayComboInput) => Promise<boolean>;
  onDeleteCombo: (comboId: string) => Promise<boolean>;
  onSetAccount: (input: SetApiGatewayAccountInput) => Promise<boolean>;
  onRefreshModels: () => Promise<boolean>;
  onCreateVirtual: (toolId: ToolId, model?: string) => Promise<boolean>;
  onRefresh: () => Promise<void>;
  onCopy: (text: string) => void;
}) {
  const gateway = snapshot.apiGateway;
  const [port, setPort] = useState(String(gateway.config.port));
  const [threshold, setThreshold] = useState(String(gateway.config.quotaThreshold));
  const [allowLan, setAllowLan] = useState(gateway.config.bindHost === "0.0.0.0");
  const [rotationStrategy, setRotationStrategy] = useState(gateway.config.rotationStrategy);
  const [portError, setPortError] = useState<string | null>(null);
  const [showKeyModal, setShowKeyModal] = useState(false);
  const [comboEditing, setComboEditing] = useState<ApiGatewayCombo | null>(null);
  const [showComboModal, setShowComboModal] = useState(false);
  const [virtualTool, setVirtualTool] = useState<ToolId | null>(null);
  const [showModels, setShowModels] = useState(false);
  const [usage, setUsage] = useState<ApiUsageReport | null>(null);
  const [starting, setStarting] = useState(false);

  const running = gateway.status.state === "running";
  const hasVirtualClaude = snapshot.tools
    .find((tool) => tool.id === "claude")
    ?.accounts.some((account) => account.fingerprint === "api-local");
  const hasVirtualCodex = snapshot.tools
    .find((tool) => tool.id === "codex")
    ?.accounts.some((account) => account.fingerprint === "api-local");

  // Eligible subscription accounts (Claude/Codex, real login, not virtual), with their gateway
  // participation entry merged in for on/off + status display.
  const accounts = useMemo(
    () =>
      snapshot.tools
        .filter((tool) => tool.id === "claude" || tool.id === "codex")
        .flatMap((tool) =>
          tool.accounts
            .filter(
              (account) =>
                !account.apiProvider &&
                account.fingerprint !== "api-local" &&
                account.state !== "needsLogin",
            )
            .map((account) => {
              const entry = gateway.config.accounts.find(
                (item) => item.toolId === tool.id && item.accountId === account.id,
              );
              return { tool, account, entry };
            }),
        ),
    [snapshot.tools, gateway.config.accounts],
  );

  // Every model the gateway can serve, grouped by provider (for the picker + the collapsible
  // "Available models" list). Primary source is the live per-account model registry; if that
  // hasn't been discovered yet we fall back to a small curated list so the picker is never blank.
  const modelsByProvider = useMemo(() => {
    const groups: { tool: ToolId; label: string; models: string[] }[] = [];
    for (const tool of ["claude", "codex"] as ToolId[]) {
      // Union the live registry with the curated known list, so the picker always offers the
      // full set of usable models (like 9router) even when account discovery is sparse.
      const models = new Set<string>(FALLBACK_MODELS[tool]);
      for (const registry of gateway.config.modelRegistry.filter((r) => r.toolId === tool)) {
        registry.models.forEach((model) => models.add(model));
      }
      groups.push({
        tool,
        label: tool === "claude" ? "Claude" : "Codex",
        models: Array.from(models).sort(),
      });
    }
    return groups;
  }, [gateway.config.modelRegistry]);

  useEffect(() => {
    api.getApiUsage().then(setUsage).catch(() => setUsage(null));
  }, [gateway.status.state, gateway.config.combos.length]);

  useEffect(() => {
    if (!running) return;
    const timer = window.setInterval(() => {
      void onRefresh().catch(() => {});
      void api.getApiUsage().then(setUsage).catch(() => {});
    }, 5_000);
    return () => window.clearInterval(timer);
  }, [onRefresh, running]);

  const submitStart = async () => {
    // `Number(port) || 8783` used to swallow a typo and start the server on a port the user
    // never asked for, while the field still showed what they typed.
    const parsedPort = Number(port);
    if (!Number.isInteger(parsedPort) || parsedPort < 1 || parsedPort > 65535) {
      setPortError("Enter a port between 1 and 65535");
      return;
    }
    setPortError(null);
    setStarting(true);
    try {
      const ok = await onStart({
        bindHost: allowLan ? "0.0.0.0" : "127.0.0.1",
        port: parsedPort,
        quotaThreshold: Number(threshold) || 95,
        rotationStrategy,
      });
      // Refresh the model registry in the background — Start no longer waits on it.
      if (ok) void onRefreshModels().catch(() => {});
    } finally {
      setStarting(false);
    }
  };

  const openCreateCombo = () => {
    setComboEditing(null);
    setShowComboModal(true);
    void onRefreshModels().catch(() => {});
  };
  const openEditCombo = (combo: ApiGatewayCombo) => {
    setComboEditing(combo);
    setShowComboModal(true);
    void onRefreshModels().catch(() => {});
  };

  return (
    <section className="panel">
      <div className="panelHead">
        <div>
          <div className="titleRow">
            <h2>API</h2>
            <span
              className={`status ${
                starting ? "muted" : running ? "ok" : gateway.status.state === "errored" ? "bad" : "muted"
              }`}
            >
              {starting
                ? "Starting…"
                : running
                  ? "Running"
                  : gateway.status.state === "errored"
                    ? "Errored"
                    : "Stopped"}
            </span>
          </div>
          <p className="panelLead">Local OpenAI/Anthropic-compatible gateway for Claude and Codex subscription accounts.</p>
        </div>
        <div className="actions">
          <button onClick={() => onCopy(gateway.status.baseUrl)}>
            <Copy />
            Base URL
          </button>
          {running ? (
            <button className="danger" onClick={onStop} disabled={busy}>
              <X />
              Stop
            </button>
          ) : (
            <button className="primary" onClick={submitStart} disabled={busy || starting}>
              {starting ? <Loader2 className="spin" /> : <Server />}
              {starting ? "Starting…" : "Start"}
            </button>
          )}
        </div>
      </div>

      {gateway.status.error && <p className="quotaError">{gateway.status.error}</p>}

      <div className="apiGrid">
        <section className="apiSection">
          <div className="settingsSectionHead">
            <Server />
            <div>
              <strong>Server</strong>
              <small>{gateway.status.baseUrl}</small>
            </div>
          </div>
          <div className="apiFormRow cols3">
            <label>
              Bind
              <input value={allowLan ? "0.0.0.0" : "127.0.0.1"} readOnly />
            </label>
            <label>
              Port
              <input
                value={port}
                onChange={(event) => setPort(event.target.value)}
                disabled={running}
                title={running ? "Stop the gateway to change the port." : undefined}
              />
            </label>
            <label>
              <span>
                Quota cutoff <small>(%)</small>
              </span>
              <input
                value={threshold}
                onChange={(event) => setThreshold(event.target.value)}
                disabled={running}
                title={
                  running
                    ? "Stop the gateway to change the quota cutoff."
                    : "Drop an account from rotation once its quota usage passes this percentage."
                }
              />
            </label>
          </div>
          {portError && <p className="quotaError">{portError}</p>}
          {running && (
            <p className="hint">Server settings apply on start — stop the gateway to change them.</p>
          )}
          <div className="apiToggleRow">
            <span>
              <strong>Allow LAN access</strong>
              <small>Bind to 0.0.0.0. API keys remain required.</small>
            </span>
            <Toggle checked={allowLan} onChange={setAllowLan} title="Allow LAN access" />
          </div>
          <div className="apiInline">
            <button
              onClick={() => setVirtualTool("claude")}
              disabled={busy || !running || gateway.config.combos.length === 0}
            >
              <Terminal />
              {hasVirtualClaude ? "Update Claude Code" : "Add to Claude Code"}
            </button>
            <button
              onClick={() => setVirtualTool("codex")}
              disabled={busy || !running || gateway.config.combos.length === 0}
            >
              <Terminal />
              {hasVirtualCodex ? "Update Codex" : "Add to Codex"}
            </button>
          </div>
        </section>

        <section className="apiSection">
          <div className="settingsSectionHead">
            <KeyRound />
            <div>
              <strong>API keys</strong>
              <small>Any valid key can call every combo.</small>
            </div>
            <button onClick={() => setShowKeyModal(true)} disabled={busy}>
              <Plus />
              Add key
            </button>
          </div>
          <div className="apiList">
            {gateway.config.keys.length === 0 ? (
              <p className="hint">Create a key before calling the gateway.</p>
            ) : (
              gateway.config.keys.map((key) => (
                <div className="apiListItem" key={key.id}>
                  <div>
                    <strong>{key.name || "Unnamed key"}</strong>
                    <small>
                      {key.prefix}
                      {key.expiresAt ? ` · expires ${new Date(key.expiresAt).toLocaleDateString()}` : ""}
                    </small>
                  </div>
                  <div className="apiItemActions">
                    <button className="iconButton" onClick={() => onCopyKey(key.id)} disabled={busy} title="Copy full key">
                      <Copy />
                    </button>
                    <button className="iconButton danger" onClick={() => onDeleteKey(key.id)} disabled={busy} title="Delete key">
                      <Trash2 />
                    </button>
                  </div>
                </div>
              ))
            )}
          </div>
        </section>

        <section className="apiSection wide">
          <div className="settingsSectionHead">
            <Users />
            <div>
              <strong>Accounts</strong>
              <small>Toggle which subscription accounts the gateway may rotate through.</small>
            </div>
            <label
              className="apiRotation"
              title={
                running
                  ? "Stop the gateway to change the rotation strategy."
                  : "On = rotate accounts round-robin. Off = fill one until exhausted."
              }
            >
              <span>Round-robin</span>
              <Toggle
                checked={rotationStrategy === "roundRobin"}
                disabled={running}
                onChange={(next) => setRotationStrategy(next ? "roundRobin" : "fillFirst")}
              />
            </label>
            <button
              className="iconButton"
              onClick={onRefreshModels}
              disabled={busy}
              title="Refresh subscription models"
            >
              <RefreshCw />
            </button>
          </div>
          {accounts.length === 0 ? (
            <p className="hint">No Claude or Codex subscription accounts yet.</p>
          ) : (
            <div className="apiList">
              {accounts.map(({ tool, account, entry }) => {
                const enabled = entry?.enabled ?? true;
                const state = entry?.state ?? "available";
                return (
                  <div className="apiListItem" key={`${tool.id}-${account.id}`}>
                    <div>
                      <strong>{account.name}</strong>
                      <small>
                        {tool.name}
                        {!enabled && " · Off"}
                        {enabled && state !== "available" && (
                          <>
                            {" · "}
                            <span className={`poolMemberState ${state}`}>{poolStateLabel(state)}</span>
                          </>
                        )}
                      </small>
                    </div>
                    <button
                      className={`toggle ${enabled ? "on" : ""}`}
                      role="switch"
                      aria-checked={enabled}
                      onClick={() =>
                        onSetAccount({ toolId: tool.id, accountId: account.id, enabled: !enabled })
                      }
                      disabled={busy}
                      title={enabled ? "Disable for gateway" : "Enable for gateway"}
                    >
                      <span className="toggleKnob" />
                    </button>
                  </div>
                );
              })}
            </div>
          )}
        </section>

        <section className="apiSection wide">
          <div className="settingsSectionHead">
            <Layers />
            <div>
              <strong>Combos</strong>
              <small>A combo name is the model id clients request; members are tried in order.</small>
            </div>
            <button onClick={openCreateCombo} disabled={busy}>
              <Plus />
              Add Combo
            </button>
          </div>
          <div className="apiList">
            {gateway.config.combos.length === 0 ? (
              <p className="hint">Create a combo, then point your client at its name.</p>
            ) : (
              gateway.config.combos.map((combo: ApiGatewayCombo) => (
                <div className="apiListItem comboItem" key={combo.id}>
                  <div>
                    <strong>{combo.name}</strong>
                    <small>
                      {(combo.strategy ?? rotationStrategy) === "fillFirst"
                        ? "Fill-first"
                        : "Round-robin"}
                      {" · "}
                      {combo.members.length} model(s)
                    </small>
                    <div className="apiModelChips comboMembers">
                      {combo.members.map((model, index) => (
                        <span className="apiModelChip" key={model}>
                          <span className="comboMemberOrder">{index + 1}</span>
                          {model}
                        </span>
                      ))}
                    </div>
                  </div>
                  <div className="apiItemActions">
                    <button className="iconButton" onClick={() => onCopy(combo.name)} title="Copy combo name">
                      <Copy />
                    </button>
                    <button className="iconButton" onClick={() => openEditCombo(combo)} disabled={busy} title="Edit combo">
                      <Pencil />
                    </button>
                    <button className="iconButton danger" onClick={() => onDeleteCombo(combo.id)} disabled={busy} title="Delete combo">
                      <Trash2 />
                    </button>
                  </div>
                </div>
              ))
            )}
          </div>
        </section>

        <section className="apiSection wide">
          <button className="apiCollapseHead" onClick={() => setShowModels((value) => !value)}>
            <div className="settingsSectionHead">
              <Boxes />
              <div>
                <strong>Available models</strong>
                <small>
                  {modelsByProvider.reduce((sum, group) => sum + group.models.length, 0)} model(s)
                  from your enabled accounts
                </small>
              </div>
            </div>
            <ChevronDown className={showModels ? "rotated" : ""} />
          </button>
          {showModels &&
            (modelsByProvider.length === 0 ? (
              <p className="hint">No models yet — refresh while accounts are signed in.</p>
            ) : (
              modelsByProvider.map((group) => (
                <div className="apiModelGroup" key={group.tool}>
                  <small className="apiModelGroupLabel">
                    {group.label} ({group.models.length})
                  </small>
                  <div className="apiModelChips">
                    {group.models.map((model) => (
                      <span className="apiModelChip" key={model}>
                        {model}
                      </span>
                    ))}
                  </div>
                </div>
              ))
            ))}
        </section>

        <section className="apiSection wide">
          <div className="settingsSectionHead">
            <BarChart3 />
            <div>
              <strong>API usage</strong>
              <small>{usage ? `${usage.totalRequests} request(s)` : "No proxy usage yet"}</small>
            </div>
          </div>
          <div className="apiUsageStats">
            <span>Input {formatCount(usage?.total.input ?? 0)}</span>
            <span>Output {formatCount(usage?.total.output ?? 0)}</span>
            <span>Cache {formatCount((usage?.total.cacheRead ?? 0) + (usage?.total.cacheCreation ?? 0))}</span>
          </div>
          <div className="apiList">
            {usage?.rows.map((row) => (
              <div className="apiListItem" key={`${row.comboName}-${row.keyId}-${row.accountId}`}>
                <div>
                  <strong>{row.comboName}</strong>
                  <small>
                    {row.toolId} · {row.requests} request(s) · {formatCount(row.tokens.input + row.tokens.output)} tokens
                  </small>
                </div>
              </div>
            ))}
          </div>
        </section>
      </div>

      {showKeyModal && (
        <AddKeyModal
          busy={busy}
          onClose={() => setShowKeyModal(false)}
          onCreate={onCreateKey}
          onCopy={onCopy}
        />
      )}

      {showComboModal && (
        <ComboModal
          busy={busy}
          combo={comboEditing}
          defaultStrategy={rotationStrategy}
          modelsByProvider={modelsByProvider}
          existingCombos={gateway.config.combos}
          onClose={() => setShowComboModal(false)}
          onSave={async (input) => {
            const ok = await onSaveCombo(input);
            if (ok) setShowComboModal(false);
          }}
        />
      )}

      {virtualTool && (
        <VirtualAccountModal
          busy={busy}
          tool={virtualTool}
          combos={gateway.config.combos.filter((combo) => combo.enabled)}
          modelsByProvider={modelsByProvider}
          currentModel={
            snapshot.tools
              .find((t) => t.id === virtualTool)
              ?.accounts.find((a) => a.fingerprint === "api-local")?.apiProvider?.model ?? null
          }
          onClose={() => setVirtualTool(null)}
          onConfirm={async (model) => {
            const ok = await onCreateVirtual(virtualTool, model);
            if (ok) setVirtualTool(null);
          }}
        />
      )}
    </section>
  );
}

function VirtualAccountModal({
  busy,
  tool,
  combos,
  modelsByProvider,
  currentModel,
  onClose,
  onConfirm,
}: {
  busy: boolean;
  tool: ToolId;
  combos: ApiGatewayCombo[];
  modelsByProvider: { tool: ToolId; label: string; models: string[] }[];
  currentModel: string | null;
  onClose: () => void;
  onConfirm: (model: string) => Promise<void>;
}) {
  const toolName = tool === "claude" ? "Claude Code" : "Codex";
  const [model, setModel] = useState(currentModel ?? combos[0]?.name ?? "");
  return (
    <div className="modalBackdrop" role="presentation" onMouseDown={onClose}>
      <section className="modal" onMouseDown={(event) => event.stopPropagation()}>
        <h2>{currentModel ? `Update ${toolName}` : `Add to ${toolName}`}</h2>
        <p className="hint">
          A <code>{tool === "claude" ? "claude-api" : "codex-api"}</code> account is added to{" "}
          {toolName} pointing at the local gateway. Choose the model it requests — a combo (with
          fallback) or any single model the gateway can serve.
        </p>
        <label>
          Model
          <input
            value={model}
            onChange={(event) => setModel(event.target.value)}
            placeholder="combo name or model id"
          />
        </label>
        <div className="comboPicker">
          {combos.length > 0 && (
            <div className="apiModelGroup">
              <small className="apiModelGroupLabel">Combos</small>
              <div className="apiModelChips">
                {combos.map((combo) => (
                  <button
                    key={combo.id}
                    className={`apiModelChip pickable ${model === combo.name ? "picked" : ""}`}
                    onClick={() => setModel(combo.name)}
                  >
                    {combo.name}
                  </button>
                ))}
              </div>
            </div>
          )}
          {modelsByProvider.map((group) => (
            <div className="apiModelGroup" key={group.tool}>
              <small className="apiModelGroupLabel">
                {group.label} ({group.models.length})
              </small>
              <div className="apiModelChips">
                {group.models.map((name) => (
                  <button
                    key={name}
                    className={`apiModelChip pickable ${model === name ? "picked" : ""}`}
                    onClick={() => setModel(name)}
                  >
                    {name}
                  </button>
                ))}
              </div>
            </div>
          ))}
        </div>
        <div className="modalActions">
          <button onClick={onClose}>Cancel</button>
          <button
            className="primary"
            onClick={() => model.trim() && onConfirm(model.trim())}
            disabled={busy || !model.trim()}
          >
            <Terminal />
            {currentModel ? "Update" : "Add"}
          </button>
        </div>
      </section>
    </div>
  );
}

function ComboModal({
  busy,
  combo,
  defaultStrategy,
  modelsByProvider,
  existingCombos,
  onClose,
  onSave,
}: {
  busy: boolean;
  combo: ApiGatewayCombo | null;
  defaultStrategy: ApiRotationStrategy;
  modelsByProvider: { tool: ToolId; label: string; models: string[] }[];
  existingCombos: ApiGatewayCombo[];
  onClose: () => void;
  onSave: (input: SaveApiGatewayComboInput) => Promise<void>;
}) {
  const [name, setName] = useState(combo?.name ?? "");
  const [members, setMembers] = useState<string[]>(combo?.members ?? []);
  const [roundRobin, setRoundRobin] = useState(
    (combo?.strategy ?? defaultStrategy) === "roundRobin",
  );
  const [picking, setPicking] = useState(false);

  const move = (index: number, delta: number) => {
    const next = [...members];
    const target = index + delta;
    if (target < 0 || target >= next.length) return;
    [next[index], next[target]] = [next[target], next[index]];
    setMembers(next);
  };
  const toggleModel = (model: string) => {
    setMembers((current) =>
      current.includes(model) ? current.filter((item) => item !== model) : [...current, model],
    );
  };
  const submit = () =>
    onSave({
      id: combo?.id ?? null,
      name: name.trim(),
      members,
      strategy: roundRobin ? "roundRobin" : "fillFirst",
    });

  return (
    <div className="modalBackdrop" role="presentation" onMouseDown={onClose}>
      <section className="modal" onMouseDown={(event) => event.stopPropagation()}>
        <h2>{combo ? "Edit Combo" : "Create Combo"}</h2>
        <label>
          Combo name
          <input
            autoFocus
            value={name}
            onChange={(event) => setName(event.target.value)}
            placeholder="e.g. claude-sonnet-4-6"
          />
          <small>Only letters, numbers, -, _ and . allowed. Clients request this name.</small>
        </label>

        <div className="apiToggleRow">
          <span>
            Round-robin
            <small>Rotate members each request. Off = fallback (try members top to bottom).</small>
          </span>
          <Toggle checked={roundRobin} onChange={setRoundRobin} title="Round-robin" />
        </div>

        <div className="comboModels">
          <span className="labelRow">Models</span>
          {members.length === 0 ? (
            <p className="comboEmpty">
              <Layers />
              No models added yet
            </p>
          ) : (
            members.map((model, index) => (
              <div className="comboMemberRow" key={model}>
                <span className="comboMemberIndex">{index + 1}</span>
                <span className="comboMemberName">{model}</span>
                <button
                  className="iconButton"
                  onClick={() => move(index, -1)}
                  disabled={index === 0}
                  title="Move up"
                >
                  <ArrowUp />
                </button>
                <button
                  className="iconButton"
                  onClick={() => move(index, 1)}
                  disabled={index === members.length - 1}
                  title="Move down"
                >
                  <ArrowDown />
                </button>
                <button className="iconButton danger" onClick={() => toggleModel(model)} title="Remove">
                  <X />
                </button>
              </div>
            ))
          )}
          <button className="comboAddModel" onClick={() => setPicking((value) => !value)}>
            <Plus />
            Add Model
          </button>
          {picking && (
            <div className="comboPicker">
              {existingCombos.filter((item) => item.id !== combo?.id).length > 0 && (
                <div className="apiModelGroup">
                  <small className="apiModelGroupLabel">Combos</small>
                  <div className="apiModelChips">
                    {existingCombos
                      .filter((item) => item.id !== combo?.id)
                      .map((item) => (
                        <button
                          key={item.id}
                          className={`apiModelChip pickable ${members.includes(item.name) ? "picked" : ""}`}
                          onClick={() => toggleModel(item.name)}
                        >
                          {item.name}
                        </button>
                      ))}
                  </div>
                </div>
              )}
              {modelsByProvider.map((group) => (
                <div className="apiModelGroup" key={group.tool}>
                  <small className="apiModelGroupLabel">
                    {group.label} ({group.models.length})
                  </small>
                  <div className="apiModelChips">
                    {group.models.map((model) => (
                      <button
                        key={model}
                        className={`apiModelChip pickable ${members.includes(model) ? "picked" : ""}`}
                        onClick={() => toggleModel(model)}
                      >
                        {model}
                      </button>
                    ))}
                  </div>
                </div>
              ))}
            </div>
          )}
        </div>

        <div className="modalActions">
          <button onClick={onClose}>Cancel</button>
          <button
            className="primary"
            onClick={submit}
            disabled={busy || !name.trim() || members.length === 0}
          >
            {combo ? "Save" : "Create"}
          </button>
        </div>
      </section>
    </div>
  );
}

function AddKeyModal({
  busy,
  onClose,
  onCreate,
  onCopy,
}: {
  busy: boolean;
  onClose: () => void;
  onCreate: (input: CreateApiGatewayKeyInput) => Promise<string | null>;
  onCopy: (text: string) => void;
}) {
  const [name, setName] = useState("");
  const [expiry, setExpiry] = useState("");
  const [secret, setSecret] = useState<string | null>(null);
  const submit = async () => {
    const created = await onCreate({
      name: name.trim() || "Untitled key",
      expiresAt: expiry ? new Date(`${expiry}T23:59:59`).toISOString() : null,
    });
    if (created) setSecret(created);
  };
  return (
    <div className="modalBackdrop" role="presentation" onMouseDown={onClose}>
      <section className="modal" onMouseDown={(event) => event.stopPropagation()}>
        <h2>Add API key</h2>
        {secret ? (
          // Reveal view: the secret is shown exactly once. Copy works even if auto-copy was denied.
          <>
            <label>
              Your new API key
              <div className="secretReveal">
                <code>{secret}</code>
                <button className="iconButton" onClick={() => onCopy(secret)} title="Copy key">
                  <Copy />
                </button>
              </div>
              <small>Copy it now — it will not be shown again.</small>
            </label>
            <div className="modalActions">
              <button className="primary" onClick={onClose}>
                Done
              </button>
            </div>
          </>
        ) : (
          <>
            <label>
              Key name
              <input
                autoFocus
                value={name}
                onChange={(event) => setName(event.target.value)}
                placeholder="e.g. Cline laptop"
              />
            </label>
            <label>
              <span className="labelRow">
                Expires <small>(optional)</small>
              </span>
              <input type="date" value={expiry} onChange={(event) => setExpiry(event.target.value)} />
            </label>
            <p className="hint">The full key is shown once when created.</p>
            <div className="modalActions">
              <button onClick={onClose}>Cancel</button>
              <button className="primary" onClick={submit} disabled={busy}>
                <Plus />
                Create key
              </button>
            </div>
          </>
        )}
      </section>
    </div>
  );
}

let toastSeq = 0;
function nextToastId() {
  toastSeq += 1;
  return toastSeq;
}

/// Copy text to the clipboard. Prefers Tauri's clipboard plugin (writes via the OS, so it works
/// even though the webview denies the JS Clipboard API), then falls back to navigator/execCommand
/// for the browser demo. Never throws — returns whether it worked.
async function copyToClipboard(text: string): Promise<boolean> {
  try {
    const { writeText } = await import("@tauri-apps/plugin-clipboard-manager");
    await writeText(text);
    return true;
  } catch {
    // Not in Tauri (browser demo) or plugin unavailable — try the web APIs.
  }
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // fall through to the legacy path
  }
  try {
    const area = document.createElement("textarea");
    area.value = text;
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.appendChild(area);
    area.focus();
    area.select();
    const ok = document.execCommand("copy");
    document.body.removeChild(area);
    return ok;
  } catch {
    return false;
  }
}

// The live per-account model registry is authoritative — it lists exactly the models each
// subscription account can actually serve (e.g. a ChatGPT/Codex account can't use `gpt-5`, only
// `gpt-5.4`/`gpt-5.5`). We deliberately keep no hardcoded fallback so the picker never offers a
// model the account would reject; the registry refreshes on Start and when the combo modal opens.
const FALLBACK_MODELS: Record<ToolId, string[]> = {
  claude: [],
  codex: [],
  antigravity: [],
};

function poolStateLabel(state: ApiPoolAccountState) {
  if (state === "coolingDown") return "Cooling down";
  if (state === "exhausted") return "Exhausted";
  if (state === "errored") return "Needs attention";
  if (state === "excluded") return "Excluded";
  return "Available";
}

function formatCount(value: number) {
  return new Intl.NumberFormat("en", { notation: value >= 100_000 ? "compact" : "standard" }).format(value);
}

function CliSetupBar({
  tool,
  setup,
  onOpen,
}: {
  tool: ToolStatus;
  setup: ToolSetup | undefined;
  onOpen: () => void;
}) {
  const ready = !!setup?.binaryPath && !!setup?.defaultConfigDir;
  const fullPaths = ready
    ? `${normalizePathForDisplay(setup!.binaryPath!)} · ${normalizePathForDisplay(setup!.defaultConfigDir!)}`
    : undefined;
  return (
    <div className={`cliSetup ${ready ? "ready" : "needsSetup"}`}>
      <Terminal />
      <div className="cliSetupText">
        <strong>{ready ? "CLI setup resolved" : "CLI setup needs review"}</strong>
        <small title={fullPaths}>
          {ready
            ? `${formatPathForDisplay(setup!.binaryPath!)} · ${formatPathForDisplay(setup!.defaultConfigDir!)}`
            : `Set where ${tool.name} is installed and where its main config lives.`}
        </small>
      </div>
      <button type="button" onClick={onOpen}>
        {ready ? "Review" : "Setup"}
      </button>
    </div>
  );
}

function AccountCard({
  account,
  tool,
  busy,
  onSwitch,
  onRename,
  onSetLauncher,
  onCredentials,
  onCopy,
  onDelete,
  onRefreshQuota,
  refreshingQuota,
  onReauthenticate,
}: {
  account: Account;
  tool: ToolStatus;
  busy: string | null;
  onSwitch: () => void;
  onRename: () => void;
  onSetLauncher: () => void;
  onCredentials: () => void;
  onCopy: (text: string) => void;
  onDelete: () => void;
  onRefreshQuota: () => void;
  refreshingQuota: boolean;
  onReauthenticate: () => void;
}) {
  const isAntigravity = tool.id === "antigravity";
  const isApi = !!account.apiProvider;
  const isVirtualApi = account.fingerprint === "api-local";
  const isActive = account.id === tool.activeAccountId;
  const exhausted = account.state === "exhausted";
  const needsLogin = account.state === "needsLogin";
  const canReauthenticate = needsLogin && tool.id === "codex" && !account.isDefault && !isApi;

  return (
    <article className={`account ${isActive ? "active" : ""} ${exhausted ? "exhausted" : ""}`}>
      <div className="accountTop">
        <div className="accountIdentity">
          {isAntigravity &&
            (account.avatarUrl ? (
              <img className="avatar" src={account.avatarUrl} alt="" referrerPolicy="no-referrer" />
            ) : (
              <span className="avatar avatarFallback">
                {account.name.slice(0, 1).toUpperCase()}
              </span>
            ))}
          <div className="identityText">
            <div className="accountName">
              {isActive && <span className="dot" aria-hidden />}
              <h3 title={account.name}>{account.name}</h3>
              {account.quota?.plan && <span className="plan">{account.quota.plan}</span>}
              {account.launcherCommand && (
                <button
                  className="cmdChip"
                  onClick={() => onCopy(account.launcherCommand!)}
                  title={`Copy command: ${account.launcherCommand}`}
                >
                  <code>{account.launcherCommand}</code>
                  <Copy />
                </button>
              )}
            </div>
            {account.accountEmail ? (
              <span className="accountEmail" title={account.accountEmail}>
                {account.accountEmail}
              </span>
            ) : isApi ? (
              <span className="fingerprint" title={account.apiProvider!.baseUrl}>
                via {gatewayHost(account.apiProvider!.baseUrl)}
              </span>
            ) : (
              account.isDefault && <span className="fingerprint">Machine default</span>
            )}
          </div>
        </div>
        <div className="badgeRow">
          {isApi && <span className="badge api">{isVirtualApi ? "Local API" : "API"}</span>}
          <span className={`badge ${badgeClass(account.state)}`}>
            {exhausted ? "Out of quota" : isActive ? "In use" : stateLabel(account.state)}
          </span>
        </div>
      </div>

      {isApi ? (
        <p className="apiMeta">
          Model <code>{account.apiProvider!.model}</code>
        </p>
      ) : (
        <Quota quota={account.quota} />
      )}

      {needsLogin && (
        <div className="pendingLogin">
          {canReauthenticate ? <ShieldAlert /> : <Loader2 className="spin" />}
          <span>
            {isAntigravity
              ? "Waiting for sign-in in the Antigravity IDE window - the app will detect it."
              : tool.id === "codex"
                ? canReauthenticate
                  ? "Codex sign-in expired. Start re-login to refresh this account's quota."
                  : "Waiting for browser approval - the code appears in this app."
                : "Waiting for login in Terminal - the app will detect it."}
          </span>
          {canReauthenticate && (
            <button type="button" onClick={onReauthenticate} disabled={busy !== null}>
              <LogIn /> Re-login
            </button>
          )}
        </div>
      )}

      <div className="cardActions">
        <button onClick={onSwitch} disabled={isActive || needsLogin || busy !== null}>
          <RotateCcw /> Use
        </button>
        {!isVirtualApi && !isApi && (
          <button
            className="iconButton vaultAction"
            onClick={onCredentials}
            disabled={busy !== null}
            title="Secure credentials"
          >
            <KeyRound />
          </button>
        )}
        {!isApi && !needsLogin && (
          <button
            className="iconButton"
            onClick={onRefreshQuota}
            disabled={refreshingQuota || busy !== null}
            title="Refresh quota"
          >
            {refreshingQuota ? <Loader2 className="spin" /> : <RefreshCw />}
          </button>
        )}
        {tool.id !== "antigravity" && !account.isDefault && !isVirtualApi && (
          <button
            className="iconButton"
            onClick={onSetLauncher}
            disabled={needsLogin || busy !== null}
            title="Custom command"
          >
            <Terminal />
          </button>
        )}
        {!account.isDefault && !isVirtualApi && (
          <button
            className="iconButton"
            onClick={onRename}
            disabled={needsLogin || busy !== null}
            title="Rename"
          >
            <Pencil />
          </button>
        )}
        {!account.isDefault && !isVirtualApi && (
          <button className="iconButton danger" onClick={onDelete} disabled={busy !== null} title="Delete">
            <Trash2 />
          </button>
        )}
      </div>
    </article>
  );
}

function CredentialsDialog({
  account,
  vaultStatus,
  onClose,
  onInitializeVault,
  onUnlockVault,
  onLoad,
  onSave,
}: {
  account: Account;
  vaultStatus: VaultStatus;
  onClose: () => void;
  onInitializeVault: (passkey: string) => Promise<boolean>;
  onUnlockVault: (passkey: string) => Promise<boolean>;
  onLoad: (account: Account) => Promise<VaultCredential | null>;
  onSave: (input: VaultCredentialInput) => Promise<boolean>;
}) {
  const [passkey, setPasskey] = useState("");
  const [confirm, setConfirm] = useState("");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [totpSecret, setTotpSecret] = useState("");
  const [source, setSource] = useState("");
  const [notes, setNotes] = useState("");
  const [showPassword, setShowPassword] = useState(false);
  const [showTotp, setShowTotp] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const needsSetup = !vaultStatus.configured;

  useEffect(() => {
    if (vaultStatus.unlocked) return;
    setPasskey("");
    setConfirm("");
    setPassword("");
    setTotpSecret("");
    setShowPassword(false);
    setShowTotp(false);
  }, [vaultStatus.unlocked]);

  useEffect(() => {
    if (!vaultStatus.unlocked) return;
    let cancelled = false;
    setLoading(true);
    void onLoad(account)
      .then((saved) => {
        if (cancelled || !saved) return;
        setUsername(saved.username);
        setPassword(saved.password);
        setTotpSecret(saved.totpSecret);
        setSource(saved.source);
        setNotes(saved.notes);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [account.id, account.toolId, vaultStatus.unlocked]);

  const unlock = async () => {
    setMessage(null);
    if (passkey.length < 8) {
      setMessage("Passkey must be at least 8 characters");
      return;
    }
    if (needsSetup && passkey !== confirm) {
      setMessage("Passkeys do not match");
      return;
    }
    const ok = needsSetup ? await onInitializeVault(passkey) : await onUnlockVault(passkey);
    if (ok) {
      setPasskey("");
      setConfirm("");
      setMessage(null);
    }
  };

  const submit = async () => {
    setMessage(null);
    if (!username.trim()) {
      setMessage("Username or email is required");
      return;
    }
    if (!password) {
      setMessage("Password is required");
      return;
    }
    await onSave({
      toolId: account.toolId,
      accountId: account.id,
      username: username.trim(),
      password,
      totpSecret: totpSecret.trim(),
      source: source.trim(),
      notes: notes.trim(),
    });
  };

  return (
    <div className="modalBackdrop" role="presentation" onMouseDown={onClose}>
      <section className="modal credentialsModal" onMouseDown={(event) => event.stopPropagation()}>
        <div className="credentialsModalHead">
          <div>
            <span className="eyebrow">Secure vault</span>
            <h2>{account.name}</h2>
            <p>Store details manually for this account. OAuth/device-code login is never scraped.</p>
          </div>
          <LockKeyhole aria-hidden />
        </div>

        {!vaultStatus.unlocked ? (
          <div className="vaultGate">
            <strong>{needsSetup ? "Create the vault passkey" : "Unlock the vault"}</strong>
            <small>
              {needsSetup
                ? "Use a passkey you can remember. Without it, the encrypted file cannot be opened."
                : "The account details remain encrypted until this passkey is verified."}
            </small>
            <label>
              {needsSetup ? "Create passkey" : "Passkey"}
              <input
                autoFocus
                type="password"
                value={passkey}
                onChange={(event) => setPasskey(event.target.value)}
                autoComplete={needsSetup ? "new-password" : "current-password"}
              />
            </label>
            {needsSetup && (
              <label>
                Confirm passkey
                <input
                  type="password"
                  value={confirm}
                  onChange={(event) => setConfirm(event.target.value)}
                  autoComplete="new-password"
                />
              </label>
            )}
            {message && <p className="quotaError">{message}</p>}
            <div className="modalActions">
              <button type="button" onClick={onClose}>Cancel</button>
              <button type="button" className="primary" onClick={() => void unlock()}>
                <KeyRound /> {needsSetup ? "Create & continue" : "Unlock"}
              </button>
            </div>
          </div>
        ) : (
          <>
            <div className="credentialFields">
              <label>
                Username or email
                <input autoFocus value={username} onChange={(event) => setUsername(event.target.value)} />
              </label>
              <label>
                Password
                <span className="sensitiveInput">
                  <input
                    type={showPassword ? "text" : "password"}
                    value={password}
                    onChange={(event) => setPassword(event.target.value)}
                    autoComplete="off"
                  />
                  <button type="button" className="iconButton" onClick={() => setShowPassword((value) => !value)} title={showPassword ? "Hide password" : "Reveal password"}>
                    {showPassword ? <EyeOff /> : <Eye />}
                  </button>
                </span>
              </label>
              <label>
                2FA secret <small>(optional TOTP seed)</small>
                <span className="sensitiveInput">
                  <input
                    type={showTotp ? "text" : "password"}
                    value={totpSecret}
                    onChange={(event) => setTotpSecret(event.target.value)}
                    autoComplete="off"
                  />
                  <button type="button" className="iconButton" onClick={() => setShowTotp((value) => !value)} title={showTotp ? "Hide 2FA secret" : "Reveal 2FA secret"}>
                    {showTotp ? <EyeOff /> : <Eye />}
                  </button>
                </span>
              </label>
              <label>
                Where it came from <small>(optional)</small>
                <input value={source} onChange={(event) => setSource(event.target.value)} placeholder="Shop, seller or platform" />
              </label>
              <label>
                Notes <small>(optional)</small>
                <textarea value={notes} onChange={(event) => setNotes(event.target.value)} rows={3} placeholder="Plan, renewal date, or other reminder" />
              </label>
            </div>
            {message && <p className="quotaError">{message}</p>}
            <div className="modalActions">
              <button type="button" onClick={onClose}>Cancel</button>
              <button type="button" className="primary" onClick={() => void submit()} disabled={loading}>
                {loading ? <Loader2 className="spin" /> : <KeyRound />} Save securely
              </button>
            </div>
          </>
        )}
      </section>
    </div>
  );
}

function AutoSwitchBar({
  tool,
  enabled,
  threshold,
  busy,
  onChange,
}: {
  tool: ToolStatus;
  enabled: boolean;
  threshold: number;
  busy: boolean;
  onChange: (enabled: boolean, threshold: number) => void;
}) {
  return (
    <div className={`autoSwitch ${enabled ? "on" : ""}`}>
      <div className="autoSwitchMain">
        <span className="autoSwitchIcon" aria-hidden>
          <Zap />
        </span>
        <div className="autoSwitchText">
          <strong>{tool.name}</strong>
          <small>
            Switch the bare <code>{tool.id}</code> command to another account when quota is nearly out.
          </small>
        </div>
        <button
          type="button"
          role="switch"
          aria-checked={enabled}
          className={`switchTrack ${enabled ? "on" : ""}`}
          disabled={busy}
          onClick={() => onChange(!enabled, threshold)}
        >
          <span className="switchThumb" />
        </button>
      </div>
      {enabled && (
        <div className="autoSwitchOpts">
          <span>Switch after using</span>
          {[90, 95, 100].map((value) => (
            <button
              key={value}
              type="button"
              className={`pill ${threshold === value ? "active" : ""}`}
              disabled={busy}
              onClick={() => onChange(enabled, value)}
            >
              {value === 100 ? "100% (fully out)" : `${value}%`}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

function Quota({ quota }: { quota: Account["quota"] }) {
  if (!quota) return <p className="quotaError">No quota data yet</p>;
  const hasKnownQuota =
    quota.fiveHour.percentUsed !== null ||
    quota.weekly.percentUsed !== null ||
    quota.models?.some((model) => model.percentUsed !== null);
  if (quota.error && !hasKnownQuota) return <p className="quotaError">{quota.error}</p>;

  // Antigravity: quota is reported per model instead of as a single overall window.
  if (quota.models && quota.models.length > 0) {
    return (
      <div className="quotaBox">
        {quota.error && <p className="quotaWarning">{quota.error}</p>}
        <span className="quotaTitle">Quota by model</span>
        {quota.models.map((model) => (
          <QuotaBar key={model.label} label={model.label} percent={model.percentUsed} resetAt={model.resetAt} />
        ))}
      </div>
    );
  }

  return (
    <div className="quotaBox">
      {quota.error && <p className="quotaWarning">{quota.error}</p>}
      <QuotaBar label="5-hour limit" percent={quota.fiveHour.percentUsed} resetAt={quota.fiveHour.resetAt} />
      <QuotaBar label="Weekly limit" percent={quota.weekly.percentUsed} resetAt={quota.weekly.resetAt} />
    </div>
  );
}

function QuotaBar({ label, percent, resetAt }: { label: string; percent: number | null; resetAt: string | null }) {
  const value = Math.max(0, Math.min(100, percent ?? 0));
  const level = percent === null ? "unknown" : value >= 90 ? "high" : value >= 70 ? "mid" : "low";
  return (
    <div className="quotaLine">
      <div className="quotaMeta">
        <span className="quotaLabel">{label}</span>
        <strong>{percent === null ? "?" : `${Math.round(value)}%`}</strong>
        {resetAt && (
          <span className="quotaReset" title={`Resets at ${formatTime(resetAt)}`}>
            {formatReset(resetAt)}
          </span>
        )}
      </div>
      <div className="bar" data-level={level}>
        <span style={{ width: `${value}%` }} />
      </div>
    </div>
  );
}

function AddDialog({
  tool,
  onClose,
  onSubmit,
  onSubmitImport,
  onSubmitBulkImport,
  onSubmitBulkLogin,
  onSubmitApi,
}: {
  tool: ToolStatus;
  onClose: () => void;
  onSubmit: (input: AddAccountInput) => Promise<void>;
  onSubmitImport: (input: ImportCodexAccountInput) => Promise<void>;
  onSubmitBulkImport: (input: BulkImportCodexAccountsInput) => Promise<void>;
  onSubmitBulkLogin: (input: BulkLoginCodexInput) => Promise<void>;
  onSubmitApi: (input: AddApiAccountInput) => Promise<void>;
}) {
  const isCli = tool.id !== "antigravity";
  // API/proxy accounts are supported for the CLI tools (Codex + Claude Code).
  const canApi = tool.id === "codex" || tool.id === "claude";
  const canImport = tool.id === "codex";
  const bypassFlag =
    tool.id === "claude"
      ? "--dangerously-skip-permissions"
      : "--dangerously-bypass-approvals-and-sandbox";
  const [kind, setKind] = useState<"login" | "import" | "bulk" | "auto" | "api">("login");
  const isImport = canImport && kind === "import";
  const isBulkImport = canImport && kind === "bulk";
  const isBulkLogin = canImport && kind === "auto";
  const isApi = canApi && kind === "api";
  const [codexLoginMethod, setCodexLoginMethod] =
    useState<CodexLoginMethod>("deviceCode");

  const [name, setName] = useState("");
  const [launcher, setLauncher] = useState("");
  const [sessionFile, setSessionFile] = useState<File | null>(null);
  const [bulkSessionFile, setBulkSessionFile] = useState<File | null>(null);
  const [bulkNamePrefix, setBulkNamePrefix] = useState("acc");
  const [bulkLauncherPrefix, setBulkLauncherPrefix] = useState("bulk-");
  const [bulkLoginFile, setBulkLoginFile] = useState<File | null>(null);
  const [bulkLoginWorkerPath, setBulkLoginWorkerPath] = useState("");
  const [bulkLoginWorkers, setBulkLoginWorkers] = useState("3");
  const [bulkLoginHeaded, setBulkLoginHeaded] = useState(true);
  const [message, setMessage] = useState<string | null>(null);

  // API-mode state.
  const [baseUrl, setBaseUrl] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [models, setModels] = useState<string[]>([]);
  const [fetching, setFetching] = useState(false);
  const [model, setModel] = useState("");
  const [bypass, setBypass] = useState(false);

  // Forget models fetched from a now-edited gateway so the picker + submitted model can't drift
  // out of sync with the URL/key.
  const resetFetchedModels = () => {
    setModels([]);
    setModel("");
  };

  const fetchModels = async () => {
    if (!baseUrl.trim().startsWith("https://")) {
      setMessage("Gateway URL must start with https://");
      return;
    }
    if (!apiKey.trim()) {
      setMessage("API key is required");
      return;
    }
    setMessage(null);
    setFetching(true);
    try {
      const list = await api.fetchGatewayModels(baseUrl.trim(), apiKey.trim());
      setModels(list);
      // Re-fetching after correcting the URL returns a different gateway's models. Keeping the
      // previous selection would leave the <select> blank while still submitting a model the new
      // gateway can't serve, so always re-anchor to the fetched list.
      setModel((current) => (current && list.includes(current) ? current : (list[0] ?? "")));
    } catch (err) {
      setMessage(errorMessage(err));
    } finally {
      setFetching(false);
    }
  };

  const submit = async () => {
    if (name.trim().length > 20) {
      setMessage("Account name is limited to 20 characters");
      return;
    }
    if (isCli && launcher.trim() === "" && !isBulkImport) {
      setMessage(`A custom command is required (e.g. ${tool.id}-work)`);
      return;
    }
    if (isBulkImport) {
      if (!bulkSessionFile) {
        setMessage("Choose the batch OAuth session export file first");
        return;
      }
      if (bulkSessionFile.size > 5 * 1024 * 1024) {
        setMessage("Session file is too large (maximum is 5 MB)");
        return;
      }
      let sessionJson: string;
      try {
        sessionJson = await bulkSessionFile.text();
      } catch {
        setMessage("Couldn't read the selected session file");
        return;
      }
      await onSubmitBulkImport({
        sessionJson,
        namePrefix: bulkNamePrefix.trim() || "acc",
        launcherPrefix: bulkLauncherPrefix.trim() || "bulk-",
      });
      return;
    }
    if (isBulkLogin) {
      if (!bulkLoginFile) {
        setMessage("Choose the email|password|2fa file first");
        return;
      }
      if (bulkLoginFile.size > 5 * 1024 * 1024) {
        setMessage("Credential file is too large (maximum is 5 MB)");
        return;
      }
      const workers = Number(bulkLoginWorkers);
      if (!Number.isInteger(workers) || workers < 1 || workers > 3) {
        setMessage("Workers must be between 1 and 3");
        return;
      }
      let credentialsText: string;
      try {
        credentialsText = await bulkLoginFile.text();
      } catch {
        setMessage("Couldn't read the selected credential file");
        return;
      }
      await onSubmitBulkLogin({
        credentialsText,
        workerPath: bulkLoginWorkerPath.trim() || undefined,
        workers,
        headed: bulkLoginHeaded,
      });
      setBulkLoginFile(null);
      return;
    }
    if (isImport) {
      if (!sessionFile) {
        setMessage("Choose the K12 session export file first");
        return;
      }
      if (sessionFile.size > 5 * 1024 * 1024) {
        setMessage("Session file is too large (maximum is 5 MB)");
        return;
      }
      let sessionJson: string;
      try {
        sessionJson = await sessionFile.text();
      } catch {
        setMessage("Couldn't read the selected session file");
        return;
      }
      await onSubmitImport({
        name: name.trim(),
        launcher: launcher.trim(),
        sessionJson,
      });
      return;
    }
    await onSubmit({
      toolId: tool.id,
      name: name.trim(),
      mode: isCli ? "login" : "import",
      launcher: isCli ? launcher.trim() : undefined,
      codexLoginMethod: tool.id === "codex" ? codexLoginMethod : undefined,
    });
  };

  const submitApi = async () => {
    if (name.trim().length > 20) {
      setMessage("Account name is limited to 20 characters");
      return;
    }
    if (models.length === 0) {
      setMessage("Fetch the gateway models first");
      return;
    }
    if (!model) {
      setMessage("Pick a model");
      return;
    }
    await onSubmitApi({
      toolId: tool.id,
      name: name.trim(),
      baseUrl: baseUrl.trim(),
      apiKey: apiKey.trim(),
      model,
      launcher: launcher.trim() || undefined,
      bypass,
    });
  };

  const [runSubmit, submitting] = useSingleFlight(() => (isApi ? submitApi() : submit()));

  return (
    <div className="modalBackdrop" role="presentation" onMouseDown={onClose}>
      <section className="modal" onMouseDown={(event) => event.stopPropagation()}>
        <h2>{isCli ? `Add ${tool.name} account` : "Save the signed-in Antigravity IDE account"}</h2>

        {canApi && (
          <div className={`kindToggle${canImport ? " fiveWay" : ""}`}>
            <button
              type="button"
              className={kind === "login" ? "active" : ""}
              onClick={() => setKind("login")}
            >
              Subscription (login)
            </button>
            {canImport && (
              <button
                type="button"
                className={kind === "import" ? "active" : ""}
                onClick={() => setKind("import")}
              >
                Import K12
              </button>
            )}
            {canImport && (
              <button
                type="button"
                className={kind === "bulk" ? "active" : ""}
                onClick={() => setKind("bulk")}
              >
                Bulk OAuth
              </button>
            )}
            {canImport && (
              <button
                type="button"
                className={kind === "auto" ? "active" : ""}
                onClick={() => setKind("auto")}
              >
                Auto login (staging)
              </button>
            )}
            <button
              type="button"
              className={kind === "api" ? "active" : ""}
              onClick={() => setKind("api")}
            >
              API / Proxy
            </button>
          </div>
        )}

        {!isBulkImport && !isBulkLogin && <label>
          Account name
          <input
            autoFocus
            value={name}
            maxLength={20}
            onChange={(event) => setName(event.target.value)}
            placeholder="Auto-generated if left blank"
          />
        </label>}

        {isCli && !isApi && (
          <>
            {tool.id === "codex" && !isImport && !isBulkImport && !isBulkLogin && (
              <fieldset className="codexLoginMethod">
                <legend>Sign-in method</legend>
                <div className="kindToggle">
                  <button
                    type="button"
                    className={codexLoginMethod === "deviceCode" ? "active" : ""}
                    onClick={() => setCodexLoginMethod("deviceCode")}
                  >
                    Device code
                  </button>
                  <button
                    type="button"
                    className={codexLoginMethod === "browserProfile" ? "active" : ""}
                    onClick={() => setCodexLoginMethod("browserProfile")}
                  >
                    Chrome profile (EDU)
                  </button>
                </div>
                <small>
                  {codexLoginMethod === "deviceCode"
                    ? "Current flow: enter a one-time code. Use this for personal accounts that allow device authentication."
                    : "Opens a separate Chrome profile, then starts standard browser sign-in. Use this for EDU/workspace accounts that block device codes."}
                </small>
              </fieldset>
            )}
            {isImport && (
              <div className="importSessionPanel">
                <label>
                  K12 session export
                  <input
                    type="file"
                    accept=".json,.txt,application/json,text/plain"
                    onChange={(event) => {
                      const file = event.target.files?.[0] ?? null;
                      if (file && file.size > 5 * 1024 * 1024) {
                        setSessionFile(null);
                        setMessage("Session file is too large (maximum is 5 MB)");
                        return;
                      }
                      setMessage(null);
                      setSessionFile(file);
                    }}
                  />
                </label>
                <small>
                  The file is read locally and reduced to Codex credentials. Nothing is uploaded
                  to a converter or sent to a third party.
                </small>
                {sessionFile && (
                  <span className="importSessionFile" title={sessionFile.name}>
                    Selected: {sessionFile.name}
                  </span>
                )}
              </div>
            )}
            {isBulkImport && (
              <div className="importSessionPanel">
                <label>
                  Batch OAuth session export
                  <input
                    type="file"
                    accept=".json,.txt,application/json,text/plain"
                    onChange={(event) => {
                      const file = event.target.files?.[0] ?? null;
                      if (file && file.size > 5 * 1024 * 1024) {
                        setBulkSessionFile(null);
                        setMessage("Session file is too large (maximum is 5 MB)");
                        return;
                      }
                      setMessage(null);
                      setBulkSessionFile(file);
                    }}
                  />
                </label>
                <small>
                  Import OAuth sessions only. The file is processed locally, reduced to Codex
                  credentials, and each new account is enrolled in the gateway pool.
                </small>
                {bulkSessionFile && (
                  <span className="importSessionFile" title={bulkSessionFile.name}>
                    Selected: {bulkSessionFile.name}
                  </span>
                )}
                <label>
                  Account name prefix
                  <input
                    value={bulkNamePrefix}
                    maxLength={16}
                    onChange={(event) => setBulkNamePrefix(event.target.value)}
                    placeholder="acc"
                  />
                </label>
                <label>
                  Command prefix
                  <input
                    value={bulkLauncherPrefix}
                    maxLength={16}
                    onChange={(event) => setBulkLauncherPrefix(event.target.value)}
                    placeholder="bulk-"
                  />
                </label>
              </div>
            )}
            {isBulkLogin && (
              <div className="importSessionPanel stagingLoginPanel">
                <label>
                  Credential file (`email|password|2fa`)
                  <input
                    type="file"
                    accept=".txt,.csv,text/plain"
                    onChange={(event) => {
                      const file = event.target.files?.[0] ?? null;
                      if (file && file.size > 5 * 1024 * 1024) {
                        setBulkLoginFile(null);
                        setMessage("Credential file is too large (maximum is 5 MB)");
                        return;
                      }
                      setMessage(null);
                      setBulkLoginFile(file);
                    }}
                  />
                </label>
                {bulkLoginFile && (
                  <span className="importSessionFile" title={bulkLoginFile.name}>
                    Selected: {bulkLoginFile.name}
                  </span>
                )}
                <label>
                  9router worker path (optional)
                  <input
                    value={bulkLoginWorkerPath}
                    onChange={(event) => setBulkLoginWorkerPath(event.target.value)}
                    placeholder={String.raw`D:\9router\Login tự động 9router v2\auto_login.py`}
                  />
                </label>
                <label>
                  Browser workers (1–3)
                  <input
                    type="number"
                    min={1}
                    max={3}
                    value={bulkLoginWorkers}
                    onChange={(event) => setBulkLoginWorkers(event.target.value)}
                  />
                </label>
                <label className="checkRow">
                  <input
                    type="checkbox"
                    checked={bulkLoginHeaded}
                    onChange={(event) => setBulkLoginHeaded(event.target.checked)}
                  />
                  Show browser windows (recommended for CAPTCHA/extra verification)
                </label>
                <div className="stagingWarning">
                  <strong>Staging only</strong>
                  <small>
                    Credentials are used transiently by the local worker, then erased. The app does
                    not save passwords or 2FA. CAPTCHA, security checks, and provider consent remain
                    manual; use only accounts you own.
                  </small>
                </div>
              </div>
            )}
            {!isBulkImport && !isBulkLogin && (
              <label>
                <span className="labelRow">
                  Custom command (required)
                  <span
                    className="helpDot"
                    title={`Open a terminal and run this command to use the account separately, in parallel. Forces the ${tool.id}- prefix, only a-z 0-9 - _`}
                  >
                    <CircleHelp />
                  </span>
                </span>
                <input
                  value={launcher}
                  onChange={(event) => setLauncher(event.target.value)}
                  placeholder={`${tool.id}-work`}
                />
              </label>
            )}
          </>
        )}

        {isApi && (
          <>
            <label>
              Gateway URL
              <input
                value={baseUrl}
                onChange={(event) => {
                  setBaseUrl(event.target.value);
                  // The fetched models belong to the previous gateway; drop them so a stale model
                  // can't be submitted against a URL it doesn't serve. A re-fetch is required.
                  resetFetchedModels();
                }}
                placeholder="https://your-gateway.com/v1"
              />
            </label>
            <label>
              API key
              <input
                type="password"
                value={apiKey}
                onChange={(event) => {
                  setApiKey(event.target.value);
                  resetFetchedModels();
                }}
                placeholder="sk-…"
              />
            </label>
            <button type="button" className="fetchModels" onClick={fetchModels} disabled={fetching}>
              {fetching ? <Loader2 className="spin" /> : <RefreshCw />}
              {models.length > 0 ? `Models loaded (${models.length})` : "Fetch models"}
            </button>

            {models.length > 0 && (
              <>
                <label>
                  <span className="labelRow">
                    Model (required)
                    <span
                      className="helpDot"
                      title="The gateway model this account runs. The CLI's model picker can't switch gateway models — add a separate account for a different model."
                    >
                      <CircleHelp />
                    </span>
                  </span>
                  <select value={model} onChange={(event) => setModel(event.target.value)}>
                    {models.map((id) => (
                      <option key={id} value={id}>
                        {id}
                      </option>
                    ))}
                  </select>
                </label>
                <p className="hint">
                  One account = one model. The gateway only knows its own model ids — for another
                  model, add another account.
                </p>

                <label>
                  <span className="labelRow">
                    Custom command (optional)
                    <span
                      className="helpDot"
                      title={`A separate command (e.g. ${tool.id}-p) to use this account in its own terminal. Forces the ${tool.id}- prefix.`}
                    >
                      <CircleHelp />
                    </span>
                  </span>
                  <input
                    value={launcher}
                    onChange={(event) => setLauncher(event.target.value)}
                    placeholder={`${tool.id}-p`}
                  />
                </label>

                <div className="apiToggleRow">
                  <span>
                    Bypass approvals &amp; sandbox in the custom command
                    <small>Adds {bypassFlag}. Off by default.</small>
                  </span>
                  <Toggle checked={bypass} onChange={setBypass} title="Bypass approvals & sandbox" />
                </div>
              </>
            )}
          </>
        )}

        {!isCli && (
          <p className="hint">
            Make sure Antigravity IDE is signed into the account you want to save, then click Save.
            The app will quit &amp; reopen the IDE to capture the right session (the IDE only writes
            the token on quit).
          </p>
        )}
        {message && <p className="quotaError">{message}</p>}
        <div className="modalActions">
          <button onClick={onClose}>Cancel</button>
          <button className="primary" onClick={runSubmit} disabled={submitting}>
            {isApi
              ? "Create account"
              : isImport
                ? "Import session"
                : isCli
                  ? "Create & login"
                  : "Save this account"}
          </button>
        </div>
      </section>
    </div>
  );
}

function NameDialog({
  title,
  label,
  hint,
  initialName,
  maxLength,
  submitText,
  onClose,
  onSubmit,
}: {
  title: string;
  label: string;
  hint?: string;
  initialName: string;
  maxLength: number;
  submitText: string;
  onClose: () => void;
  onSubmit: (name: string) => Promise<void>;
}) {
  const [name, setName] = useState(initialName);
  const [runSubmit, submitting] = useSingleFlight(() => onSubmit(name.trim()));

  return (
    <div className="modalBackdrop" role="presentation" onMouseDown={onClose}>
      <section className="modal" onMouseDown={(event) => event.stopPropagation()}>
        <h2>{title}</h2>
        <label>
          <span className="labelRow">
            {label}
            {hint && (
              <span className="helpDot" title={hint}>
                <CircleHelp />
              </span>
            )}
          </span>
          <input
            autoFocus
            value={name}
            maxLength={maxLength}
            onChange={(event) => setName(event.target.value)}
          />
        </label>
        <div className="modalActions">
          <button onClick={onClose}>Cancel</button>
          <button className="primary" onClick={runSubmit} disabled={submitting}>
            {submitText}
          </button>
        </div>
      </section>
    </div>
  );
}

function CliSetupDialog({
  tool,
  currentSetup,
  onClose,
  onSave,
}: {
  tool: ToolStatus;
  currentSetup: ToolSetup | undefined;
  onClose: () => void;
  onSave: (input: SetToolSetupInput) => Promise<void>;
}) {
  const [report, setReport] = useState<DetectionReport | null>(null);
  const [binaryPath, setBinaryPath] = useState(currentSetup?.binaryPath ?? "");
  const [configDir, setConfigDir] = useState(currentSetup?.defaultConfigDir ?? "");
  const [message, setMessage] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [manual, setManual] = useState(false);

  const detect = useCallback(async () => {
    setLoading(true);
    setMessage(null);
    try {
      const next = await api.detectToolSetup(tool.id);
      setReport(next);
      if (next.resolution.setup?.binaryPath) setBinaryPath(next.resolution.setup.binaryPath);
      if (next.resolution.setup?.defaultConfigDir) setConfigDir(next.resolution.setup.defaultConfigDir);
      if (!next.resolution.setup) {
        setManual(true);
        setMessage(next.resolution.reason);
      }
    } catch (err) {
      setMessage(errorMessage(err));
    } finally {
      setLoading(false);
    }
  }, [tool.id]);

  useEffect(() => {
    void detect();
  }, [detect]);

  const save = async () => {
    if (!binaryPath.trim() || !configDir.trim()) {
      setMessage("Both binary path and config directory are required");
      return;
    }
    await onSave({
      toolId: tool.id,
      binaryPath: binaryPath.trim(),
      defaultConfigDir: configDir.trim(),
    });
  };

  const [runSave, saving] = useSingleFlight(save);

  const binaryChoices = report?.binaryCandidates ?? [];
  const configChoices = report?.configCandidates ?? [];
  const simpleRecommendation =
    binaryChoices.length <= 1 &&
    configChoices.length <= 1 &&
    Boolean(binaryPath.trim()) &&
    Boolean(configDir.trim());
  const canEditPaths = manual || !simpleRecommendation;

  return (
    <div className="modalBackdrop" role="presentation" onMouseDown={onClose}>
      <section className="modal setupModal" onMouseDown={(event) => event.stopPropagation()}>
        <div className="modalTitleRow">
          <div>
            <h2>{tool.name} paths</h2>
            <p className="modalSub">
              Review the paths detected for this CLI.
            </p>
          </div>
          <button className="iconButton" onClick={detect} disabled={loading} title="Re-detect">
            <RefreshCw className={loading ? "spin" : ""} />
          </button>
        </div>

        <div className="setupCurrent">
          {canEditPaths ? (
            <>
              <label>
                CLI executable
                <input value={binaryPath} onChange={(event) => setBinaryPath(event.target.value)} />
              </label>
              <label>
                Main config folder
                <input value={configDir} onChange={(event) => setConfigDir(event.target.value)} />
              </label>
            </>
          ) : (
            <>
              <ReadOnlyPath label="CLI executable" value={binaryPath} />
              <ReadOnlyPath label="Main config folder" value={configDir} />
            </>
          )}
        </div>

        {simpleRecommendation && (
          <div className="setupSummary">
            <Check />
            <div>
              <strong>Recommended paths found</strong>
              <small>No extra choice is needed.</small>
            </div>
          </div>
        )}

        {report && !simpleRecommendation && (
          <div className="candidateGrid">
            <CandidateList
              title="Detected binaries"
              kind="binary"
              items={report.binaryCandidates}
              selected={binaryPath}
              onSelect={setBinaryPath}
            />
            <CandidateList
              title="Detected config folders"
              kind="config"
              items={report.configCandidates}
              selected={configDir}
              onSelect={setConfigDir}
            />
          </div>
        )}

        {message && <p className="quotaError">{message}</p>}
        <div className="modalActions">
          {!canEditPaths && (
            <button type="button" onClick={() => setManual(true)}>
              Enter manually
            </button>
          )}
          <button onClick={onClose}>Cancel</button>
          <button className="primary" onClick={runSave} disabled={saving}>
            {simpleRecommendation ? "Save recommended paths" : "Save paths"}
          </button>
        </div>
      </section>
    </div>
  );
}

function ReadOnlyPath({ label, value }: { label: string; value: string }) {
  return (
    <div className="readonlyPath">
      <span>{label}</span>
      <code title={normalizePathForDisplay(value)}>
        {formatPathForDisplay(value)}
      </code>
    </div>
  );
}

function CandidateList({
  title,
  kind,
  items,
  selected,
  onSelect,
}: {
  title: string;
  kind: "binary" | "config";
  items: Array<BinaryCandidate | ConfigCandidate>;
  selected: string;
  onSelect: (path: string) => void;
}) {
  return (
    <div className="candidateList">
      <strong>{title}</strong>
      {items.length === 0 ? (
        <span className="candidateEmpty">Nothing detected. Enter the path manually above.</span>
      ) : (
        items.map((item, index) => (
          <button
            key={`${item.source}:${item.path}`}
            type="button"
            className={`candidate ${selected === item.path ? "selected" : ""} ${item.valid ? "" : "weak"}`}
            onClick={() => onSelect(item.path)}
          >
            <span className="candidatePath" title={normalizePathForDisplay(item.path)}>
              {formatPathForDisplay(item.path)}
            </span>
            <span className="candidateMeta">
              {candidateLabel(item, index, kind)}
            </span>
            {item.warnings.length > 0 && <span className="candidateWarn">{item.warnings[0]}</span>}
          </button>
        ))
      )}
    </div>
  );
}

function stateLabel(state: Account["state"]) {
  if (state === "needsLogin") return "Needs login";
  if (state === "exhausted") return "Out of quota";
  if (state === "active") return "In use";
  return "Ready";
}

function badgeClass(state: Account["state"]) {
  if (state === "active") return "ok";
  if (state === "exhausted") return "warn";
  if (state === "needsLogin") return "bad";
  return "muted";
}

function sourceLabel(source: string) {
  if (source === "env") return "from environment";
  if (source === "default") return "standard location";
  if (source === "path") return "found in PATH";
  if (source === "manual") return "manual";
  return source;
}

function candidateLabel(item: BinaryCandidate | ConfigCandidate, index: number, kind: "binary" | "config") {
  const tags = [index === 0 && item.valid ? "Recommended" : null, sourceLabel(item.source)]
    .filter(Boolean)
    .join(" · ");
  if (!item.valid) return `${tags} · needs review`;
  if (kind === "config" && item.source === "default") return `${tags} · main config`;
  return tags;
}

function formatTime(value: string) {
  return new Intl.DateTimeFormat("en-US", {
    dateStyle: "short",
    timeStyle: "short",
  }).format(new Date(value));
}

/** Compact reset stamp shown inline on a quota row, e.g. "06/07, 17:07". */
function formatReset(value: string) {
  return new Intl.DateTimeFormat("en-US", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  }).format(new Date(value));
}

function errorMessage(err: unknown) {
  return err instanceof Error ? err.message : String(err);
}
