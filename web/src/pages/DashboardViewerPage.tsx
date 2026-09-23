import { useState } from "react";
import { useParams, Link } from "react-router-dom";
import {
  usePoll,
  TimeFilterBar,
  defaultTimeFilter,
  type TimeFilterState,
} from "../components";
import { PanelView, type PeriodOverride } from "../Panel";
import { api, type DashboardConfig, type Panel } from "../api";

export default function DashboardViewerPage() {
  const { id } = useParams<{ id: string }>();
  const { data: dash, error } = usePoll<DashboardConfig>(
    () => api.getDashboard(Number(id)),
    30000,
  );

  // View-time override for every panel's period ("7d → 30d" without editing
  // the dashboard). "" = panel defaults; the saved definition is never
  // written from this page.
  const [tf, setTf] = useState<TimeFilterState>(() => ({
    ...defaultTimeFilter(),
    window: "",
  }));

  const override: PeriodOverride | undefined =
    tf.window === ""
      ? undefined
      : tf.window === "custom"
        ? tf.from &&
          tf.to &&
          !Number.isNaN(Date.parse(tf.from)) &&
          !Number.isNaN(Date.parse(tf.to)) &&
          Date.parse(tf.to) > Date.parse(tf.from)
          ? {
              from: new Date(tf.from).toISOString(),
              to: new Date(tf.to).toISOString(),
            }
          : undefined
        : { window: tf.window };

  if (error) return <div className="text-red-600 text-sm">{error}</div>;
  if (!dash) return <div className="text-tremor-content-subtle">Loading...</div>;

  const panels = Array.isArray(dash.panels) ? (dash.panels as Panel[]) : [];

  return (
    <div>
      <div className="flex justify-between items-center mb-6">
        <div>
          <h2 className="text-xl font-semibold">{dash.name}</h2>
          {dash.description && (
            <p className="text-sm text-tremor-content-subtle">{dash.description}</p>
          )}
        </div>
        <Link
          to={`/dashboards/edit/${dash.id}`}
          className="px-3 py-1.5 rounded-md text-sm border border-tremor-border dark:border-dark-tremor-border"
        >
          Edit
        </Link>
      </div>
      {/* Dynamic time range: applies to all panels at view time. */}
      <TimeFilterBar value={tf} onChange={setTf} showFilter={false} defaultOption="panel defaults" />
      <div className="grid grid-cols-12 gap-4">
        {panels.map((p, i) => (
          <PanelView key={i} panel={p} periodOverride={override} />
        ))}
        {panels.length === 0 && (
          <div className="text-sm text-tremor-content-subtle col-span-12">
            This dashboard has no panels.
          </div>
        )}
      </div>
    </div>
  );
}
