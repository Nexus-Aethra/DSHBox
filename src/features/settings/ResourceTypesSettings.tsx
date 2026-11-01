import { useCallback, useEffect, useState } from 'react'
import { boxApi } from '../../shared/api/box-api'
import type { DshContainer, ResourceView } from '../../shared/types/domain'
import { Badge } from '../../ui/Badge'
import { Button } from '../../ui/Button'
import { Card } from '../../ui/Card'
import { Dialog } from '../../ui/Dialog'

export type ResourceTypesText = {
  resourceTypesTitle: string
  resourceTypesNote: string
  resourceTypesEmpty: string
  resourceTypesEnabled: string
  resourceTypesDisabled: string
  resourceTypesClose: string
  resourceTypesOpen: string
  resourceTypesDelete: string
  resourceTypesDeleteConfirm: (label: string) => string
  resourceTypesContainer: string
  resourceTypesKind: string
  resourceTypesPath: string
  resourceTypeSecret: string
  confirmTitle: string
  dialogCancel: string
  dialogConfirm: string
}

type Props = {
  text: ResourceTypesText
}

/**
 * The resource types Box shows as tabs under Resources, and the two ways a
 * user gets rid of one.
 *
 * "Close" and "Delete" are different acts and are kept apart on purpose:
 * closing hides the tab and keeps every copy already taken under that type,
 * while deleting removes the type itself. A user who closes a tab they do not
 * want to look at has not asked for their copies to be thrown away, and an
 * action that quietly did that would be the one way to lose state by accident.
 *
 * The daemon owns the flag; this pane only asks for it and re-reads the list,
 * so what it shows is what the Resources page will show.
 *
 * The last list seen this session is kept outside the component, below. The
 * pane is unmounted whenever the user switches to another Settings page, so
 * every return trip used to start from `[]` and paint an empty state before
 * the first response arrived. The daemon stays authoritative and the list is
 * small, so the cached copy is only ever a placeholder: the first render shows
 * it, and the reload this pane always performs replaces it a moment later.
 */
let lastViews: ResourceView[] = []

export function ResourceTypesSettings({ text }: Props) {
  const [views, setViews] = useState<ResourceView[]>(lastViews)
  const [containers, setContainers] = useState<DshContainer[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [busyId, setBusyId] = useState<string | null>(null)
  const [pendingDelete, setPendingDelete] = useState<ResourceView | null>(null)

  /**
   * The types, and separately the container names they refer to.
   *
   * The two reads are deliberately not awaited together. The list is the whole
   * point of this pane and the daemon answers it in milliseconds; the
   * container list walks the runtime directory and probes each host, so it can
   * take hundreds of milliseconds. Joined under one `Promise.all`, the fast
   * answer waited for the slow one and the pane sat blank with a spinner --
   * which reads as "no caching", because there is nothing else on screen to
   * look at.
   *
   * A container name is a label, not a gate: the row still renders with the id
   * while the names arrive, and `containerName` falls back to the id anyway.
   */
  const reload = useCallback(async (): Promise<void> => {
    setLoading(true)
    try {
      const listed = await boxApi.listResourceViews()
      lastViews = listed.views
      setViews(listed.views)
      setError(null)
    } catch (loadError) {
      setError(String(loadError))
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => { void reload() }, [reload])

  // Names are resolved once per mount and are not part of any reload: a toggle
  // must not re-walk the runtime directory just to redraw a label it already
  // knows. `containerName` falls back to the id, so an empty map is harmless.
  useEffect(() => {
    let cancelled = false
    boxApi.listContainers()
      .then((listed) => { if (!cancelled) setContainers(listed) })
      .catch(() => { /* the ids already render; a label miss is not an error */ })
    return () => { cancelled = true }
  }, [])

  function containerName(id: string): string {
    return containers.find((entry) => entry.id === id)?.name ?? id
  }

  /** Switch one type on or off. The daemon answers with the row it stored. */
  async function toggle(view: ResourceView): Promise<void> {
    setBusyId(view.id)
    setError(null)
    try {
      await boxApi.setResourceViewEnabled(view.id, !view.enabled)
      await reload()
    } catch (toggleError) {
      setError(String(toggleError))
    } finally {
      setBusyId(null)
    }
  }

  /** Remove the type itself. The copies taken under it are not touched. */
  async function confirmDelete(): Promise<void> {
    if (pendingDelete === null) return
    const view = pendingDelete
    setPendingDelete(null)
    setBusyId(view.id)
    setError(null)
    try {
      await boxApi.deleteResourceView(view.id)
      await reload()
    } catch (deleteError) {
      setError(String(deleteError))
    } finally {
      setBusyId(null)
    }
  }

  return (
    <section className="runtime-settings">
      <div className="workspace-heading">
        <div><p className="eyebrow">RESOURCES</p><h2>{text.resourceTypesTitle}</h2></div>
      </div>
      <p className="workspace-note">{text.resourceTypesNote}</p>

      {error !== null && <p className="error" role="alert">{error}</p>}

      {loading && views.length === 0 ? (
        <Card><span>…</span></Card>
      ) : views.length === 0 ? (
        <Card><span>{text.resourceTypesEmpty}</span></Card>
      ) : (
        <div className="settings-list">
          {views.map((view) => (
            <section key={view.id} className="settings-row">
              <div>
                <p className="label">{view.label}</p>
                <p className="path">
                  <code>{view.kind}</code>
                  {' · '}{text.resourceTypesContainer}: <code>{containerName(view.container)}</code>
                  {' · '}{text.resourceTypesPath}: <code>{view.path ?? view.kind}</code>
                </p>
              </div>
              <div className="settings-controls">
                {view.secret && <Badge variant="danger">{text.resourceTypeSecret}</Badge>}
                <Badge variant={view.enabled ? 'success' : 'neutral'}>
                  {view.enabled ? text.resourceTypesEnabled : text.resourceTypesDisabled}
                </Badge>
                <Button
                  variant="secondary"
                  size="sm"
                  disabled={busyId === view.id}
                  onClick={() => { void toggle(view) }}
                >
                  {view.enabled ? text.resourceTypesClose : text.resourceTypesOpen}
                </Button>
                <Button
                  variant="danger"
                  size="sm"
                  disabled={busyId === view.id}
                  onClick={() => { setPendingDelete(view) }}
                >
                  {text.resourceTypesDelete}
                </Button>
              </div>
            </section>
          ))}
        </div>
      )}

      <Dialog
        open={pendingDelete !== null}
        title={text.confirmTitle}
        description={pendingDelete === null ? '' : text.resourceTypesDeleteConfirm(pendingDelete.label)}
        onClose={() => { setPendingDelete(null) }}
      >
        <div className="ui-dialog-actions">
          <Button variant="secondary" size="sm" onClick={() => { setPendingDelete(null) }}>{text.dialogCancel}</Button>
          <Button variant="danger" size="sm" onClick={() => { void confirmDelete() }}>{text.dialogConfirm}</Button>
        </div>
      </Dialog>
    </section>
  )
}