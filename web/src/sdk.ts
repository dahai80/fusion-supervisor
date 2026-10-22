import { createShellSdk } from "@fusion/web-sdk";

// Single SDK instance for the module (toast/tab/theme bridge to the shell
// host). Data calls use raw same-origin fetch in api.ts.
export const embeddedParam = (): boolean => {
    try {
        return (
            new URLSearchParams(window.location.search).get("embedded") === "1"
        );
    } catch {
        return false;
    }
};

export const isEmbedded = (): boolean =>
    typeof window !== "undefined" &&
    (window.parent !== window || embeddedParam());

export const sdk = createShellSdk({ embedded: isEmbedded() });
