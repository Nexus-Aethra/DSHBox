import type { ReactNode } from 'react'

export type TabItem<T extends string> = { id: T; label: string; disabled?: boolean }

export type TabsProps<T extends string> = {
  items: TabItem<T>[]
  value: T
  onChange: (next: T) => void
  children: ReactNode
  ariaLabel?: string
}

export function Tabs<T extends string>({ items, value, onChange, children, ariaLabel }: TabsProps<T>) {
  function focusTab(index: number) {
    const buttons = document.querySelectorAll<HTMLButtonElement>(`[data-tabs-list="${ariaLabel ?? 'default'}"] button`)
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
  return (
    <div className="ui-tabs">
      <div className="ui-tab-list" role="tablist" aria-label={ariaLabel} data-tabs-list={ariaLabel ?? 'default'}>
        {items.map((item, index) => (
          <button
            key={item.id}
            type="button"
            role="tab"
            aria-selected={item.id === value}
            aria-controls={`tab-panel-${item.id}`}
            disabled={item.disabled}
            className={`ui-tab${item.id === value ? ' active' : ''}`}
            onClick={() => onChange(item.id)}
            onKeyDown={(event) => onKey(event, index)}
          >
            {item.label}
          </button>
        ))}
      </div>
      <div className="ui-tab-panel" role="tabpanel" id={`tab-panel-${value}`}>{children}</div>
    </div>
  )
}
