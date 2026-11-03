import { useEffect, useState } from "react";
import { useT } from "./i18n";
import { AlertsPage } from "./pages/AlertsPage";
import { HealthPage } from "./pages/HealthPage";
import { ServicesPage } from "./pages/ServicesPage";
import { embeddedParam, isEmbedded } from "./sdk";

// Hash router (no router dep). Standalone shows a bottom tab bar; embedded
// (?embedded=1 or iframe) the shell owns the chrome.
type Route = "services" | "health" | "alerts";

const ROUTES: ReadonlySet<string> = new Set<Route>([
    "services",
    "health",
    "alerts",
]);

function parseHash(): Route {
    const h = window.location.hash.replace(/^#\/?/, "");
    if (ROUTES.has(h)) return h as Route;
    return "services";
}

export function App() {
    const [route, setRoute] = useState<Route>(parseHash());

    useEffect(() => {
        const onHash = () => setRoute(parseHash());
        window.addEventListener("hashchange", onHash);
        return () => window.removeEventListener("hashchange", onHash);
    }, []);

    const embedded = embeddedParam() || isEmbedded();

    return (
        <div className={embedded ? "h-full" : "flex h-full flex-col"}>
            {embedded && <EmbeddedTabs route={route} onChange={setRoute} />}
            <main className={`min-h-0 flex-1 ${embedded ? "" : "pb-16 pt-4"}`}>
                {route === "services" && <ServicesPage />}
                {route === "health" && <HealthPage />}
                {route === "alerts" && <AlertsPage />}
            </main>
            {!embedded && <BottomNav route={route} onChange={setRoute} />}
        </div>
    );
}

// issue #95: 双语导航 (zh 优先 — shell 默认语言)。
const NAV_ITEMS: Array<{ id: Route; zh: string; en: string }> = [
    { id: "services", zh: "服务", en: "Services" },
    { id: "health", zh: "健康", en: "Health" },
    { id: "alerts", zh: "告警", en: "Alerts" },
];

function EmbeddedTabs({
    route,
    onChange,
}: { route: Route; onChange: (r: Route) => void }) {
    const t = useT();
    return (
        <nav className="flex shrink-0 border-b border-border bg-bg-elevated">
            {NAV_ITEMS.map((it) => (
                <button
                    key={it.id}
                    type="button"
                    onClick={() => onChange(it.id)}
                    className={`flex-1 py-2 text-center text-xs uppercase tracking-wide ${
                        route === it.id
                            ? "border-b-2 border-brand text-brand"
                            : "text-text-secondary"
                    }`}
                >
                    {t(it.zh, it.en)}
                </button>
            ))}
        </nav>
    );
}

function BottomNav({
    route,
    onChange,
}: { route: Route; onChange: (r: Route) => void }) {
    const t = useT();
    return (
        <nav className="fixed inset-x-0 bottom-0 z-10 flex border-t border-border bg-bg-elevated">
            {NAV_ITEMS.map((it) => (
                <button
                    key={it.id}
                    type="button"
                    onClick={() => onChange(it.id)}
                    className={`flex-1 py-3 text-center text-sm ${
                        route === it.id ? "text-brand" : "text-text-secondary"
                    }`}
                >
                    {t(it.zh, it.en)}
                </button>
            ))}
        </nav>
    );
}
