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
import { type BootEntry, bootGroup, bootPin, fetchBoot, mutate } from "../api";
import { useT } from "../i18n";

// issue #99: ops surface — backup trigger, boot-group up/down, boot-pin editor.
// Backend routes already exist (GET /api/v1/boot, POST boot/group, boot/pin,
// POST /api/v1/backup); this page only wires them into the console.

type PinMode = "auto" | "manual" | "clear";

interface GroupAction {
    group: string;
    action: "up" | "down";
}

interface PendingPin {
    service: string;
    mode: PinMode;
}

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

const GROUPS: ReadonlyArray<{ id: string; zh: string; en: string }> = [
    { id: "T0", zh: "核心层 T0", en: "Core T0" },
    { id: "T1", zh: "平台层 T1", en: "Platform T1" },
    { id: "T2", zh: "按需层 T2", en: "On-demand T2" },
    { id: "pinned", zh: "置顶服务", en: "Pinned" },
];

export function OpsPage() {
    const t = useT();
    const [boot, setBoot] = useState<BootEntry[] | null>(null);
    const [error, setError] = useState<string | null>(null);
    const [notice, setNotice] = useState<string | null>(null);
    const [pendingGroup, setPendingGroup] = useState<GroupAction | null>(null);
    const [pendingPin, setPendingPin] = useState<PendingPin | null>(null);
    const [backupConfirm, setBackupConfirm] = useState(false);
    const [busy, setBusy] = useState(false);

    const load = useCallback(async () => {
        try {
            setBoot(await fetchBoot());
            setError(null);
        } catch (e) {
            console.error("[supervisor-web] boot load fail:", e);
            setError(e instanceof Error ? e.message : String(e));
        }
    }, []);

    useEffect(() => {
        load();
        const t = setInterval(load, 10000);
        return () => clearInterval(t);
    }, [load]);

    const runGroup = async (g: GroupAction) => {
        setBusy(true);
        try {
            await bootGroup(g.group, g.action);
            setNotice(
                t(
                    `已对 ${g.group} 执行 ${g.action === "up" ? "启动" : "停止"}`,
                    `${g.action} on ${g.group} done`,
                ),
            );
        } catch (e) {
            console.error("[supervisor-web] boot group fail:", e);
            setError(e instanceof Error ? e.message : String(e));
        }
        setBusy(false);
        setPendingGroup(null);
        load();
    };

    const runPin = async (p: PendingPin) => {
        setBusy(true);
        try {
            await bootPin(p.service, p.mode);
            setNotice(
                t(
                    `${p.service} 置顶策略已更新为 ${p.mode}`,
                    `${p.service} pin updated to ${p.mode}`,
                ),
            );
        } catch (e) {
            console.error("[supervisor-web] boot pin fail:", e);
            setError(e instanceof Error ? e.message : String(e));
        }
        setBusy(false);
        setPendingPin(null);
        load();
    };

    const runBackup = async () => {
        setBusy(true);
        try {
            await mutate("backup");
            setNotice(t("备份完成", "Backup done"));
        } catch (e) {
            console.error("[supervisor-web] backup fail:", e);
            setError(e instanceof Error ? e.message : String(e));
        }
        setBusy(false);
        setBackupConfirm(false);
    };

    if (error && !boot) {
        return (
            <ErrorState
                message={error}
                retryLabel={t("重试", "Retry")}
                onRetry={() => void load()}
            />
        );
    }
    if (!boot) {
        return <SkeletonLines rows={6} />;
    }

    const pinned = boot.filter((b) => b.pinned).length;

    const columns: DataTableColumn<BootEntry>[] = [
        {
            key: "name",
            header: t("服务", "Service"),
            render: (r) => (
                <span className="font-mono text-sm text-text-primary">
                    {r.name}
                </span>
            ),
        },
        { key: "tier", header: t("层级", "Tier") },
        {
            key: "pinned",
            header: t("置顶", "Pin"),
            render: (r) => r.pinned ?? "—",
        },
        {
            key: "state",
            header: t("状态", "State"),
            render: (r) => (
                <StatusBadge status={stateToKind(r.state)} label={r.state} />
            ),
        },
        {
            key: "pinActions",
            header: t("置顶操作", "Pin actions"),
            render: (r) => (
                <div className="flex gap-1">
                    <OpBtn
                        label={t("常驻", "auto")}
                        disabled={busy || r.pinned === "auto"}
                        onClick={() =>
                            setPendingPin({ service: r.name, mode: "auto" })
                        }
                    />
                    <OpBtn
                        label={t("手动", "manual")}
                        disabled={busy || r.pinned === "manual"}
                        onClick={() =>
                            setPendingPin({ service: r.name, mode: "manual" })
                        }
                    />
                    <OpBtn
                        label={t("清除", "clear")}
                        disabled={busy || !r.pinned}
                        onClick={() =>
                            setPendingPin({ service: r.name, mode: "clear" })
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
                    {t("运维", "Ops")}
                </h1>
                <OpBtn
                    label={t("立即备份", "Backup now")}
                    disabled={busy}
                    onClick={() => setBackupConfirm(true)}
                />
            </div>
            {error ? (
                <div className="mb-2 rounded-md border border-error/40 bg-error/10 px-3 py-2 text-xs text-error">
                    {error}
                </div>
            ) : null}
            {notice ? (
                <div className="mb-2 rounded-md border border-brand/40 bg-brand/10 px-3 py-2 text-xs text-brand">
                    {notice}
                </div>
            ) : null}

            <h2 className="mb-2 mt-4 text-sm text-text-secondary">
                {t("分级启停", "Boot groups")}
            </h2>
            <div className="mb-4 grid grid-cols-1 gap-2 sm:grid-cols-2">
                {GROUPS.map((g) => {
                    const count =
                        g.id === "pinned"
                            ? pinned
                            : boot.filter((b) => b.tier === g.id).length;
                    return (
                        <div
                            key={g.id}
                            className="flex items-center justify-between rounded-xl border border-border bg-bg-elevated px-3 py-2 shadow-sm"
                        >
                            <div>
                                <div className="text-sm text-text-primary">
                                    {t(g.zh, g.en)}
                                </div>
                                <div className="text-xs text-text-muted">
                                    {t(`${count} 个服务`, `${count} services`)}
                                </div>
                            </div>
                            <div className="flex gap-1">
                                <OpBtn
                                    label={t("启动", "Up")}
                                    disabled={busy}
                                    onClick={() =>
                                        setPendingGroup({
                                            group: g.id,
                                            action: "up",
                                        })
                                    }
                                />
                                <OpBtn
                                    label={t("停止", "Down")}
                                    danger
                                    disabled={busy}
                                    onClick={() =>
                                        setPendingGroup({
                                            group: g.id,
                                            action: "down",
                                        })
                                    }
                                />
                            </div>
                        </div>
                    );
                })}
            </div>

            <h2 className="mb-2 text-sm text-text-secondary">
                {t("置顶排序", "Boot pins")}
            </h2>
            <DataTable
                columns={columns}
                rows={boot}
                rowKey={(r) => r.name}
                pageSize={30}
            />

            {/* group up is safe (probe-skip-healthy); group down + pin changes need confirm */}
            <ConfirmDialog
                open={pendingGroup !== null}
                danger={pendingGroup?.action === "down"}
                title={
                    pendingGroup
                        ? t(
                              `确认对 ${pendingGroup.group} 执行${pendingGroup.action === "up" ? "启动" : "停止"}？`,
                              `Confirm ${pendingGroup.action} on ${pendingGroup.group}?`,
                          )
                        : ""
                }
                description={t(
                    pendingGroup?.action === "down"
                        ? "停止波会停掉该分级内所有受管服务，需手动或按分级重新拉起。"
                        : "启动波只拉起未健康的服务（探活跳过已健康）。",
                    pendingGroup?.action === "down"
                        ? "The stop wave stops all managed services in this tier; restart manually or by tier."
                        : "The start wave only starts services that are not healthy (healthy ones are skipped).",
                )}
                confirmLabel={t("确认", "Confirm")}
                cancelLabel={t("取消", "Cancel")}
                onConfirm={() => pendingGroup && runGroup(pendingGroup)}
                onCancel={() => setPendingGroup(null)}
            />
            <ConfirmDialog
                open={backupConfirm}
                danger={false}
                title={t("确认立即备份？", "Run backup now?")}
                description={t(
                    "对 Postgres 与 Qdrant 执行一次性备份到 FUSION_BACKUP_TARGET，并按保留策略裁剪旧备份。",
                    "Runs a one-shot Postgres + Qdrant backup to FUSION_BACKUP_TARGET, then prunes old backups per retention.",
                )}
                confirmLabel={t("确认", "Confirm")}
                cancelLabel={t("取消", "Cancel")}
                onConfirm={runBackup}
                onCancel={() => setBackupConfirm(false)}
            />
            <ConfirmDialog
                open={pendingPin !== null}
                danger={false}
                title={
                    pendingPin
                        ? t(
                              `确认将 ${pendingPin.service} 置顶策略改为 ${pendingPin.mode}？`,
                              `Confirm ${pendingPin.service} pin -> ${pendingPin.mode}?`,
                          )
                        : ""
                }
                description={t(
                    "置顶决定该服务随哪个分级启停（写入 history.db，重启不丢）。",
                    "Pins decide which tier boot wave covers the service (persisted in history.db).",
                )}
                confirmLabel={t("确认", "Confirm")}
                cancelLabel={t("取消", "Cancel")}
                onConfirm={() => pendingPin && runPin(pendingPin)}
                onCancel={() => setPendingPin(null)}
            />
        </div>
    );
}

function OpBtn({
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
