// Theme & font registries for the central-logs UI.
//
// A theme is a set of CSS custom properties (`--cl-*`) defined per
// `[data-theme="<id>"]` in index.css. Applying a theme only touches three
// attributes on <html>:
//   data-theme="dark"   → selects the palette
//   class "dark" (or not) → flips Tailwind `dark:` variants so hardcoded
//                            utility pairs (pills, error text) match
//   data-font / data-mono → select the font stacks
//
// index.html contains a tiny copy of the default/dark-theme id list so the
// palette is applied before first paint (no flash). Keep the two in sync.

export interface ThemeDef {
  id: string;
  label: string;
  group: "Base" | "Editor & design systems";
  dark: boolean;
  /** Three colors shown as a preview chip in the switcher. */
  swatch: [string, string, string];
}

export const THEMES: ThemeDef[] = [
  {
    id: "dark",
    label: "Mission Control",
    group: "Base",
    dark: true,
    swatch: ["#0a0e16", "#5b8dff", "#eef2f9"],
  },
  {
    id: "light",
    label: "Light",
    group: "Base",
    dark: false,
    swatch: ["#ffffff", "#3b6fe0", "#101624"],
  },
  {
    id: "contrast",
    label: "High Contrast",
    group: "Base",
    dark: true,
    swatch: ["#000000", "#4da3ff", "#ffffff"],
  },
  {
    id: "tokyo-night",
    label: "Tokyo Night",
    group: "Editor & design systems",
    dark: true,
    swatch: ["#1a1b26", "#7aa2f7", "#c0caf5"],
  },
  {
    id: "dracula",
    label: "Dracula",
    group: "Editor & design systems",
    dark: true,
    swatch: ["#282a36", "#bd93f9", "#f8f8f2"],
  },
  {
    id: "nord",
    label: "Nord",
    group: "Editor & design systems",
    dark: true,
    swatch: ["#2e3440", "#81a1c1", "#eceff4"],
  },
  {
    id: "catppuccin",
    label: "Catppuccin Mocha",
    group: "Editor & design systems",
    dark: true,
    swatch: ["#1e1e2e", "#89b4fa", "#cdd6f4"],
  },
  {
    id: "one-dark",
    label: "One Dark",
    group: "Editor & design systems",
    dark: true,
    swatch: ["#282c34", "#61afef", "#abb2bf"],
  },
  {
    id: "solarized-light",
    label: "Solarized Light",
    group: "Editor & design systems",
    dark: false,
    swatch: ["#fdf6e3", "#268bd2", "#586e75"],
  },
];

export interface FontDef {
  id: string;
  label: string;
}

export const FONTS: FontDef[] = [
  { id: "inter", label: "Inter (default)" },
  { id: "system", label: "System UI" },
  { id: "plex", label: "IBM Plex Sans" },
  { id: "roboto", label: "Roboto" },
  { id: "grotesk", label: "Space Grotesk" },
];

export const MONOS: FontDef[] = [
  { id: "jetbrains", label: "JetBrains Mono (default)" },
  { id: "system-mono", label: "System mono" },
  { id: "fira", label: "Fira Code" },
  { id: "plex-mono", label: "IBM Plex Mono" },
  { id: "cascadia", label: "Cascadia Code" },
];

export const DEFAULT_THEME = "dark";
export const DEFAULT_FONT = "inter";
export const DEFAULT_MONO = "jetbrains";

const STORAGE_KEYS = {
  theme: "cl-theme",
  font: "cl-font",
  mono: "cl-mono",
} as const;

function findTheme(id: string | null): ThemeDef {
  return THEMES.find((t) => t.id === id) ?? THEMES[0];
}

function stored(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function store(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    /* private mode etc. — theme just won't persist */
  }
}

export function currentThemeId(): string {
  return findTheme(stored(STORAGE_KEYS.theme)).id;
}

export function applyTheme(id: string): ThemeDef {
  const theme = findTheme(id);
  const el = document.documentElement;
  el.setAttribute("data-theme", theme.id);
  el.classList.toggle("dark", theme.dark);
  store(STORAGE_KEYS.theme, theme.id);
  return theme;
}

export function applyFont(id: string): void {
  const font = FONTS.find((f) => f.id === id) ?? FONTS[0];
  document.documentElement.setAttribute("data-font", font.id);
  store(STORAGE_KEYS.font, font.id);
}

export function applyMono(id: string): void {
  const mono = MONOS.find((f) => f.id === id) ?? MONOS[0];
  document.documentElement.setAttribute("data-mono", mono.id);
  store(STORAGE_KEYS.mono, mono.id);
}

/** Re-applies the persisted selections (idempotent; safe on mount). */
export function initTheme(): void {
  applyTheme(currentThemeId());
  applyFont(stored(STORAGE_KEYS.font) ?? DEFAULT_FONT);
  applyMono(stored(STORAGE_KEYS.mono) ?? DEFAULT_MONO);
}
