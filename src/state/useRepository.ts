import { useState } from 'react'
import { boxApi } from '../shared/api/box-api'
import type { DshContainer, ExtensionBundle, RepositoryExtension } from '../shared/types/domain'

export function useRepository(
  onError: (message: string | null) => void,
  onContainers: (containers: DshContainer[]) => void,
) {
  const [repositoryDetails, setRepositoryDetails] = useState<RepositoryExtension[]>([])
  const [bundles, setBundles] = useState<ExtensionBundle[]>([])

  async function loadPluginRepository(): Promise<void> {
    try { const snapshot = await boxApi.listResourceStates(); setRepositoryDetails(snapshot.extensionRepository); onContainers(snapshot.containers) } catch (reason) { onError(String(reason)) }
  }

  async function importRepositoryPlugin(source: string): Promise<void> {
    try { await boxApi.enqueueRepositoryExtensionImport(source); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function exportRepositoryPlugin(entry: RepositoryExtension, text: { exportPlugin: string }): Promise<void> {
    try { const selected = await boxApi.choosePluginExport(text.exportPlugin, `${entry.name.replaceAll('/', '-')}.tar.gz`); if (typeof selected === 'string') await boxApi.enqueueRepositoryExtensionExport(entry.id, selected); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function deleteRepositoryPlugin(id: string): Promise<void> {
    try { await boxApi.removeRepositoryExtension(id); await loadPluginRepository(); onError(null) } catch (reason) { onError(String(reason)) }
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
    try { const selected = await boxApi.choosePluginExport(text.exportPlugin, `${bundle.name.replaceAll(/\s+/g, '-').toLowerCase()}.tar.gz`); if (typeof selected === 'string') await boxApi.enqueueBundleExport(bundle.id, selected, mode); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function importBundle(archive: string, conflict: string): Promise<void> {
    try { await boxApi.enqueueBundleImport(archive, conflict); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function installBundle(containerId: string, profile: string, bundleId: string, conflict: string): Promise<void> {
    try { await boxApi.enqueueContainerBundleInstall(containerId, profile, bundleId, conflict); onError(null) } catch (reason) { onError(String(reason)) }
  }

  return {
    repositoryDetails, setRepositoryDetails, bundles, loadPluginRepository, importRepositoryPlugin,
    exportRepositoryPlugin, deleteRepositoryPlugin, loadBundles, createBundle, deleteBundle,
    exportBundle, importBundle, installBundle,
  }
}
