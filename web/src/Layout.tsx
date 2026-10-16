import { Link, NavLink, Outlet } from "react-router-dom";
import { useEffect, useRef, useState } from "react";
import { api, type PipelineStatus, WhoamiResponse } from "./api";
import { usePoll } from "./components";
import ThemeMenu from "./ThemeMenu";

interface LayoutProps {
  whoami: WhoamiResponse;
}

/** Compact global status indicator. Polls `/api/pipeline` (the same
 *  endpoint the Pipeline page uses) so we never make a second request
 *  just to render the top bar — and so the numbers the user sees
 *  there match the Pipeline page verbatim.
 *
 *  Three values, left → right:
 *    • logs/m     — records ingested over the trailing 60s
 *    • Queue      — link to /pipeline (clicking opens the pipeline page);
 *                   shows fill % + absolute depth (rows). Color-coded.
 *    • Services   — distinct services in last 24h; the full
 *                   "Active services (24h)" label appears on hover.
 */
/** Live pipeline snapshot (same `/api/pipeline` data as the Pipeline page).
 *
 *  Two layouts:
 *    • row   — the old inline strip (kept for the mobile nav sheet)
 *    • stack — label/value rows for the account-menu "Live status" section;
 *              the Queue row links to /pipeline and calls `onNavigate`
 *              so the menu closes on click.
 */
function HeaderStatus({ compact = false, stack = false, onNavigate }: {
  compact?: boolean;
  stack?: boolean;
  onNavigate?: () => void;
}) {
  const { data } = usePoll<PipelineStatus>(() => api.pipeline(), 10_000, []);
  const fillPct = data ? Math.min(100, data.channel_fill_ratio * 100) : 0;
  const fillColor =
    fillPct > 80
      ? "text-red-600 dark:text-red-300"
      : fillPct > 50
      ? "text-amber-600 dark:text-amber-300"
      : "text-emerald-600 dark:text-emerald-300";

  // Render placeholder dashes before the first poll resolves so the
  // surrounding layout doesn't jump around as data arrives.
  if (!data) {
    if (stack) {
      return (
        <div className="cl-mono text-xs space-y-1.5 text-tremor-content-subtle dark:text-dark-tremor-content-subtle">
          <div className="flex items-center justify-between gap-3"><span>Throughput</span><span>—</span></div>
          <div className="flex items-center justify-between gap-3"><span>Queue</span><span>—</span></div>
          <div className="flex items-center justify-between gap-3"><span>Services</span><span>—</span></div>
        </div>
      );
    }
    return (
      <div className="flex items-center gap-4 text-xs cl-mono text-tremor-content-subtle dark:text-dark-tremor-content-subtle whitespace-nowrap">
        <span>— logs/m</span>
        <span>Queue: —</span>
        <span title="Active services (24h)">Services: —</span>
      </div>
    );
  }

  const lpm = data.logs_per_minute;
  const lpmLabel =
    lpm >= 1000
      ? `${(lpm / 1000).toFixed(lpm >= 10_000 ? 0 : 1)}k`
      : `${lpm}`;
  const services = data.active_services_24h;
  const queueDepth = data.channel_depth;
  const queueCap = data.channel_capacity;

  if (stack) {
    const subtle = "text-tremor-content-subtle dark:text-dark-tremor-content-subtle";
    const strong = "text-tremor-content-emphasis dark:text-dark-tremor-content-emphasis";
    return (
      <div
        className="cl-mono text-xs space-y-1.5"
        title="live traffic snapshot — same data as the Pipeline page"
      >
        <div className="flex items-center justify-between gap-3">
          <span className={subtle}>Throughput</span>
          <span className={strong}>{lpmLabel} logs/m</span>
        </div>
        <div className="flex items-center justify-between gap-3">
          <span className={subtle} title="Open the pipeline view">Queue</span>
          <Link
            to="/pipeline"
            onClick={onNavigate}
            className={`${fillColor} hover:underline underline-offset-2 whitespace-nowrap`}
            title="Open the pipeline view"
          >
            {fillPct.toFixed(0)}% ({formatRows(queueDepth)} / {formatRows(queueCap)})
          </Link>
        </div>
        <div className="flex items-center justify-between gap-3">
          <span className={subtle} title="Active services (24h) — distinct services seen in the last 24h">Services</span>
          <span className={strong}>{services}</span>
        </div>
      </div>
    );
  }

  return (
    <div
      className={`flex items-center ${compact ? "gap-3" : "gap-4"} text-xs cl-mono whitespace-nowrap`}
      title="live traffic snapshot — same data as the Pipeline page"
    >
      <span className="text-tremor-content-emphasis dark:text-dark-tremor-content-emphasis">
        {lpmLabel} logs/m
      </span>
      <Link
        to="/pipeline"
        className={`${fillColor} hover:underline underline-offset-2`}
        title="Open the pipeline view"
      >
        Queue: {fillPct.toFixed(0)}% ({formatRows(queueDepth)} /{" "}
        {formatRows(queueCap)})
      </Link>
      <span
        className="text-tremor-content-emphasis dark:text-dark-tremor-content-emphasis"
        title="Active services (24h) — distinct services seen in the last 24h"
      >
        Services: {services}
      </span>
    </div>
  );
}

