import { NavLink, Outlet, useNavigate } from "react-router-dom";
import { useState } from "react";
import { api, WhoamiResponse } from "./api";
import ThemeMenu from "./ThemeMenu";

interface LayoutProps {
  whoami: WhoamiResponse;
}

export default function Layout({ whoami }: LayoutProps) {
  const navigate = useNavigate();
  const [busy, setBusy] = useState(false);
  const isAdmin = whoami.scopes.includes("admin");

  const navItems = [
    { to: "/", label: "Logs", end: true },
    { to: "/errors", label: "Errors" },
    { to: "/dashboards", label: "Dashboards" },
    { to: "/alerts", label: "Alerts" },
    { to: "/pipeline", label: "Pipeline" },
    { to: "/storage", label: "Storage" },
    ...(isAdmin ? [{ to: "/api-keys", label: "API Keys" }] : []),
  ];

  async function onLogout() {
    setBusy(true);
    try {
      await api.logout();
    } finally {
      // Server clears the cookie; bounce to /login.
      window.location.href = "/login";
    }
  }

  return (
    <div className="min-h-screen">
      <header className="cl-header border-b border-tremor-border sticky top-0 z-10">
        <div className="max-w-7xl mx-auto px-6 h-14 flex items-center gap-8">
          {/* Brand */}
          <NavLink to="/" className="flex items-center gap-2.5 group">
            <span className="flex h-7 w-7 items-center justify-center rounded-md bg-tremor-brand shadow-[0_0_12px_var(--cl-glow)]">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" style={{ stroke: "var(--cl-brand-inverted)" }} strokeWidth="2.5" strokeLinecap="round">
                <path d="M4 17l4.5-5.5 3.5 3 5-7 3 3.5" />
              </svg>
            </span>
            <span className="text-[15px] font-semibold tracking-tight text-tremor-content-strong dark:text-dark-tremor-content-strong">
              central-logs
            </span>
          </NavLink>

          {/* Nav */}
          <nav className="flex gap-1">
            {navItems.map((n) => (
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

          {/* User + preferences */}
          <div className="ml-auto flex items-center gap-3 text-sm">
            <ThemeMenu />
            <span className="flex items-center gap-2 text-tremor-content-emphasis">
              <span className="flex h-6 w-6 items-center justify-center rounded-full bg-dark-tremor-background-emphasis text-[11px] font-semibold text-dark-tremor-content-strong">
                {whoami.name.slice(0, 2).toUpperCase()}
              </span>
              <span>{whoami.name}</span>
              <span className="cl-mono text-[10px] px-1.5 py-0.5 rounded border border-dark-tremor-border text-dark-tremor-content-subtle">
                {whoami.scopes.join("·")}
              </span>
            </span>
            <button
              type="button"
              onClick={onLogout}
              disabled={busy}
              className="px-2.5 py-1 rounded-md text-sm border border-tremor-border dark:border-dark-tremor-border text-tremor-content-subtle dark:text-dark-tremor-content-subtle hover:bg-tremor-background-subtle dark:hover:bg-dark-tremor-background-subtle hover:text-tremor-content-strong dark:hover:text-dark-tremor-content-strong disabled:opacity-50 transition-colors"
            >
              {busy ? "…" : "Sign out"}
            </button>
          </div>
        </div>
      </header>
      <main className="max-w-7xl mx-auto px-6 py-6 animate-cl-fade-in">
        <Outlet />
      </main>
    </div>
  );
}
