// Header dropdown for switching theme & fonts. Selections are applied
// immediately (attributes on <html>) and persisted to localStorage.

import { useEffect, useRef, useState } from "react";
import {
  THEMES,
  FONTS,
  MONOS,
  currentThemeId,
  applyTheme,
  applyFont,
  applyMono,
} from "./theme";

function PaletteIcon() {
  return (
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
      <path d="M12 3a9 9 0 1 0 0 18h1.5a2.5 2.5 0 0 0 0-5H12a2 2 0 0 1-2-2 2 2 0 0 1 2-2h1.5A5.5 5.5 0 0 0 19 6.5 9 9 0 0 0 12 3Z" />
      <circle cx="7.5" cy="10.5" r="0.9" fill="currentColor" stroke="none" />
      <circle cx="12" cy="7.5" r="0.9" fill="currentColor" stroke="none" />
      <circle cx="16.5" cy="10.5" r="0.9" fill="currentColor" stroke="none" />
    </svg>
  );
}

function ThemeSwatch({ swatch }: { swatch: [string, string, string] }) {
  return (
    <span className="flex h-5 w-9 shrink-0 overflow-hidden rounded border border-tremor-border">
      <span className="h-full w-1/2" style={{ background: swatch[0] }} />
      <span className="h-full w-1/4" style={{ background: swatch[1] }} />
      <span className="h-full w-1/4" style={{ background: swatch[2] }} />
    </span>
  );
}

export default function ThemeMenu() {
  const [open, setOpen] = useState(false);
  const [theme, setTheme] = useState(currentThemeId);
  const [font, setFont] = useState(
    () => document.documentElement.getAttribute("data-font") ?? FONTS[0].id,
  );
  const [mono, setMono] = useState(
    () => document.documentElement.getAttribute("data-mono") ?? MONOS[0].id,
  );
  const rootRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    function onDown(e: MouseEvent) {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    }
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") setOpen(false);
    }
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  const groups = [...new Set(THEMES.map((t) => t.group))];

  return (
    <div className="relative" ref={rootRef}>
      <button
        type="button"
        title="Theme & fonts"
        aria-label="Theme & fonts"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
        className="flex h-8 w-8 items-center justify-center rounded-md border border-tremor-border text-tremor-content-subtle hover:bg-tremor-background-subtle hover:text-tremor-content-strong transition-colors"
      >
        <PaletteIcon />
      </button>

      {open && (
        <div className="absolute right-0 top-full z-50 mt-2 w-72 rounded-lg border border-tremor-border bg-tremor-background-muted p-3 shadow-tremor-dropdown animate-cl-fade-in">
          {groups.map((group, gi) => (
            <div key={group} className={gi > 0 ? "mt-3 pt-3 border-t border-tremor-border" : ""}>
              <div className="cl-stat-label mb-2">{group}</div>
              <div className="grid grid-cols-1 gap-1">
                {THEMES.filter((t) => t.group === group).map((t) => (
                  <button
                    key={t.id}
                    type="button"
                    onClick={() => {
                      applyTheme(t.id);
                      setTheme(t.id);
                    }}
                    className={`flex w-full items-center gap-2.5 rounded-md px-2 py-1.5 text-left text-sm transition-colors ${
                      theme === t.id
                        ? "bg-tremor-brand-faint text-tremor-brand"
                        : "text-tremor-content hover:bg-tremor-background-subtle hover:text-tremor-content-strong"
                    }`}
                  >
                    <ThemeSwatch swatch={t.swatch} />
                    <span className="flex-1">{t.label}</span>
                    {theme === t.id && (
                      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                        <path d="M5 13l4 4L19 7" />
                      </svg>
                    )}
                  </button>
                ))}
              </div>
            </div>
          ))}

          <div className="mt-3 pt-3 border-t border-tremor-border">
            <div className="cl-stat-label mb-2">Fonts</div>
            <label className="mb-2 block">
              <span className="mb-1 block text-xs text-tremor-content-subtle">Interface</span>
              <select
                value={font}
                onChange={(e) => {
                  applyFont(e.target.value);
                  setFont(e.target.value);
                }}
                className="w-full rounded-md border border-tremor-border bg-tremor-background px-2 py-1.5 text-sm text-tremor-content-emphasis focus:border-tremor-brand"
              >
                {FONTS.map((f) => (
                  <option key={f.id} value={f.id}>{f.label}</option>
                ))}
              </select>
            </label>
            <label className="block">
              <span className="mb-1 block text-xs text-tremor-content-subtle">Monospace (logs)</span>
              <select
                value={mono}
                onChange={(e) => {
                  applyMono(e.target.value);
                  setMono(e.target.value);
                }}
                className="w-full rounded-md border border-tremor-border bg-tremor-background px-2 py-1.5 text-sm text-tremor-content-emphasis focus:border-tremor-brand"
              >
                {MONOS.map((f) => (
                  <option key={f.id} value={f.id}>{f.label}</option>
                ))}
              </select>
            </label>
          </div>
        </div>
      )}
    </div>
  );
}
