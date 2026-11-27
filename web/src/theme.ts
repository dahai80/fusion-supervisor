import { sdk } from "./sdk";

// issue #95: theme follows the shell (UI spec — tokens.css dual theme) —
// embedded mode takes the sdk.theme snapshot + subscribes to the host
// 'theme' event to toggle html.light; standalone mode falls back to
// localStorage memory → prefers-color-scheme (aligned with cowork
// initTheme).
export function initTheme() {
    const apply = (mode: "dark" | "light") => {
        const el = document.documentElement;
        el.classList.toggle("light", mode === "light");
        el.classList.toggle("dark", mode === "dark");
    };
    const standalone = () => {
        try {
            const saved = localStorage.getItem("fusion.shell.theme");
            if (saved === "light" || saved === "dark") return saved;
        } catch {
            // localStorage unavailable — fall back to system preference
        }
        return window.matchMedia("(prefers-color-scheme: light)").matches
            ? "light"
            : "dark";
    };
    apply(sdk.theme ? sdk.theme.get() : standalone());
    if (sdk.theme) {
        sdk.theme.subscribe((mode) => apply(mode));
    } else {
        window
            .matchMedia("(prefers-color-scheme: light)")
            .addEventListener("change", (e) =>
                apply(e.matches ? "light" : "dark"),
            );
    }
}
