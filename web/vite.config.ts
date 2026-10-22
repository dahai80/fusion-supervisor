import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

// issue #25: dev server 11473 (architecture/port-registry.yaml). Proxies the
// supervisor API surface to the HTTP face on 9100 so standalone mode mirrors
// the embedded gateway /shell/fusion-supervisor/ layout (same-origin).
// FUSION_DEV_BACKEND / FUSION_SV_WEB_PORT override for parity.
const backend = process.env.FUSION_DEV_BACKEND ?? 'http://127.0.0.1:9100';
const port = Number(process.env.FUSION_SV_WEB_PORT ?? 11473);

const apiPaths = ['/api', '/metrics', '/health'];

export default defineConfig({
    plugins: [react()],
    // Embedded deployment serves dist through the gateway sub-path
    // /shell/fusion-supervisor/ — relative base + hash router works in both
    // embedded and standalone modes.
    base: './',
    server: {
        port,
        strictPort: true,
        proxy: Object.fromEntries(
            apiPaths.map((p) => [p, { target: backend, changeOrigin: true }])
        ),
    },
    preview: {
        port,
        strictPort: true,
        proxy: Object.fromEntries(
            apiPaths.map((p) => [p, { target: backend, changeOrigin: true }])
        ),
    },
    envPrefix: ['VITE_', 'FUSION_'],
});
