import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import path from 'node:path'
import { readFileSync } from 'node:fs'

// Vite configuration for the Agent WebUI.
// - base './' so the built bundle can be served from any sub-path (incl. by
//   `agent --serve`, which serves ./web as static files).
// - '@' alias → ./src for clean, decoupled imports.
// - dev proxy forwards /api and /ws to the Rust agent server (default
//   127.0.0.1:8080) so `npm run dev` (:5173) talks to `agent --serve` (:8080).
// 注入 package.json 版本号（Sidebar 页脚展示），避免源码里硬编码 v1.0 漂移。
const pkg = JSON.parse(readFileSync(path.resolve(__dirname, 'package.json'), 'utf-8')) as { version: string }
export default defineConfig({
  base: './',
  plugins: [react()],
  define: {
    __APP_VERSION__: JSON.stringify(pkg.version),
  },
  resolve: {
    alias: {
      '@': path.resolve(__dirname, 'src'),
    },
  },
  build: {
    // 构建产物直接写入仓库根 web/（rust-embed 内嵌服务目录，见 server/lib.rs 的 WebAsset
    // folder）。与 `--serve` 读取路径一致——`npm run build` 后产物即生效（debug 模式 rust-embed
    // 实时读盘；release 需重新编译二进制内嵌）。emptyOutDir=false：outDir 在项目根之外，
    // 禁止清空以免误删 web/c5-ui 源码与既有字体资源。
    outDir: path.resolve(__dirname, '..'),
    emptyOutDir: false,
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
})
