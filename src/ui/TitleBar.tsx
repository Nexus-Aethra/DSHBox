import { useEffect, useMemo, useState } from 'react'
import type { ReactNode } from 'react'
import { getCurrentWindow } from '@tauri-apps/api/window'
import type { Text } from '../i18n'

/**
 * The window chrome. Windows/Linux run the main window with
 * `decorations: false`, so this full-width strip is the only title bar the
 * window has: it drags the window (`data-tauri-drag-region`), toggles
 * maximize on double click (built into the drag region) and hosts the three
 * window control buttons, which call the core window API -- no custom IPC
 * commands, the permissions come from `capabilities/default.json`. On macOS
 * the native traffic lights stay (`titleBarStyle: "Overlay"`), so the bar
 * only reserves room for them and draws no buttons of its own. In a plain
 * browser (`pnpm dev`) there is no window to control: the bar renders, the
 * buttons do not -- keyed off the missing `__TAURI_INTERNALS__`.
 */

type TitleBarMode = 'tauri-win' | 'tauri-mac' | 'browser'

function detectMode(): TitleBarMode {
  if (typeof window === 'undefined' || !('__TAURI_INTERNALS__' in window)) return 'browser'
  return /Macintosh/.test(navigator.userAgent) ? 'tauri-mac' : 'tauri-win'
}

function WindowsControls({ text, maximized }: { text: Text; maximized: boolean }) {
  const win = getCurrentWindow()
  return (
    <div className="window-controls">
      <button type="button" className="win-btn" aria-label={text.winMinimize} title={text.winMinimize}
        onClick={() => { void win.minimize() }}>
        <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true"><path d="M0 5.5h10" fill="none" stroke="currentColor" /></svg>
      </button>
      <button type="button" className="win-btn" aria-label={maximized ? text.winRestore : text.winMaximize}
        title={maximized ? text.winRestore : text.winMaximize} onClick={() => { void win.toggleMaximize() }}>
        {maximized ? (
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M2.5 2.5v-2h7v7h-2" fill="none" stroke="currentColor" />
            <rect x="0.5" y="2.5" width="7" height="7" fill="none" stroke="currentColor" />
          </svg>
        ) : (
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <rect x="0.5" y="0.5" width="9" height="9" fill="none" stroke="currentColor" />
          </svg>
        )}
      </button>
      <button type="button" className="win-btn win-btn-close" aria-label={text.winClose} title={text.winClose}
        onClick={() => { void win.close() }}>
        <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true"><path d="M0 0l10 10M10 0L0 10" fill="none" stroke="currentColor" /></svg>
      </button>
    </div>
  )
}

export function TitleBar({ text, children }: { text: Text; children?: ReactNode }) {
  const mode = useMemo(detectMode, [])
  const [maximized, setMaximized] = useState(false)

  useEffect(() => {
    if (mode === 'browser') return
    const win = getCurrentWindow()
    let alive = true
    let unlisten: (() => void) | null = null
    const sync = (): void => {
      void win.isMaximized().then((m) => { if (alive) setMaximized(m) }).catch(() => { /* no window to ask */ })
    }
    sync()
    void win.onResized(() => { sync() })
      .then((fn) => { if (alive) unlisten = fn; else fn() })
      .catch(() => { /* as above */ })
    return () => {
      alive = false
      unlisten?.()
    }
  }, [mode])

  const dragAttr = mode === 'browser' ? undefined : 'true'
  return (
    <header className={mode === 'tauri-mac' ? 'titlebar mac' : 'titlebar'} data-tauri-drag-region={dragAttr}>
      <div className="titlebar-content" data-tauri-drag-region={dragAttr}>
        {children ?? <div className="brand">DSH Box</div>}
      </div>
      {mode === 'tauri-win' && <WindowsControls text={text} maximized={maximized} />}
    </header>
  )
}