/** Compact integer-with-thousand-separators for queue depth. We deliberately
 *  don't promote to "1.2k" / "1.2M" because the channel depth is a small
 *  bounded integer the operator wants to see verbatim. */
function formatRows(n: number): string {
  return n.toLocaleString("en-US");
}

/** Map the backend's Debug-formatted lowercase enum into a human label. */
function formatVia(via: string): string {
  switch (via) {
    case "staticadminkey":
      return "Static admin key";
    case "cookie":
      return "Browser session";
    case "bearer":
      return "API key session";
    default:
      return via;
  }
}

/** Account dropdown behind the avatar button: identity, live pipeline
 *  status, overflow pages (Storage / API Keys — kept out of the header
 *  bar so it stays short), scopes, sign out. Click-outside + Escape close. */
function UserMenu({ whoami, onLogout, overflowLinks }: {
  whoami: WhoamiResponse;
  onLogout: () => Promise<void>;
  overflowLinks: NavItem[];
}) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
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

  async function handleSignOut() {
    setBusy(true);
    try {
      await onLogout();
    } finally {
      setBusy(false);
      setOpen(false);
    }
  }

  function close() {
    setOpen(false);
  }

  const menuLinkCls = ({ isActive }: { isActive: boolean }) =>
    `flex w-full items-center rounded-md px-2.5 py-1.5 text-sm transition-colors ${
      isActive
        ? "bg-tremor-brand-faint text-tremor-brand dark:bg-dark-tremor-brand-faint dark:text-dark-tremor-brand font-medium"
        : "text-tremor-content-subtle dark:text-dark-tremor-content-subtle hover:bg-tremor-background-subtle dark:hover:bg-dark-tremor-background-subtle hover:text-tremor-content-strong dark:hover:text-dark-tremor-content-strong"
    }`;

  return (
    <div className="relative" ref={rootRef}>
      <button
        type="button"
        aria-label="Account menu"
        aria-expanded={open}
        title={whoami.name}
        onClick={() => setOpen((o) => !o)}
        className="flex h-8 w-8 items-center justify-center rounded-full bg-dark-tremor-background-emphasis text-[12px] font-semibold text-dark-tremor-content-strong hover:ring-2 hover:ring-tremor-brand/40 transition-shadow"
      >
        {whoami.name.slice(0, 2).toUpperCase()}
      </button>

      {open && (
        <div
          className="absolute right-0 top-full z-50 mt-2 w-64 rounded-lg border border-tremor-border bg-tremor-background-muted p-3 shadow-tremor-dropdown animate-cl-fade-in"
          role="menu"
        >
          <div className="flex items-center gap-2.5 pb-2.5 mb-2 border-b border-tremor-border">
            <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-dark-tremor-background-emphasis text-[12px] font-semibold text-dark-tremor-content-strong">
              {whoami.name.slice(0, 2).toUpperCase()}
            </span>
            <div className="min-w-0">
              <div className="text-sm font-medium text-tremor-content-strong dark:text-dark-tremor-content-strong truncate">
                {whoami.name}
              </div>
              <div className="text-[11px] text-tremor-content-subtle dark:text-dark-tremor-content-subtle truncate">
                {formatVia(whoami.via)}
              </div>
            </div>
          </div>

          {/* Live status — the header-bar strip moved in here so the
              header stays short. Mounts (and polls) only while open. */}
          <div className="mb-3 pb-3 border-b border-tremor-border dark:border-dark-tremor-border">
            <div className="cl-stat-label mb-1.5">Live status</div>
            <HeaderStatus stack onNavigate={close} />
          </div>

          {/* Overflow pages — Storage / API Keys live here instead of
              crowding the header nav. */}
          {overflowLinks.length > 0 && (
            <div className="mb-3 pb-3 border-b border-tremor-border dark:border-dark-tremor-border">
              <div className="cl-stat-label mb-1.5">Manage</div>
              <div className="space-y-0.5">
                {overflowLinks.map((n) => (
                  <NavLink
                    key={n.to}
                    to={n.to}
                    end={n.end}
                    onClick={close}
                    className={menuLinkCls}
                  >
                    {n.label}
                  </NavLink>
                ))}
              </div>
            </div>
          )}

          {/* Scopes — read-only badge row, same chip as before but inside
              the menu instead of crowding the header strip. */}
          <div className="mb-3">
            <div className="cl-stat-label mb-1.5">Scopes</div>
            <div className="flex flex-wrap gap-1">
              {whoami.scopes.map((s) => (
                <span
                  key={s}
                  className="cl-mono text-[10px] px-1.5 py-0.5 rounded border border-tremor-border dark:border-dark-tremor-border text-tremor-content-subtle dark:text-dark-tremor-content-subtle"
                >
                  {s}
                </span>
              ))}
            </div>
          </div>

          <button
            type="button"
            role="menuitem"
            onClick={handleSignOut}
            disabled={busy}
            className="w-full rounded-md border border-tremor-border dark:border-dark-tremor-border px-2.5 py-1.5 text-sm text-tremor-content-subtle dark:text-dark-tremor-content-subtle hover:bg-tremor-background-subtle dark:hover:bg-dark-tremor-background-subtle hover:text-tremor-content-strong dark:hover:text-dark-tremor-content-strong disabled:opacity-50 transition-colors text-left"
          >
            {busy ? "Signing out…" : "Sign out"}
          </button>
        </div>
      )}
    </div>
  );
}

