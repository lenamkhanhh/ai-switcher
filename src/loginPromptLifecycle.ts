import type { AccountState, ToolId } from "./types";

export interface LoginPromptEvent {
  toolId: ToolId;
  accountId: string;
  attemptId: number;
  loginUrl: string | null;
  deviceCode: string | null;
}

export interface LoginPromptFinishedEvent {
  toolId: ToolId;
  accountId: string;
  attemptId: number;
  success: boolean;
}

interface LoginAccountState {
  id: string;
  state: AccountState;
}

interface LoginToolState {
  id: ToolId;
  accounts: LoginAccountState[];
}

export function mergeLoginPrompt(
  current: LoginPromptEvent | null,
  next: LoginPromptEvent,
): LoginPromptEvent {
  if (current && current.attemptId > next.attemptId) return current;
  if (
    current?.toolId !== next.toolId ||
    current.accountId !== next.accountId
  ) {
    return next;
  }
  if (current.attemptId < next.attemptId) return next;
  return {
    ...current,
    loginUrl: next.loginUrl ?? current.loginUrl,
    deviceCode: next.deviceCode ?? current.deviceCode,
  };
}

export function finishLoginPrompt(
  current: LoginPromptEvent | null,
  finished: LoginPromptFinishedEvent,
): LoginPromptEvent | null {
  if (
    current?.toolId === finished.toolId &&
    current.accountId === finished.accountId &&
    finished.attemptId >= current.attemptId
  ) {
    return null;
  }
  return current;
}

export function reconcileLoginPromptWithTools(
  current: LoginPromptEvent | null,
  tools: LoginToolState[],
): LoginPromptEvent | null {
  if (!current || tools.length === 0) return current;
  const tool = tools.find((item) => item.id === current.toolId);
  if (!tool) return current;
  const account = tool.accounts.find((item) => item.id === current.accountId);
  if (!account) return current;
  return account.state === "needsLogin" ? current : null;
}
