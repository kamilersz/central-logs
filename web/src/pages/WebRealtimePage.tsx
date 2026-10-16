import { usePoll } from "../components";
import { api, type WebRealtimeResponse, type WebSitesResponse } from "../api";

/** Realtime web traffic (last 5 minutes), refreshed every 10s. */
export default function WebRealtimePage() {
  const { data: sites } = usePoll<WebSitesResponse>(() => api.webSites(), 60000, []);
  const siteOptions = (sites?.sites ?? []).map((s) => s.id);
  const { data: rt } = usePoll<WebRealtimeResponse | undefined>(
    () => api.webRealtime(siteOptions[0]),
    10000,
    [siteOptions.join(",")],
  );

  return (
    <div>
      <h2 className="text-xl font-semibold mb-4">Web Realtime</h2>
      {siteOptions.length > 0 && (
        <div className="text-sm opacity-70 mb-4">site: {siteOptions[0]}</div>
      )}
      <div className="grid grid-cols-1 sm:grid-cols-3 gap-4 mb-4">
        <div className="p-4 cl-card">
          <div className="cl-stat-label mb-1.5">visitors (5 min)</div>
          <div className="text-3xl font-semibold tabular-nums">
            {rt?.visitors_5m ?? "—"}
          </div>
        </div>
        <div className="p-4 cl-card col-span-2">
          <div className="cl-stat-label mb-1.5">active pages</div>
          {(rt?.pages?.length ?? 0) === 0 ? (
            <div className="text-sm opacity-50">no pageviews in the last 5 minutes</div>
          ) : (
            <table className="w-full text-sm tabular-nums">
              <tbody>
                {(rt?.pages ?? []).map((p) => (
                  <tr key={p.path} className="border-t border-opacity-10">
                    <td className="py-1 pr-3 truncate max-w-[24rem]" title={p.path}>
                      {p.path}
                    </td>
                    <td className="py-1 text-right">{p.hits}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </div>
      <p className="text-xs opacity-60">
        Counts come straight from the hot table — bots excluded, no cookies or
        stored IPs involved.
      </p>
    </div>
  );
}
