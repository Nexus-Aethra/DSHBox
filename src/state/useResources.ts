import { useState } from 'react'
import { boxApi } from '../shared/api/box-api'
import type { DshContainer, ExtensionBundle, PreviewScriptResult, RepositoryExtension, TemplateInfo } from '../shared/types/domain'

export function useResources(
  onError: (message: string | null) => void,
  onContainers: (containers: DshContainer[]) => void,
) {
  const [plugins, setPlugins] = useState<RepositoryExtension[]>([])
  const [bundles, setBundles] = useState<ExtensionBundle[]>([])
  const [scriptPreview, setScriptPreview] = useState<PreviewScriptResult | null>(null)
  const [references, setReferences] = useState<Record<string, { containers: number; templates: number }>>({})

  // Page-scoped loading: no mount-time prefetch. Every section/tab loads
  // its own data when the user enters it (see App.tsx `loadSection` and the
  // ResourcesPage tab effect), so the list on screen always matches the
  // daemon even when the CLI changed state in between.

  async function loadPlugins(): Promise<void> {
    try {
      const snapshot = await boxApi.listResourceStates()
      setPlugins(snapshot.extensionRepository)
      setReferences(snapshot.repositoryReferences ?? {})
      onContainers(snapshot.containers)
    } catch (reason) { onError(String(reason)) }
  }

  async function importPlugin(source: string): Promise<void> {
    try { await boxApi.enqueueRepositoryExtensionImport(source); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function exportPlugin(entry: RepositoryExtension, text: { exportPlugin: string }): Promise<void> {
    try {
      const selected = await boxApi.choosePluginExport(text.exportPlugin, `${entry.name.replaceAll('/', '-')}.tar.gz`)
      if (typeof selected === 'string') await boxApi.enqueueRepositoryExtensionExport(entry.id, selected); onError(null)
    } catch (reason) { onError(String(reason)) }
  }

  async function deletePlugin(id: string): Promise<void> {
    try { await boxApi.removeRepositoryExtension(id); await loadPlugins(); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function loadBundles(): Promise<void> {
    try { setBundles(await boxApi.listExtensionBundles()); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function createBundle(name: string, ids: string[]): Promise<void> {
    try { await boxApi.createExtensionBundle(name, ids); await loadBundles(); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function deleteBundle(id: string): Promise<void> {
    try { await boxApi.deleteExtensionBundle(id); setBundles((current) => current.filter((bundle) => bundle.id !== id)); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function exportBundle(bundle: ExtensionBundle, mode: string, text: { exportPlugin: string }): Promise<void> {
    try {
      const selected = await boxApi.choosePluginExport(text.exportPlugin, `${bundle.name.replaceAll(/\s+/g, '-').toLowerCase()}.tar.gz`)
      if (typeof selected === 'string') await boxApi.enqueueBundleExport(bundle.id, selected, mode); onError(null)
    } catch (reason) { onError(String(reason)) }
  }

  async function importBundle(archive: string, conflict: string): Promise<void> {
    try { await boxApi.enqueueBundleImport(archive, conflict); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function installBundle(containerId: string, profile: string, bundleId: string, conflict: string): Promise<void> {
    try { await boxApi.enqueueContainerBundleInstall(containerId, profile, bundleId, conflict); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function chooseScriptFile(text: { chooseScript: string }): Promise<string | null> {
    try {
      const selected = await boxApi.chooseScriptFile(text.chooseScript)
      if (typeof selected === 'string') {
        const preview = await boxApi.previewImageScript(selected)
        setScriptPreview(preview)
        return selected
      }
      return null
    } catch (reason) { onError(String(reason)); return null }
  }

  async function buildTemplate(scriptPath: string): Promise<void> {
    try {
      await boxApi.enqueueImageBuild(scriptPath, null, null)
      setScriptPreview(null)
      onError(null)
    } catch (reason) { onError(String(reason)) }
  }

  async function importTemplateFromArchive(archive: string, name: string | null): Promise<void> {
    try {
      await boxApi.importTemplate(archive, name)
      onError(null)
    } catch (reason) { onError(String(reason)) }
  }

  async function exportTemplateToFile(name: string, destination: string): Promise<void> {
    try {
      await boxApi.exportTemplate(name, destination)
      onError(null)
    } catch (reason) { onError(String(reason)) }
  }

  async function removeTemplateByName(name: string): Promise<void> {
    try {
      await boxApi.removeTemplate(name)
      onError(null)
    } catch (reason) { onError(String(reason)) }
  }

  return {
    plugins, setPlugins, bundles, scriptPreview, references,
    loadPlugins, importPlugin,
    exportPlugin, deletePlugin, loadBundles, createBundle, deleteBundle,
    exportBundle, importBundle, installBundle, chooseScriptFile, buildTemplate,
    importTemplateFromArchive, exportTemplateToFile, removeTemplateByName,
  }
}
