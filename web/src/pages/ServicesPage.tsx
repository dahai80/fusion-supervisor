import {
    ConfirmDialog,
    DataTable,
    type DataTableColumn,
    ErrorState,
    SkeletonLines,
    StatusBadge,
    type StatusKind,
} from "@fusion/web-ui";
import { useCallback, useEffect, useState } from "react";
import {
    type MutationMethod,
    type ServiceStatus,
    fetchStatus,
    mutate,
} from "../api";

// state string from render_status_json → StatusBadge kind (spec §3.1).
function stateToKind(state: string): StatusKind {
    switch (state) {
        case "Healthy":
            return "success";
        case "Starting":
        case "Draining":
            return "running";
        case "Stopping":
        case "Backoff":
            return "warning";
        case "Crashed":
        case "Unhealthy":
            return "error";
        default:
            return "offline";
    }
}

// danger ops need explicit confirm (spec S5); up is safe (idempotent start).
const DANGER_OPS: ReadonlySet<MutationMethod> = new Set([
    "down",
    "restart",
    "drain",
    "rollout",
]);

interface PendingOp {
    method: MutationMethod;
    service?: string;
}

export function ServicesPage() {
    const [services, setServices] = useState<ServiceStatus[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [pending, setPending] = useState<PendingOp | null>(null);
    const [busy, setBusy] = useState(false);

    const load = useCallback(async () => {
        try {
            setServices(await fetchStatus());
            setError(null);
        } catch (e) {
            console.error("[supervisor-web] status load fail:", e);
            setError(e instanceof Error ? e.message : String(e));
        }
    }, []);

    useEffect(() => {
        load();
        // 5s poll — status is the console landing view.
        const t = setInterval(load, 5000);
        return () => clearInterval(t);
    }, [load]);

    const runOp = async (op: PendingOp) => {
        setBusy(true);
        try {
            await mutate(op.method, op.service);
        } catch (e) {
            console.error("[supervisor-web] mutation fail:", e);
            setError(e instanceof Error ? e.message : String(e));
        }
        setBusy(false);
        setPending(null);
        load();
    };

    if (error && !services) {
        return <ErrorState message={error} retryLabel="Retry" />;
    }
    if (!services) {
        return <SkeletonLines rows={6} />;
    }

    const columns: DataTableColumn<ServiceStatus>[] = [
        {
            key: "name",
            header: "Service",
            render: (r) => (
                <span className="font-mono text-sm text-text-primary">
                    {r.name}
                </span>
            ),
        },
        {
            key: "state",
            header: "State",
            render: (r) => (
                <StatusBadge status={stateToKind(r.state)} label={r.state} />
            ),
        },
        { key: "port", header: "Port", sortable: true, align: "right" },
        { key: "plane", header: "Plane" },
        {
            key: "actions",
            header: "Actions",
            render: (r) => (
                <div className="flex gap-1">
                    <ActionBtn
                        label="Up"
                        disabled={busy}
                        onClick={() => setPending({ method: "up" })}
                    />
                    <ActionBtn
                        label="Restart"
                        disabled={busy}
                        onClick={() =>
                            setPending({ method: "restart", service: r.name })
                        }
                    />
                    <ActionBtn
                        label="Drain"
                        disabled={busy}
                        onClick={() =>
                            setPending({ method: "drain", service: r.name })
                        }
                    />
                    <ActionBtn
                        label="Rollout"
                        disabled={busy}
                        onClick={() =>
                            setPending({ method: "rollout", service: r.name })
                        }
                    />
                </div>
            ),
        },
    ];

    return (
        <div className="mx-auto max-w-4xl px-4">
            <div className="mb-3 flex items-center justify-between">
                <h1 className="text-base text-text-primary">Services</h1>
                <div className="flex items-center gap-2">
                    <span className="text-xs text-text-muted">
                        {services.length} services · 5s poll
                    </span>
                    <ActionBtn
                        label="Up all"
                        disabled={busy}
                        onClick={() => setPending({ method: "up" })}
                    />
                    <ActionBtn
                        label="Down all"
                        danger
                        disabled={busy}
                        onClick={() => setPending({ method: "down" })}
                    />
                </div>
            </div>
            {error ? (
                <div className="mb-2 rounded-md border border-error/40 bg-error/10 px-3 py-2 text-xs text-error">
                    {error}
                </div>
            ) : null}
            <DataTable
                columns={columns}
                rows={services}
                rowKey={(r) => r.name}
                pageSize={30}
            />
            <ConfirmDialog
                open={pending !== null}
                danger={pending ? DANGER_OPS.has(pending.method) : false}
                title={
                    pending
                        ? `${pending.method}${pending.service ? ` ${pending.service}` : " all"}?`
                        : ""
                }
                description="The supervisor executes this through the same RPC path as the fusion-sv CLI."
                confirmLabel="Confirm"
                cancelLabel="Cancel"
                onConfirm={() => pending && runOp(pending)}
                onCancel={() => setPending(null)}
            />
        </div>
    );
}

function ActionBtn({
    label,
    onClick,
    disabled,
    danger,
}: {
    label: string;
    onClick: () => void;
    disabled?: boolean;
    danger?: boolean;
}) {
    return (
        <button
            type="button"
            disabled={disabled}
            onClick={onClick}
            className={`rounded-md border px-2 py-1 text-xs disabled:cursor-not-allowed disabled:opacity-40 ${
                danger
                    ? "border-error/50 text-error hover:bg-error/10"
                    : "border-border text-text-secondary hover:bg-bg-muted"
            }`}
        >
            {label}
        </button>
    );
}
