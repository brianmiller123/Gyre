import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import path from 'node:path';
// Vite configuration for the Agent WebUI.
// - base './' so the built bundle can be served from any sub-path (incl. by
//   `agent --serve`, which serves ./web as static files).
// - '@' alias → ./src for clean, decoupled imports.
// - dev proxy forwards /api and /ws to the Rust agent server (default
//   127.0.0.1:8080) so `npm run dev` (:5173) talks to `agent --serve` (:8080).
export default defineConfig({
    base: './',
    plugins: [react()],
    resolve: {
        alias: {
            '@': path.resolve(__dirname, 'src'),
        },
    },
    server: {
        host: true,
        port: 5173,
        strictPort: false,
        proxy: {
            '/api': { target: 'http://127.0.0.1:8080', changeOrigin: true },
            '/ws': { target: 'ws://127.0.0.1:8080', ws: true, changeOrigin: true },
        },
    },
});
