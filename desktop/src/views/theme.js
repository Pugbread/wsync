// views/theme.js — appearance presets and the custom-property token maps
// behind them (Design 8.2).
//
// Four presets: system, dark, black, light. `system` resolves through
// `prefers-color-scheme` and re-resolves live when the OS flips. Ro-Sync's
// fifth preset, `host`, was for the terminal-widget surface and is deliberately
// absent — WSync's only surface is the desktop window (Design 1.4).
//
// Applying a preset always removes every property this module owns before
// writing the new set, so switching themes can never leave a stale token behind
// from a preset that happened to define more keys.

export const THEME_IDS = Object.freeze(["system", "dark", "black", "light"]);

const THEME_SET = new Set(THEME_IDS);

export const THEME_OPTIONS = Object.freeze([
  Object.freeze({
    id: "system",
    label: "System",
    description: "Follow this computer's light or dark appearance.",
  }),
  Object.freeze({
    id: "dark",
    label: "Dark",
    description: "WSync's default: cool slate surfaces, soft depth.",
  }),
  Object.freeze({
    id: "black",
    label: "Black",
    description: "True black for OLED displays, with lifted panels.",
  }),
  Object.freeze({
    id: "light",
    label: "Light",
    description: "Bright neutral surfaces with crisp separation.",
  }),
]);

// Tokens shared by both dark presets. Hue system, used consistently across the
// app: indigo = primary action and focus; green = healthy / added; amber =
// attention / differs; rose = destructive / missing.
const DARK_SHARED = Object.freeze({
  "--fg": "#e8ecf3",
  "--muted": "#98a3b3",
  "--faint": "#6c7789",
  "--accent": "#7d8cf8",
  "--accent-hover": "#96a2ff",
  "--accent-contrast": "#0a0c1c",
  "--accent-soft": "rgba(125, 140, 248, 0.15)",
  "--accent-line": "rgba(125, 140, 248, 0.42)",
  "--ok": "#46c68b",
  "--ok-soft": "rgba(70, 198, 139, 0.14)",
  "--warn": "#dda65a",
  "--warn-soft": "rgba(221, 166, 90, 0.15)",
  "--danger": "#ef6b7c",
  "--danger-contrast": "#1b0d11",
  "--danger-soft": "rgba(239, 107, 124, 0.14)",
  "--diff-add-bg": "rgba(70, 198, 139, 0.16)",
  "--diff-add-fg": "#a6e8c6",
  "--diff-del-bg": "rgba(239, 107, 124, 0.16)",
  "--diff-del-fg": "#f6bcc4",
  "--scrim": "rgba(3, 5, 9, 0.62)",
  "--scroll-thumb": "rgba(255, 255, 255, 0.13)",
  "--scroll-thumb-hover": "rgba(255, 255, 255, 0.22)",
  "--hairline": "rgba(255, 255, 255, 0.07)",
  "--hairline-strong": "rgba(255, 255, 255, 0.13)",
  "--ring": "0 0 0 3px rgba(125, 140, 248, 0.32)",
});

