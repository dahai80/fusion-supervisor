import { fusionPreset } from "@fusion/web-ui/preset";
import type { Config } from "tailwindcss";

export default {
    presets: [fusionPreset as Config],
    content: ["./index.html", "./src/**/*.{ts,tsx}"],
} satisfies Config;
