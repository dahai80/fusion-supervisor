import { useEffect, useState } from "react";
import { sdk } from "./sdk";

// issue #95: 语言状态绑定 SDK i18n 通道 (UI spec §9) — host 推 'lang'
// 事件时订阅者重渲染; 独立模式回退 <html lang> (SDK 内部 MutationObserver)。
// 全部模块文案经 t() 出口, 不自建词典文件 (对齐 fusion-cowork web GUI)。
export function useLang(): "zh" | "en" {
    const [lang, setLang] = useState<"zh" | "en">(sdk.i18n.lang());
    useEffect(() => sdk.i18n.subscribe((next) => setLang(next)), []);
    return lang;
}

export function useT() {
    const lang = useLang();
    return (zh: string, en: string) => (lang === "en" ? en : zh);
}
