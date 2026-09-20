import { useEffect, useState } from 'react'
import { boxApi } from '../../shared/api/box-api'
import type { DiscoveredResource, DshContainer } from '../../shared/types/domain'
import { Badge } from '../../ui/Badge'
import { Button } from '../../ui/Button'
import { Dialog } from '../../ui/Dialog'
import { Field } from '../../ui/Field'
import { Input } from '../../ui/Input'
import { Select } from '../../ui/Select'
import { ContainerTree, PluginChips, useInstalledPlugins } from './pickers'
import type { PickerText } from './pickers'

export type AddResourceTypeText = PickerText & {
  resourceAddType: string
  resourceAddTypeHint: string
  resourceAddModeDetected: string
  resourceAddModePlugin: string
  resourceAddModePath: string
  resourcePickContainer: string
  resourcePickContainerPlaceholder: string
  resourcePickKind: string
  resourceKindEmpty: string
  resourceKindLoading: string
  resourceLabel: string
  resourceLabelPlaceholder: string
  resourceAdd: string
  resourceAdding: string
  resourceSecret: string
  resourceAbsent: string
  resourceError: (message: string) => string
  dialogCancel: string
  resourceBuiltin: string
  resourceDeclared: string
  resourceInferred: string
  resourceBrowseExtract: string
}

type Props = {
  containers: DshContainer[]
  text: AddResourceTypeText
  onClose: () => void
  onAdded: () => Promise<void>
}

type Mode = 'detected' | 'plugin' | 'path'

/** What the user settled on, in the three ways the dialog offers. */
type Selection = { kind: string; path?: string; label: string; secret: boolean }

/**
 * Add a resource type to the Resources navigation. Three ways in, because the
 * right one depends on what the user already knows:
 *
 * - the kinds Box detected in a container (built-in, declared, scanned),
 * - a plugin to scan, so a plugin's own state is found without a path,
 * - the container's storage area as a tree, for a path nobody declared.
 */
