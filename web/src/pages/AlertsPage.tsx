import {
    DataTable,
    type DataTableColumn,
    ErrorState,
    SkeletonLines,
    StatusBadge,
    type StatusKind,
} from "@fusion/web-ui";
import { formatRelativeTime } from "@fusion/web-ui/format";
import { useCallback, useEffect, useState } from "react";
import { type EventRow, fetchEvents } from "../api";

// issue #25: global event stream (/api/v1/events) — crash/restart/drain/
// rollout/backup events across services, time-ascending from the store.
function kindToBadgeKind(kind: string): StatusKind {
    if (
        kind.includes("Fail") ||
        kind.includes("Crash") ||
        kind === "Unhealthy"
    ) {
        return "error";
    }
    if (kind.includes("Alert") || kind.includes("Backoff")) {
        return "warning";
    }
    if (kind.includes("Start") || kind.includes("Rollout")) {
        return "running";
    }
    return "success";
}

export function AlertsPage() {
    const [events, setEvents] = useState<EventRow[] | null>(null);
    const [error, setError] = useState<string | null>(null);

    const load = useCallback(async () => {
        try {
            setEvents(await fetchEvents(200));
            setError(null);
        } catch (e) {
            console.error("[supervisor-web] events load fail:", e);
            setError(e instanceof Error ? e.message : String(e));
        }
    }, []);

    useEffect(() => {
        load();
        const t = setInterval(load, 10_000);
        return () => clearInterval(t);
    }, [load]);

    if (error && !events) {
        return <ErrorState message={error} retryLabel="Retry" />;
    }
    if (!events) {
        return <SkeletonLines rows={6} />;
    }

    const columns: DataTableColumn<EventRow>[] = [
        {
            key: "ts",
            header: "Time",
            render: (r) => (
                <span className="text-xs text-text-muted">
                    {formatRelativeTime(r.ts * 1000)}
                </span>
            ),
        },
        {
            key: "service",
            header: "Service",
            render: (r) => (
                <span className="font-mono text-sm text-text-primary">
                    {r.service}
                </span>
            ),
        },
        {
            key: "kind",
            header: "Event",
            render: (r) => (
                <StatusBadge status={kindToBadgeKind(r.kind)} label={r.kind} />
            ),
        },
        {
            key: "detail",
            header: "Detail",
            render: (r) => (
                <span className="text-xs text-text-secondary">{r.detail}</span>
            ),
        },
    ];

    return (
        <div className="mx-auto max-w-4xl px-4">
            <div className="mb-3 flex items-center justify-between">
                <h1 className="text-base text-text-primary">Alerts</h1>
                <span className="text-xs text-text-muted">
                    last {events.length} events · 10s poll
                </span>
            </div>
            <DataTable
                columns={columns}
                rows={[...events].reverse()}
                rowKey={(r) => `${r.ts}-${r.service}-${r.kind}`}
                pageSize={20}
                emptyState={
                    <div className="py-8 text-center text-xs text-text-muted">
                        No events recorded.
                    </div>
                }
            />
        </div>
    );
}
