const MAX_PATH_DISPLAY_LENGTH = 58;

function isWindowsPath(path: string) {
  return /^[A-Za-z]:[\\/]/.test(path) || path.includes("\\");
}

export function normalizePathForDisplay(path: string) {
  return isWindowsPath(path) ? path.replace(/\//g, "\\") : path;
}

export function formatPathForDisplay(
  path: string,
  maxLength = MAX_PATH_DISPLAY_LENGTH,
) {
  const normalized = normalizePathForDisplay(path);
  let compact = normalized;

  if (isWindowsPath(normalized)) {
    compact = normalized.replace(/^[A-Za-z]:\\Users\\[^\\]+\\/i, "~\\");
  } else {
    compact = normalized.replace(/^\/Users\/[^/]+\//, "~/");
  }

  if (compact.length <= maxLength) return compact;
  return `…${compact.slice(-(maxLength - 1))}`;
}
