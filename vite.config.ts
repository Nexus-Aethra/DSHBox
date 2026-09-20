import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import { dshboxDevRpcBridge } from './scripts/dev-rpc-bridge.mjs'

export default defineConfig({
  // `dshboxDevRpcBridge` is `apply: 'serve'`, so it exists only while `pnpm dev`
  // runs. It is what lets a plain browser reach the daemon; see its header.
  plugins: [react(), dshboxDevRpcBridge()],
  clearScreen: false,
  // Tauri reads its frontend assets from src-tauri/dist. The default
  // vite outDir is the repo-root `dist/`, which leaves src-tauri/dist
  // empty and breaks `tauri build` with "Unable to find your web
  // assets".
  build: {
    outDir: 'src-tauri/dist',
    emptyOutDir: true,
  },
  server: {
    strictPort: true,
    port: 1420,
  },
})