export const THEME_TOKENS = Object.freeze({
  dark: Object.freeze({
    ...DARK_SHARED,
    "--bg": "#0f1216",
    "--surface": "#161a21",
    "--surface-2": "#1c222a",
    "--surface-3": "#242b35",
    "--inset": "#0a0d11",
    "--border": "#2a313c",
    "--border-strong": "#3a434f",
    "--shadow-1": "0 1px 2px rgba(0, 0, 0, 0.30)",
    "--shadow-2": "0 2px 6px rgba(0, 0, 0, 0.32), 0 10px 30px rgba(0, 0, 0, 0.26)",
    "--shadow-modal": "0 1px 0 rgba(255, 255, 255, 0.04) inset, 0 24px 70px rgba(0, 0, 0, 0.62)",
  }),
  black: Object.freeze({
    ...DARK_SHARED,
    "--bg": "#000000",
    "--fg": "#f2f5fa",
    "--surface": "#07090c",
    "--surface-2": "#0d1015",
    "--surface-3": "#161b23",
    "--inset": "#000000",
    "--border": "#20262f",
    "--border-strong": "#2f3743",
    "--scrim": "rgba(0, 0, 0, 0.72)",
    "--shadow-1": "0 1px 2px rgba(0, 0, 0, 0.6)",
    "--shadow-2": "0 2px 8px rgba(0, 0, 0, 0.7), 0 14px 36px rgba(0, 0, 0, 0.55)",
    "--shadow-modal": "0 1px 0 rgba(255, 255, 255, 0.03) inset, 0 28px 80px rgba(0, 0, 0, 0.9)",
  }),
  light: Object.freeze({
    "--bg": "#f3f5f9",
    "--fg": "#141b25",
    "--muted": "#5a6675",
    "--faint": "#7c8797",
    "--accent": "#4a5ae0",
    "--accent-hover": "#3a49c9",
    "--accent-contrast": "#ffffff",
    "--accent-soft": "rgba(74, 90, 224, 0.10)",
    "--accent-line": "rgba(74, 90, 224, 0.38)",
    "--ok": "#127a52",
    "--ok-soft": "rgba(18, 122, 82, 0.11)",
    "--warn": "#8d5d15",
    "--warn-soft": "rgba(141, 93, 21, 0.12)",
    "--danger": "#c5384a",
    "--danger-contrast": "#ffffff",
    "--danger-soft": "rgba(197, 56, 74, 0.10)",
    "--surface": "#ffffff",
    "--surface-2": "#eef1f6",
    "--surface-3": "#e3e8f0",
    "--inset": "#f7f9fc",
    "--border": "#d4dae4",
    "--border-strong": "#a9b3c1",
    "--diff-add-bg": "rgba(18, 122, 82, 0.12)",
    "--diff-add-fg": "#0d5c3d",
    "--diff-del-bg": "rgba(197, 56, 74, 0.11)",
    "--diff-del-fg": "#93283a",
    "--scrim": "rgba(20, 27, 37, 0.34)",
    "--scroll-thumb": "rgba(20, 27, 37, 0.16)",
    "--scroll-thumb-hover": "rgba(20, 27, 37, 0.28)",
    "--hairline": "rgba(20, 27, 37, 0.08)",
    "--hairline-strong": "rgba(20, 27, 37, 0.16)",
    "--ring": "0 0 0 3px rgba(74, 90, 224, 0.26)",
    "--shadow-1": "0 1px 2px rgba(20, 27, 37, 0.07)",
    "--shadow-2": "0 2px 5px rgba(20, 27, 37, 0.08), 0 10px 28px rgba(20, 27, 37, 0.07)",
    "--shadow-modal": "0 1px 0 rgba(255, 255, 255, 0.9) inset, 0 24px 70px rgba(20, 27, 37, 0.22)",
  }),
});

// Every property any preset can set. Reset covers the union, not just the
// incoming preset's keys, so a switch never leaves an orphan behind.
export const OWNED_PROPERTIES = Object.freeze([
  ...new Set(Object.values(THEME_TOKENS).flatMap((tokens) => Object.keys(tokens))),
]);

const DARK_QUERY = "(prefers-color-scheme: dark)";

export function systemPrefersDark() {
  return globalThis.matchMedia?.(DARK_QUERY)?.matches ?? true;
}

export function normalizeTheme(value) {
  return typeof value === "string" && THEME_SET.has(value) ? value : "system";
}

/**
 * Resolve a preference into the concrete token set to write.
 * @returns {{preference: string, effective: string, colorScheme: string, tokens: object}}
 */
export function resolveTheme(value, { systemDark = systemPrefersDark() } = {}) {
  const preference = normalizeTheme(value);
  const effective = preference === "system" ? (systemDark ? "dark" : "light") : preference;
  return {
    preference,
    effective,
    colorScheme: effective === "light" ? "light" : "dark",
    tokens: { ...THEME_TOKENS[effective] },
  };
}

/**
 * Write a preset onto an element (normally `<html>`) and stamp `data-theme`.
 * Pass `reveal: false` to keep the app hidden (used before first paint).
 */
export function applyTheme(root, value, options = {}) {
  const resolved = resolveTheme(value, options);
  if (!root?.style || !root?.dataset) return resolved;

  for (const property of OWNED_PROPERTIES) root.style.removeProperty(property);
  for (const [property, token] of Object.entries(resolved.tokens)) {
    root.style.setProperty(property, token);
  }
  root.style.colorScheme = resolved.colorScheme;
  root.dataset.theme = resolved.effective;
  root.dataset.themePreference = resolved.preference;
  if (options.reveal !== false) revealTheme(root);
  return resolved;
}

export function revealTheme(root = document.documentElement) {
  root.classList?.remove("theme-pending");
}

/**
 * Reveal the UI no later than 800 ms after boot, even if loading persisted
 * state hangs or fails. A broken state read must never leave a blank window
 * (Design 8.2).
 */
export function scheduleThemeReveal(root = document.documentElement, delayMs = 800) {
  const timer = setTimeout(() => revealTheme(root), delayMs);
  return () => clearTimeout(timer);
}

/**
 * Re-resolve while the preference is `system` and the OS appearance changes.
 * @returns {() => void} unsubscribe
 */
export function watchSystemTheme(onChange) {
  const query = globalThis.matchMedia?.(DARK_QUERY);
  if (!query?.addEventListener) return () => {};
  const handler = (event) => onChange(event.matches);
  query.addEventListener("change", handler);
  return () => query.removeEventListener("change", handler);
}
