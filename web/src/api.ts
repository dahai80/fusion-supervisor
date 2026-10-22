// issue #25: supervisor HTTP face client. Same-origin fetch — standalone dev
// goes through the vite proxy (9100), embedded through the gateway
// /shell/fusion-supervisor/ proxy. Mutations need the supervisor token
// (FUSION_SV_TOKEN): VITE_SV_TOKEN wires it in dev builds only.
const TOKEN: string | undefined = import.meta.env.VITE_SV_TOKEN;

export interface ServiceStatus {
    name: string;
    state: string;
    port: number;
    plane: string;
}

export interface HealthRow {
    ts: number;
    service: string;
    status: string;
    latency_ms: number;
    plane: string;
}

export interface EventRow {
    ts: number;
    service: string;
    kind: string;
    detail: string;
    plane: string;
}

function authHeaders(): Record<string, string> {
    return TOKEN ? { "x-token": TOKEN } : {};
}

async function getJson<T>(path: string): Promise<T> {
    const resp = await fetch(path, { headers: authHeaders() });
    if (!resp.ok) {
        throw new Error(`GET ${path} failed: ${resp.status}`);
    }
    return (await resp.json()) as T;
}

export async function fetchStatus(): Promise<ServiceStatus[]> {
    const v = await getJson<{ ok: boolean; services: ServiceStatus[] }>(
        "/api/v1/status",
    );
    return v.services ?? [];
}

export async function fetchHistory(
    service: string,
    limit = 120,
): Promise<HealthRow[]> {
    const v = await getJson<{ ok: boolean; history: HealthRow[] }>(
        `/api/v1/history?service=${encodeURIComponent(service)}&limit=${limit}`,
    );
    return v.history ?? [];
}

export async function fetchEvents(limit = 100): Promise<EventRow[]> {
    const v = await getJson<{ ok: boolean; events: EventRow[] }>(
        `/api/v1/events?limit=${limit}`,
    );
    return v.events ?? [];
}

export type MutationMethod =
    | "up"
    | "down"
    | "restart"
    | "drain"
    | "rollout"
    | "backup";

// Mutations reuse the UDS dispatch over HTTP — errors carry the RPC envelope
// ({"error":{"message":...}}) which we surface verbatim.
export async function mutate(
    method: MutationMethod,
    service?: string,
): Promise<void> {
    const resp = await fetch(`/api/v1/${method}`, {
        method: "POST",
        headers: { "Content-Type": "application/json", ...authHeaders() },
        body: JSON.stringify(service ? { service } : {}),
    });
    const body = (await resp.json().catch(() => null)) as {
        error?: { message?: string };
    } | null;
    if (!resp.ok || body?.error) {
        throw new Error(
            body?.error?.message ?? `${method} failed: ${resp.status}`,
        );
    }
}
