import { useParams, Link } from "react-router-dom";
import { usePoll } from "../components";
import { PanelView } from "../Panel";
import { api, type DashboardConfig, type Panel } from "../api";

export default function DashboardViewerPage() {
  const { id } = useParams<{ id: string }>();
  const { data: dash, error } = usePoll<DashboardConfig>(
    () => api.getDashboard(Number(id)),
    30000,
  );

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
          className="px-3 py-1.5 rounded text-sm border border-tremor-border dark:border-dark-tremor-border"
        >
          Edit
        </Link>
      </div>
      <div className="grid grid-cols-12 gap-4">
        {panels.map((p, i) => (
          <PanelView key={i} panel={p} />
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
