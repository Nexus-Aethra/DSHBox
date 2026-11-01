import { useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'

export type TabItem<T extends string> = { id: T; label: string; disabled?: boolean }

export type TabsProps<T extends string> = {
  items: TabItem<T>[]
  value: T
  onChange: (next: T) => void
  children: ReactNode
  ariaLabel?: string
  /** Rendered after the tabs — a trailing affordance such as "add a type". */
  action?: ReactNode
  /** Label for the overflow menu that holds the tabs that do not fit. */
  overflowLabel?: string
}

/**
 * How much slack a tab needs before it leaves the overflow menu again.
 *
 * The menu sits beside the strip, so showing it takes room away from the
 * strip: a tab at the boundary would fit again, disappear from the menu,
 * widen the strip, and overflow once more. Requiring real slack to move a
 * tab back makes that settle instead of flapping.
 */
const MENU_SLACK_PX = 28

/**
 * A tab strip that survives more tabs than fit.
 *
 * Two mechanisms, because either one alone is wrong on its own: the strip
 * scrolls horizontally, and the tabs that are out of view are also collected
 * into an overflow menu. Scrolling alone hides the tabs that are cut off — the
 * user cannot see that "Chat history" exists any more, and cannot click it
 * either. A menu alone means the visible tabs stay put while the hidden ones
 * jump between places, which makes the strip reorder itself as the window
 * changes width.
 *
 * The overflow menu is a details element rather than a popover: the Box UI
 * is plain HTML and CSS, and a details/summary needs no positioning code to
 * stay inside the window.
 */
export function Tabs<T extends string>({ items, value, onChange, children, ariaLabel, action, overflowLabel = 'More' }: TabsProps<T>) {
  const stripRef = useRef<HTMLDivElement | null>(null)
  const [overflowing, setOverflowing] = useState<string[]>([])
  const [menuOpen, setMenuOpen] = useState(false)
  // The measurement reads the previous answer to decide whether a tab has
  // earned its way back out of the menu, but an effect that also *depends* on
  // that answer re-subscribes on every change. The ref is what the measurement
  // reads; the state is what renders.
  const overflowingRef = useRef<string[]>([])

  /**
   * Which tabs are cut off. Measured rather than computed from a width guess:
   * the strip's real width depends on the labels, the font and the window, so
   * the only honest answer comes from looking at where the buttons ended up.
   *
   * A tab is out of view when its right edge passes the strip's own visible
   * width. Comparing the strip's *content* width against that instead — the
   * difference `scrollWidth - clientWidth` — is what the first version got
   * wrong: that number is the amount hidden, so the first tab whose right edge
   * passed it was treated as cut off while it was in plain sight.
   */
  function measure(): void {
    const strip = stripRef.current
    if (strip === null) return
    const width = strip.clientWidth
    if (width <= 1) return
    const stripRect = strip.getBoundingClientRect()
    const buttons = Array.from(strip.querySelectorAll<HTMLButtonElement>('button[data-tab-id]'))
    const next: string[] = []
    const current = overflowingRef.current
    for (const button of buttons) {
      const id = button.dataset.tabId ?? ''
      // `getBoundingClientRect` moves with scrolling, `clientWidth` does not:
      // add the scroll offset back so a scrolled strip is measured in layout
      // coordinates rather than in whatever is currently on screen.
      const right = button.getBoundingClientRect().right - stripRect.left + strip.scrollLeft
      const wasHidden = current.includes(id)
      if (right > width + 0.5 || (wasHidden && right > width - MENU_SLACK_PX)) next.push(id)
    }
    // A new array on every render would re-render forever; compare first.
    if (next.length === current.length && next.every((id, index) => id === current[index])) return
    overflowingRef.current = next
    setOverflowing(next)
  }

  useEffect(() => {
    measure()
    const strip = stripRef.current
    if (strip === null || typeof ResizeObserver === 'undefined') {
      window.addEventListener('resize', measure)
      return () => window.removeEventListener('resize', measure)
    }
    const observer = new ResizeObserver(measure)
    observer.observe(strip)
    window.addEventListener('resize', measure)
    return () => {
      observer.disconnect()
      window.removeEventListener('resize', measure)
    }
    // Labels change width without changing `items.length` (a rename, or a
    // translated string), and a strip measured against the old labels hides
    // the wrong tabs. `overflowing` is deliberately absent: the measurement
    // already re-runs when the container resizes, and depending on its own
    // answer would make every change a new subscription.
  }, [items.map((item) => item.id + ':' + item.label).join('\u0001')])

  useEffect(() => { setMenuOpen(false) }, [value])

  /** Bring the selected tab into view; a menu entry is not a visible tab. */
  useEffect(() => {
    const strip = stripRef.current
    if (strip === null) return
    const active = strip.querySelector<HTMLButtonElement>('button[data-tab-id="' + value + '"]')
    if (active === null) return
    const stripRect = strip.getBoundingClientRect()
    const rect = active.getBoundingClientRect()
    if (rect.left < stripRect.left) strip.scrollLeft -= stripRect.left - rect.left + 8
    else if (rect.right > stripRect.right) strip.scrollLeft += rect.right - stripRect.right + 8
  }, [value])

  function focusTab(index: number) {
    const list = ariaLabel ?? 'default'
    const buttons = document.querySelectorAll<HTMLButtonElement>('[data-tabs-list="' + list + '"] button[data-tab-id]')
    buttons[index]?.focus()
  }

  function onKey(event: React.KeyboardEvent<HTMLButtonElement>, index: number) {
    if (event.key === 'ArrowRight' || event.key === 'ArrowDown') {
      event.preventDefault()
      const next = items[(index + 1) % items.length]
      if (next !== undefined && !next.disabled) {
        onChange(next.id)
        focusTab((index + 1) % items.length)
      }
    } else if (event.key === 'ArrowLeft' || event.key === 'ArrowUp') {
      event.preventDefault()
      const prev = items[(index - 1 + items.length) % items.length]
      if (prev !== undefined && !prev.disabled) {
        onChange(prev.id)
        focusTab((index - 1 + items.length) % items.length)
      }
    }
  }

  const overflowItems = items.filter((item) => overflowing.includes(item.id))

  return (
    <div className="ui-tabs">
      <div className="ui-tab-list" role="tablist" aria-label={ariaLabel} data-tabs-list={ariaLabel ?? 'default'}>
        <div className="ui-tab-strip" ref={stripRef}>
          {items.map((item, index) => (
            <button
              key={item.id}
              type="button"
              role="tab"
              data-tab-id={item.id}
              aria-selected={item.id === value}
              aria-controls={'tab-panel-' + item.id}
              disabled={item.disabled}
              className={'ui-tab' + (item.id === value ? ' active' : '')}
              onClick={() => onChange(item.id)}
              onKeyDown={(event) => onKey(event, index)}
            >
              {item.label}
            </button>
          ))}
        </div>
        {overflowItems.length > 0 && (
          <details
            className="ui-tab-overflow"
            open={menuOpen}
            onToggle={(event) => { setMenuOpen((event.currentTarget as HTMLDetailsElement).open) }}
          >
            <summary aria-label={overflowLabel}>{overflowLabel} ({overflowItems.length})</summary>
            <div className="ui-tab-overflow-menu" role="menu">
              {overflowItems.map((item) => (
                <button
                  key={item.id}
                  type="button"
                  role="menuitem"
                  className={'ui-tab-overflow-item' + (item.id === value ? ' active' : '')}
                  onClick={() => { onChange(item.id); setMenuOpen(false) }}
                >
                  {item.label}
                </button>
              ))}
            </div>
          </details>
        )}
        {action}
      </div>
      <div className="ui-tab-panel" role="tabpanel" id={'tab-panel-' + value}>{children}</div>
    </div>
  )
}
