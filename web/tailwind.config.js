/** @type {import('tailwindcss').Config} */

// ─────────────────────────────────────────────────────────────────────────────
// central-logs design system — "mission control"
//
// Every Tremor v3 class used in the app (`tremor-*` / `dark-tremor-*`) maps
// onto the same `--cl-*` CSS variables (see src/index.css). The active theme
// is a `data-theme` attribute on <html>; `dark:` variants are driven by the
// `dark` class toggled alongside it, so light and dark themes both work.
// ─────────────────────────────────────────────────────────────────────────────

const token = {
  brand: {
    faint: "var(--cl-brand-faint)",
    muted: "var(--cl-brand-muted)",
    subtle: "var(--cl-brand-subtle)",
    DEFAULT: "var(--cl-brand)",
    emphasis: "var(--cl-brand-emphasis)",
    inverted: "var(--cl-brand-inverted)",
  },
  background: {
    muted: "var(--cl-bg-muted)",
    subtle: "var(--cl-bg-subtle)",
    DEFAULT: "var(--cl-bg)",
    emphasis: "var(--cl-bg-emphasis)",
  },
  border: { DEFAULT: "var(--cl-border)" },
  ring: { DEFAULT: "var(--cl-ring)" },
  content: {
    subtle: "var(--cl-content-subtle)",
    DEFAULT: "var(--cl-content)",
    emphasis: "var(--cl-content-emphasis)",
    strong: "var(--cl-content-strong)",
    inverted: "var(--cl-content-inverted)",
  },
};

/** @type {import('tailwindcss').Config} */
export default {
  content: [
    "./index.html",
    "./src/**/*.{ts,tsx}",
    "./node_modules/@tremor/**/*.{js,ts,jsx,tsx}",
  ],
  darkMode: "class",
  theme: {
    transparent: "transparent",
    current: "currentColor",
    extend: {
      colors: {
        tremor: token,
        "dark-tremor": token,
      },
      fontFamily: {
        sans: ["var(--cl-font-sans)"],
        mono: ["var(--cl-font-mono)"],
      },
      boxShadow: {
        "tremor-input": "none",
        "tremor-card":
          "0 1px 0 0 rgba(255,255,255,0.02) inset, 0 8px 24px -12px var(--cl-shadow)",
        "tremor-dropdown":
          "0 12px 32px -8px var(--cl-shadow), 0 0 0 1px var(--cl-border)",
        "dark-tremor-input": "none",
        "dark-tremor-card":
          "0 1px 0 0 rgba(255,255,255,0.02) inset, 0 8px 24px -12px var(--cl-shadow)",
        "dark-tremor-dropdown":
          "0 12px 32px -8px var(--cl-shadow), 0 0 0 1px var(--cl-border)",
      },
      keyframes: {
        "cl-fade-in": { from: { opacity: "0", transform: "translateY(2px)" }, to: { opacity: "1", transform: "translateY(0)" } },
      },
      animation: {
        "cl-fade-in": "cl-fade-in 180ms ease-out",
      },
    },
  },
  safelist: [
    // dynamic pill tints (see components.tsx LevelBadge / SeverityBadge) —
    // light-friendly + dark: variants so every theme stays readable
    "bg-slate-400/15", "text-slate-600", "text-slate-300",
    "bg-emerald-400/15", "text-emerald-600", "text-emerald-300",
    "bg-amber-400/15", "text-amber-600", "text-amber-300",
    "bg-red-400/15", "text-red-600", "text-red-300",
    "bg-rose-400/15", "text-rose-600", "text-rose-300",
    "bg-orange-400/15", "text-orange-600", "text-orange-300",
  ],
  plugins: [
    // Minimal shim for Headless UI state variants used by Tremor internals
    // (Select / Popover panels) without pulling in the extra plugin package.
    function ({ addVariant }) {
      addVariant("ui-open", "&[data-headlessui-state='open'], [data-headlessui-state='open'] &");
    },
  ],
};
