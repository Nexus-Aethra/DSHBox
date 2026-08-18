import type { ReactNode } from 'react'
import { useEffect, useRef } from 'react'

export type DialogProps = {
  open: boolean
  title: string
  description?: string
  onClose: () => void
  children?: ReactNode
}

export function Dialog({ open, title, description, onClose, children }: DialogProps) {
  const ref = useRef<HTMLDialogElement | null>(null)

  useEffect(() => {
    const node = ref.current
    if (node === null) return
    if (open && !node.open) {
      node.showModal()
    } else if (!open && node.open) {
      node.close()
    }
  }, [open])

  useEffect(() => {
    const node = ref.current
    if (node === null) return
    const handleClose = () => onClose()
    node.addEventListener('close', handleClose)
    return () => node.removeEventListener('close', handleClose)
  }, [onClose])

  function handleClick(event: React.MouseEvent<HTMLDialogElement>) {
    // native <dialog> dispatches a click on the backdrop; close only when
    // the click originated outside the inner content rect.
    if (event.target === ref.current) {
      const rect = ref.current.getBoundingClientRect()
      const inside =
        event.clientX >= rect.left && event.clientX <= rect.right &&
        event.clientY >= rect.top && event.clientY <= rect.bottom
      if (!inside) onClose()
    }
  }

  return (
    <dialog ref={ref} className="ui-dialog" onClick={handleClick} onCancel={onClose}>
      <h2 className="ui-dialog-title">{title}</h2>
      {description !== undefined && description !== '' && <p className="ui-dialog-description">{description}</p>}
      {children}
    </dialog>
  )
}