interface NavItem {
  to: string;
  label: string;
  end?: boolean;
}

export default function Layout({ whoami }: LayoutProps) {
  const isAdmin = whoami.scopes.includes("admin");
  const [menuOpen, setMenuOpen] = useState(false);

  // Primary pages stay in the header bar. Secondary pages (Storage,
  // API Keys) live in the account menu so the bar stays short.
  const primaryNav: NavItem[] = [
    { to: "/", label: "Logs", end: true },
    { to: "/connect", label: "Connect" },
    { to: "/errors", label: "Errors" },
    { to: "/dashboards", label: "Dashboards" },
    { to: "/alerts", label: "Alerts" },
    { to: "/web", label: "Web" },
  ];
  const overflowLinks: NavItem[] = [
    { to: "/storage", label: "Storage" },
    { to: "/web/projects", label: "Web projects" },
    ...(isAdmin ? [{ to: "/api-keys", label: "API Keys" } as NavItem] : []),
  ];
  const mobileNav = [...primaryNav, ...overflowLinks];

  async function onLogout() {
    // Server clears the cookie; bounce to /login.
    try {
      await api.logout();
    } finally {
      window.location.href = "/login";
    }
  }

  return (
    <div className="min-h-screen overflow-x-hidden">
      <header className="cl-header border-b border-tremor-border sticky top-0 z-30">
        <div className="max-w-7xl mx-auto px-4 sm:px-6 min-h-14 py-2 flex items-center gap-3 sm:gap-6 min-w-0">
          {/* Mobile menu button */}
          <button
            type="button"
            aria-label="Toggle navigation"
            aria-expanded={menuOpen}
            onClick={() => setMenuOpen((o) => !o)}
            className="md:hidden flex h-9 w-9 shrink-0 items-center justify-center rounded-md border border-tremor-border dark:border-dark-tremor-border text-tremor-content-subtle dark:text-dark-tremor-content-subtle hover:text-tremor-content-strong dark:hover:text-dark-tremor-content-strong"
          >
            {menuOpen ? (
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round">
                <path d="M6 6l12 12M18 6L6 18" />
              </svg>
            ) : (
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round">
                <path d="M4 7h16M4 12h16M4 17h16" />
              </svg>
            )}
          </button>
          {/* Brand */}
          <NavLink to="/" className="flex items-center gap-2.5 group shrink-0 min-w-0">
            <span className="flex h-7 w-7 items-center justify-center rounded-md bg-tremor-brand shadow-[0_0_12px_var(--cl-glow)] shrink-0">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" style={{ stroke: "var(--cl-brand-inverted)" }} strokeWidth="2.5" strokeLinecap="round">
                <path d="M4 17l4.5-5.5 3.5 3 5-7 3 3.5" />
              </svg>
            </span>
            <span className="text-[15px] font-semibold tracking-tight text-tremor-content-strong dark:text-dark-tremor-content-strong truncate">
              central-logs
            </span>
          </NavLink>

          {/* Nav — desktop, primary pages only */}
          <nav className="hidden md:flex gap-1 shrink-0">
            {primaryNav.map((n) => (
              <NavLink
                key={n.to}
                to={n.to}
                end={n.end}
                className={({ isActive }) =>
                  `px-3 py-1.5 rounded-md text-sm font-medium transition-colors ${
                    isActive
                      ? "bg-tremor-brand-faint text-tremor-brand dark:bg-dark-tremor-brand-faint dark:text-dark-tremor-brand"
                      : "text-tremor-content-subtle dark:text-dark-tremor-content-subtle hover:bg-tremor-background-subtle dark:hover:bg-dark-tremor-background-subtle hover:text-tremor-content-emphasis dark:hover:text-dark-tremor-content-emphasis"
                  }`
                }
              >
                {n.label}
              </NavLink>
            ))}
          </nav>

          {/* User + preferences — pushed to the right edge. The avatar
              menu also carries the live status + secondary pages. */}
          <div className="ml-auto flex items-center gap-2 shrink-0">
            <ThemeMenu />
            <UserMenu whoami={whoami} onLogout={onLogout} overflowLinks={overflowLinks} />
          </div>
        </div>

        {/* Nav — mobile dropdown (all pages; live status lives in
            the account menu on every viewport) */}
        {menuOpen && (
          <nav className="md:hidden border-t border-tremor-border dark:border-dark-tremor-border px-4 py-2">
            <div className="grid grid-cols-2 gap-1">
              {mobileNav.map((n) => (
                <NavLink
                  key={n.to}
                  to={n.to}
                  end={n.end}
                  onClick={() => setMenuOpen(false)}
                  className={({ isActive }) =>
                    `px-3 py-2 rounded-md text-sm font-medium transition-colors ${
                      isActive
                        ? "bg-tremor-brand-faint text-tremor-brand dark:bg-dark-tremor-brand-faint dark:text-dark-tremor-brand"
                        : "text-tremor-content-subtle dark:text-dark-tremor-content-subtle hover:bg-tremor-background-subtle dark:hover:bg-dark-tremor-background-subtle hover:text-tremor-content-emphasis dark:hover:text-dark-tremor-content-emphasis"
                    }`
                  }
                >
                  {n.label}
                </NavLink>
              ))}
            </div>
          </nav>
        )}
      </header>
      <main className="max-w-7xl mx-auto px-4 sm:px-6 py-4 sm:py-6 min-w-0 animate-cl-fade-in">
        <Outlet />
      </main>
    </div>
  );
}