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
import { useT } from "../i18n";

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

// issue #95: 操作名双语映射 (确认弹窗文案用)。
const OP_NAMES: Record<MutationMethod, { zh: string; en: string }> = {
    up: { zh: "启动", en: "up" },
    down: { zh: "停止", en: "down" },
    restart: { zh: "重启", en: "restart" },
    drain: { zh: "排水", en: "drain" },
    rollout: { zh: "滚动发布", en: "rollout" },
    backup: { zh: "备份", en: "backup" },
};

export function ServicesPage() {
    const t = useT();
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
        return (
            <ErrorState
                message={error}
                retryLabel={t("重试", "Retry")}
                onRetry={() => void load()}
            />
        );
    }
    if (!services) {
        return <SkeletonLines rows={6} />;
    }

    const columns: DataTableColumn<ServiceStatus>[] = [
        {
            key: "name",
            header: t("服务", "Service"),
            render: (r) => (
                <span className="font-mono text-sm text-text-primary">
                    {r.name}
                </span>
            ),
        },
        {
            key: "state",
            header: t("状态", "State"),
            render: (r) => (
                <StatusBadge status={stateToKind(r.state)} label={r.state} />
            ),
        },
        {
            key: "port",
            header: t("端口", "Port"),
            sortable: true,
            align: "right",
        },
        { key: "plane", header: t("平面", "Plane") },
        {
            key: "actions",
            header: t("操作", "Actions"),
            render: (r) => (
                <div className="flex gap-1">
                    <ActionBtn
                        label={t("启动", "Up")}
                        disabled={busy}
                        onClick={() => setPending({ method: "up" })}
                    />
                    <ActionBtn
                        label={t("重启", "Restart")}
                        disabled={busy}
                        onClick={() =>
                            setPending({ method: "restart", service: r.name })
                        }
                    />
                    <ActionBtn
                        label={t("排水", "Drain")}
                        disabled={busy}
                        onClick={() =>
                            setPending({ method: "drain", service: r.name })
                        }
                    />
                    <ActionBtn
                        label={t("发布", "Rollout")}
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
                <h1 className="text-base text-text-primary">
                    {t("服务", "Services")}
                </h1>
                <div className="flex items-center gap-2">
                    <span className="text-xs text-text-muted">
                        {t(
                            `${services.length} 个服务 · 5s 轮询`,
                            `${services.length} services · 5s poll`,
                        )}
                    </span>
                    <ActionBtn
                        label={t("全部启动", "Up all")}
                        disabled={busy}
                        onClick={() => setPending({ method: "up" })}
                    />
                    <ActionBtn
                        label={t("全部停止", "Down all")}
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
                        ? t(
                              `确认${OP_NAMES[pending.method].zh}${pending.service ? ` ${pending.service}` : "全部服务"}？`,
                              `Confirm ${pending.method} ${pending.service ?? "all services"}?`,
                          )
                        : ""
                }
                description={t(
                    "supervisor 经与 fusion-sv CLI 相同的 RPC 路径执行此操作。",
                    "The supervisor executes this through the same RPC path as the fusion-sv CLI.",
                )}
                confirmLabel={t("确认", "Confirm")}
                cancelLabel={t("取消", "Cancel")}
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
