import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
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
