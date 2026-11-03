import { sdk } from "./sdk";

// issue #95: 主题跟随 shell (UI spec — tokens.css 双主题) — 嵌入模式取
// sdk.theme 快照 + 订阅 host 'theme' 事件切换 html.light; 独立模式
// localStorage 记忆 → prefers-color-scheme 兜底 (对齐 cowork initTheme)。
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
            // localStorage 不可用时走系统偏好
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
