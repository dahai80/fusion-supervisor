import { useEffect, useState } from "react";
import { sdk } from "./sdk";

// issue #95: language state bound to the SDK i18n channel (UI spec §9) —
// subscribers re-render when the host pushes a 'lang' event; standalone
// mode falls back to <html lang> (SDK-internal MutationObserver).
// All module copy goes through the t() outlet, no separate dictionary
// file (aligned with the fusion-cowork web GUI).
export function useLang(): "zh" | "en" {
    const [lang, setLang] = useState<"zh" | "en">(sdk.i18n.lang());
    useEffect(() => sdk.i18n.subscribe((next) => setLang(next)), []);
    return lang;
}

export function useT() {
    const lang = useLang();
    return (zh: string, en: string) => (lang === "en" ? en : zh);
}
