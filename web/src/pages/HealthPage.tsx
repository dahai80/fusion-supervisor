import {
    ErrorState,
    MetricChart,
    type MetricPoint,
    SkeletonLines,
} from "@fusion/web-ui";
import { useCallback, useEffect, useState } from "react";
import { type HealthRow, fetchHistory, fetchStatus } from "../api";

// issue #25: per-service health history — latency line chart from
// /api/v1/history (store recent_health).
export function HealthPage() {
    const [names, setNames] = useState<string[]>([]);
    const [selected, setSelected] = useState<string>("");
    const [rows, setRows] = useState<HealthRow[] | null>(null);
    const [error, setError] = useState<string | null>(null);

    useEffect(() => {
        fetchStatus()
            .then((svcs) => {
                const list = svcs.map((s) => s.name);
                setNames(list);
                setSelected((prev) => prev || list[0] || "");
            })
            .catch((e) => {
                console.error("[supervisor-web] service list fail:", e);
                setError(e instanceof Error ? e.message : String(e));
            });
    }, []);

    const load = useCallback(async (svc: string) => {
        if (!svc) return;
        try {
            setRows(await fetchHistory(svc));
            setError(null);
        } catch (e) {
            console.error("[supervisor-web] history load fail:", e);
            setError(e instanceof Error ? e.message : String(e));
        }
    }, []);

    useEffect(() => {
        setRows(null);
        load(selected);
        const t = setInterval(() => load(selected), 10_000);
        return () => clearInterval(t);
    }, [selected, load]);

    const points: MetricPoint[] = (rows ?? []).map((r) => ({
        t: r.ts * 1000,
        v: r.latency_ms,
    }));
    const latest = rows && rows.length > 0 ? rows[rows.length - 1] : null;
    const downCount = (rows ?? []).filter((r) => r.status !== "Healthy").length;

    if (error && !rows) {
        return <ErrorState message={error} retryLabel="Retry" />;
    }

    return (
        <div className="mx-auto max-w-4xl px-4">
            <div className="mb-3 flex items-center justify-between">
                <h1 className="text-base text-text-primary">Health</h1>
                <select
                    value={selected}
                    onChange={(e) => setSelected(e.target.value)}
                    className="rounded-md border border-border bg-bg px-2 py-1 text-sm text-text-primary"
                >
                    {names.map((n) => (
                        <option key={n} value={n}>
                            {n}
                        </option>
                    ))}
                </select>
            </div>
            {!rows ? (
                <SkeletonLines rows={4} />
            ) : (
                <div className="flex flex-col gap-3">
                    <MetricChart
                        series={[{ label: `${selected} latency (ms)`, points }]}
                        unit="ms"
                    />
                    <div className="grid grid-cols-3 gap-2 text-xs text-text-secondary">
                        <div className="rounded-card border border-border bg-bg-elevated p-2">
                            <div className="text-text-muted">Samples</div>
                            <div className="font-mono tabular-nums text-text-primary">
                                {rows.length}
                            </div>
                        </div>
                        <div className="rounded-card border border-border bg-bg-elevated p-2">
                            <div className="text-text-muted">Last status</div>
                            <div className="font-mono text-text-primary">
                                {latest?.status ?? "-"}
                            </div>
                        </div>
                        <div className="rounded-card border border-border bg-bg-elevated p-2">
                            <div className="text-text-muted">Non-healthy</div>
                            <div
                                className={`font-mono tabular-nums ${
                                    downCount > 0
                                        ? "text-error"
                                        : "text-success"
                                }`}
                            >
                                {downCount}
                            </div>
                        </div>
                    </div>
                </div>
            )}
        </div>
    );
}