export function AddResourceTypeDialog({ containers, text, onClose, onAdded }: Props) {
  const [container, setContainer] = useState(containers[0]?.id ?? '')
  const [mode, setMode] = useState<Mode>('detected')
  const [detected, setDetected] = useState<DiscoveredResource[]>([])
  const [plugin, setPlugin] = useState('')
  const [pluginQuery, setPluginQuery] = useState('')
  const [selection, setSelection] = useState<Selection | null>(null)
  const [label, setLabel] = useState('')
  const [loading, setLoading] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const installedPlugins = useInstalledPlugins(container)

  useEffect(() => {
    if (!container) return
    let cancelled = false
    setLoading(true)
    setSelection(null)
    // With a plugin chosen this is that plugin's own scan; without one it is
    // everything the container holds.
    boxApi.listContainerResources(container, plugin || undefined)
      .then((data) => {
        if (cancelled) return
        setDetected(plugin ? data.resources.filter((entry) => entry.plugin === plugin || entry.scope === 'builtin' ? entry.plugin === plugin : false) : data.resources)
        setError(null)
      })
      .catch((loadError: unknown) => { if (!cancelled) setError(String(loadError)) })
      .finally(() => { if (!cancelled) setLoading(false) })
    return () => { cancelled = true }
  }, [container, plugin])

  function choose(entry: DiscoveredResource): void {
    setSelection({
      kind: entry.kind.id,
      // An inferred kind carries no trusted location, so pin where it was found.
      path: entry.scope === 'inferred' ? entry.kind.path : undefined,
      label: entry.kind.label,
      secret: entry.kind.secret,
    })
    setLabel(entry.kind.label)
  }

  function choosePath(path: string): void {
    const name = path.split('/').filter(Boolean).pop() ?? path
    setSelection({ kind: 'path', path, label: name, secret: false })
    setLabel(name)
  }

  async function add(): Promise<void> {
    if (selection === null || !container) return
    setBusy(true)
    try {
      await boxApi.addResourceView({
        kind: selection.kind,
        container,
        label: label.trim() || selection.label,
        path: selection.path,
      })
      await onAdded()
      onClose()
    } catch (addError) {
      setError(String(addError))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open title={text.resourceAddType} description={text.resourceAddTypeHint} onClose={onClose} size="lg">
      <div className="resource-panel">
        {error !== null && <p className="resource-error">{text.resourceError(error)}</p>}
        <div className="resource-controls">
          <Field label={text.resourcePickContainer}>
            {(fieldId) => (
              <Select
                id={fieldId}
                value={container}
                placeholder={text.resourcePickContainerPlaceholder}
                options={containers.map((entry) => ({ value: entry.id, label: entry.name }))}
                onChange={(event) => { setContainer(event.target.value); setPlugin(''); setPluginQuery(''); setMode('detected') }}
              />
            )}
          </Field>
          <div className="resource-modes">
            <Button variant="ghost" size="sm" className={mode === 'detected' ? 'active' : ''} onClick={() => { setMode('detected') }}>{text.resourceAddModeDetected}</Button>
            <Button variant="ghost" size="sm" className={mode === 'plugin' ? 'active' : ''} onClick={() => { setMode('plugin') }}>{text.resourceAddModePlugin}</Button>
            <Button variant="ghost" size="sm" className={mode === 'path' ? 'active' : ''} onClick={() => { setMode('path'); setSelection(null) }}>{text.resourceAddModePath}</Button>
          </div>
        </div>

        {mode === 'plugin' && (
          <PluginChips
            plugins={installedPlugins}
            query={pluginQuery}
            onQuery={setPluginQuery}
            selected={plugin}
            onSelect={setPlugin}
            text={text}
          />
        )}

        {mode === 'path' ? (
          <ContainerTree id={container} text={text} onPick={choosePath} />
        ) : (
          <>
            <span className="resource-section">{text.resourcePickKind}</span>
            {loading && <p className="resource-note">{text.resourceKindLoading}</p>}
            {!loading && detected.length === 0 && <p className="resource-note">{text.resourceKindEmpty}</p>}
            <ul className="resource-kind-list">
              {detected.map((entry) => (
                <li key={`${entry.kind.id}:${entry.kind.path}`}>
                  <button
                    type="button"
                    className={selection?.kind === entry.kind.id && selection?.path === (entry.scope === 'inferred' ? entry.kind.path : undefined) ? 'active' : ''}
                    onClick={() => { choose(entry) }}
                  >
                    <span>{entry.kind.label}</span>
                    {entry.kind.secret && <Badge variant="danger">{text.resourceSecret}</Badge>}
                    <span className="resource-browser-meta">
                      {entry.scope === 'declared' ? text.resourceDeclared : entry.scope === 'inferred' ? text.resourceInferred : text.resourceBuiltin}
                      {' · '}{entry.exists ? entry.kind.path : `${entry.kind.path} (${text.resourceAbsent})`}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          </>
        )}

        {selection !== null && (
          <>
            <p className="resource-note">
              {text.resourceBrowseExtract}
              {': '}<code>{selection.path ?? selection.kind}</code>
              {selection.secret && <Badge variant="danger">{text.resourceSecret}</Badge>}
            </p>
            <Field label={text.resourceLabel}>
              {(fieldId) => (
                <Input
                  id={fieldId}
                  size="sm"
                  value={label}
                  placeholder={text.resourceLabelPlaceholder}
                  onChange={(event) => { setLabel(event.target.value) }}
                />
              )}
            </Field>
          </>
        )}

        <div className="resource-footer">
          <Button variant="ghost" size="sm" onClick={onClose}>{text.dialogCancel}</Button>
          <Button variant="primary" size="sm" disabled={busy || selection === null} onClick={() => { void add() }}>
            {busy ? text.resourceAdding : text.resourceAdd}
          </Button>
        </div>
      </div>
    </Dialog>
  )
}
