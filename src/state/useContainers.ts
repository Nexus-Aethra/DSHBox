import { useEffect, useState } from 'react'
import { boxApi } from '../shared/api/box-api'
import type { ContainerExtensions, DshContainer, RepositoryExtension, WorkspaceExtension } from '../shared/types/domain'

export function useContainers(
  installedDshVersions: string[],
  onError: (message: string | null) => void,
  onRepositoryUpdate: (repository: RepositoryExtension[]) => void,
) {
  const [containers, setContainers] = useState<DshContainer[]>([])
  const [selectedContainer, setSelectedContainer] = useState<DshContainer | null>(null)
  const [containerDetails, setContainerDetails] = useState<ContainerExtensions | null>(null)
  const [containerVersion, setContainerVersion] = useState('')
  const [creatingContainer, setCreatingContainer] = useState(false)
  const [creatingContainerView, setCreatingContainerView] = useState(false)
  const [containerName, setContainerName] = useState('')
  const [newContainerProfile, setNewContainerProfile] = useState('web')
  const [containerMenuId, setContainerMenuId] = useState<string | null>(null)
  const [workspaceExtensions, setWorkspaceExtensions] = useState<WorkspaceExtension[]>([])

  // Default the create form to the first installed DSH version once known.
  useEffect(() => {
    setContainerVersion((current) => current || installedDshVersions[0] || '')
  }, [installedDshVersions])

  async function loadContainers(): Promise<void> {
    try { setContainers(await boxApi.listContainers()) } catch (reason) { onError(String(reason)) }
  }

  async function createContainer(): Promise<void> {
    if (!containerVersion) return
    setCreatingContainer(true)
    try { await boxApi.createContainer(containerName, containerVersion, newContainerProfile); await loadContainers(); setCreatingContainerView(false); setContainerName(''); setNewContainerProfile('web'); onError(null) } catch (reason) { onError(String(reason)) } finally { setCreatingContainer(false) }
  }

  async function showContainerDetails(container: DshContainer): Promise<void> {
    setSelectedContainer(container); setContainerDetails(null)
    try { const [details, snapshot] = await Promise.all([boxApi.getContainerDetails(container.id), boxApi.listResourceStates()]); setContainerDetails(details); onRepositoryUpdate(snapshot.extensionRepository); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function refreshSelectedContainer(id: string): Promise<void> {
    const [updated, details] = await Promise.all([boxApi.listContainers(), boxApi.getContainerDetails(id)])
    setContainers(updated); setSelectedContainer(updated.find((item) => item.id === id) ?? null); setContainerDetails(details)
  }

  async function addContainerProfile(profile: string): Promise<void> {
    if (selectedContainer === null) return
    try { await boxApi.addContainerProfile(selectedContainer.id, profile); await refreshSelectedContainer(selectedContainer.id); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function setContainerProfile(profile: string): Promise<void> {
    if (selectedContainer === null) return
    try { await boxApi.setContainerProfile(selectedContainer.id, profile); await refreshSelectedContainer(selectedContainer.id); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function addContainerExtension(profile: string | null, repositoryId: string): Promise<void> {
    if (selectedContainer === null) return
    try { await boxApi.enqueueContainerExtensionCopy(selectedContainer.id, profile, repositoryId); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function deleteContainerPlugin(profile: string, name: string): Promise<void> {
    if (selectedContainer === null) return
    try { await boxApi.removeRepositoryPlugin(selectedContainer.id, profile, name); await refreshSelectedContainer(selectedContainer.id); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function scanWorkspaceExtensions(): Promise<void> {
    if (selectedContainer === null) return
    try { setWorkspaceExtensions(await boxApi.scanContainerWorkspaceExtensions(selectedContainer.id)); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function importWorkspaceExtension(relativePath: string): Promise<void> {
    if (selectedContainer === null) return
    try { await boxApi.enqueueWorkspaceExtensionImport(selectedContainer.id, relativePath); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function readContainerLog(id: string, log: 'host' | 'rebuild' | 'webview'): Promise<string> { return boxApi.readContainerLog(id, log) }

  async function toggleContainer(container: DshContainer): Promise<void> {
    try {
      if (container.status === 'running') await boxApi.enqueueContainerStop(container.id)
      else await boxApi.enqueueContainerStart(container.id)
      onError(null)
    } catch (reason) { onError(String(reason)) }
  }

  async function openContainer(container: DshContainer): Promise<void> {
    try { await boxApi.openContainer(container.id); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function openContainerInBrowser(id: string): Promise<void> { try { await boxApi.openDshFrontBrowser(id); onError(null) } catch (reason) { onError(String(reason)) } }

  async function rebuildContainer(container: DshContainer): Promise<void> {
    try { await boxApi.enqueueContainerRebuild(container.id); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function deleteContainer(container: DshContainer): Promise<void> {
    if (!window.confirm(`Delete container ${container.id}?`)) return
    try { await boxApi.deleteContainer(container.id); await loadContainers(); setContainerMenuId(null); onError(null) } catch (reason) { onError(String(reason)) }
  }

  return {
    containers, setContainers, selectedContainer, setSelectedContainer, containerDetails, setContainerDetails,
    containerVersion, setContainerVersion, creatingContainer, creatingContainerView, setCreatingContainerView,
    containerName, setContainerName, newContainerProfile, setNewContainerProfile, containerMenuId, setContainerMenuId,
    workspaceExtensions, setWorkspaceExtensions, loadContainers, createContainer, showContainerDetails,
    refreshSelectedContainer, addContainerProfile, setContainerProfile, addContainerExtension,
    deleteContainerPlugin, scanWorkspaceExtensions, importWorkspaceExtension, readContainerLog,
    toggleContainer, openContainer, openContainerInBrowser, rebuildContainer, deleteContainer,
  }
}
