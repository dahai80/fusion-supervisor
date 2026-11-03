import {
    ErrorState,
    MetricChart,
    type MetricPoint,
    MetricTile,
    SelectInput,
    SkeletonLines,
} from "@fusion/web-ui";
import { useCallback, useEffect, useState } from "react";
import { type HealthRow, fetchHistory, fetchStatus } from "../api";
import { useT } from "../i18n";

// issue #25: per-service health history — latency line chart from
// /api/v1/history (store recent_health).
export function HealthPage() {
    const t = useT();
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
        return (
            <ErrorState
                message={error}
                retryLabel={t("重试", "Retry")}
                onRetry={() => void load(selected)}
            />
        );
    }

    return (
        <div className="mx-auto max-w-4xl px-4">
            <div className="mb-3 flex items-center justify-between">
                <h1 className="text-base text-text-primary">
                    {t("健康", "Health")}
                </h1>
                <SelectInput
                    value={selected}
                    onChange={setSelected}
                    options={names.map((n) => ({ value: n, label: n }))}
                />
            </div>
            {!rows ? (
                <SkeletonLines rows={4} />
            ) : (
                <div className="flex flex-col gap-3">
                    <MetricChart
                        series={[
                            {
                                label: t(
                                    `${selected} 延迟 (ms)`,
                                    `${selected} latency (ms)`,
                                ),
                                points,
                            },
                        ]}
                        unit="ms"
                    />
                    <div className="grid grid-cols-3 gap-2">
                        <MetricTile
                            label={t("样本数", "Samples")}
                            value={rows.length}
                        />
                        <MetricTile
                            label={t("最新状态", "Last status")}
                            value={latest?.status ?? "-"}
                        />
                        <MetricTile
                            label={t("非健康样本", "Non-healthy")}
                            value={downCount}
                        />
                    </div>
                </div>
            )}
        </div>
    );
}
