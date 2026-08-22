import { useEffect, useState } from 'react'
import { pickText } from '../i18n'
import { boxApi } from '../shared/api/box-api'
import type { BoxConfig, DshVersion, Language, ServerServiceStatus, ToolchainStatus } from '../shared/types/domain'

export const INITIAL_CONFIG: BoxConfig = { runtimeDirectory: null, selectedDshVersion: null, language: 'en', toolchainSources: {}, githubMirror: null, npmRegistry: null }

/** npm registry presets shown in Settings; `__custom__` reveals a free-form input. */
export const NPM_REGISTRY_PRESETS = [
  { value: '', label: 'Default (npmjs)' },
  { value: 'https://registry.npmmirror.com', label: 'npmmirror (China)' },
  { value: 'https://mirrors.cloud.tencent.com/npm/', label: 'Tencent Cloud' },
  { value: 'https://repo.huaweicloud.com/repository/npm/', label: 'Huawei Cloud' },
  { value: '__custom__', label: 'Custom…' },
]

export function useSettings(onError: (message: string | null) => void) {
  const [config, setConfig] = useState<BoxConfig>(INITIAL_CONFIG)
  const [loading, setLoading] = useState(true)
  const [toolchains, setToolchains] = useState<ToolchainStatus[]>([])
  const [detecting, setDetecting] = useState(false)
  const [expandedToolchain, setExpandedToolchain] = useState<string | null>(null)
  const [dshVersions, setDshVersions] = useState<DshVersion[]>([])
  const [loadingVersions, setLoadingVersions] = useState(false)
  const [installingVersion, setInstallingVersion] = useState<string | null>(null)
  const [installedDshVersions, setInstalledDshVersions] = useState<string[]>([])
  const [upgradingResources, setUpgradingResources] = useState(false)
  const [upgradeReport, setUpgradeReport] = useState<string[] | null>(null)
  const [githubMirror, setGithubMirror] = useState('')
  const [npmRegistry, setNpmRegistry] = useState('')
  const [npmRegistryCustom, setNpmRegistryCustom] = useState('')
  const [savingMirror, setSavingMirror] = useState(false)
  const [serverService, setServerService] = useState<ServerServiceStatus | null>(null)

  useEffect(() => {
    void boxApi.loadConfig().then(setConfig).catch((reason: unknown) => { onError(String(reason)) }).finally(() => { setLoading(false) })
    void boxApi.getServerServiceStatus().then(setServerService).catch(() => undefined)
    // Page-scoped loading: toolchains and DSH versions are fetched when the
    // user enters the section/tab that renders them (App.tsx `loadSection`),
    // not up front — the gate above only needs config + service status.
  }, [])

  // Keep the mirror settings form in sync when the saved config changes.
  useEffect(() => {
    setGithubMirror(config.githubMirror ?? '')
    setNpmRegistry(config.npmRegistry ?? '')
    setNpmRegistryCustom(config.npmRegistry ?? '')
  }, [config.githubMirror, config.npmRegistry])

  async function refreshToolchains(): Promise<void> {
    setDetecting(true)
    try { setToolchains(await boxApi.detectToolchains()) } catch (reason) { onError(String(reason)) } finally { setDetecting(false) }
  }

  async function loadDshVersions(): Promise<void> {
    setLoadingVersions(true)
    try {
      // The daemon derives the list from the template index — every entry
      // (including the implicit `latest` reference) is returned directly,
      // no client-side synthesis needed.
      setDshVersions(await boxApi.listDshVersions())
      onError(null)
    } catch (reason) { onError(String(reason)) } finally { setLoadingVersions(false) }
  }

  async function installDshVersion(version: string): Promise<void> {
    setInstallingVersion(version)
    try { await boxApi.pullTemplate(version); onError(null) } catch (reason) { onError(String(reason)) } finally { setInstallingVersion(null) }
  }

  async function refreshDshCatalog(): Promise<void> {
    try { await boxApi.enqueueDshCatalogRefresh(); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function uninstallDshVersion(version: string): Promise<void> {
    if (!window.confirm(`Uninstall DSH ${version}?`)) return
    try { setConfig(await boxApi.uninstallDshVersion(version)); await loadDshVersions(); onError(null) } catch (reason) { onError(String(reason)) }
  }

  async function loadInstalledDshVersions(): Promise<void> {
    try { const versions = await boxApi.listInstalledDshVersions(); setInstalledDshVersions(versions) } catch (reason) { onError(String(reason)) }
  }

  // One-shot migration: mirror every runtime clone into the template index.
// Older installs used a writer that left the runtime directory without a
// matching index entry; without this pass the Harness tab and the
// Container dropdown both miss those harnesses.
  async function upgradeResources(): Promise<void> {
    setUpgradingResources(true)
    try {
      const registered = await boxApi.upgradeLegacyResources()
      setUpgradeReport(registered)
      await loadDshVersions()
      onError(null)
    } catch (reason) { onError(String(reason)) } finally { setUpgradingResources(false) }
  }

  async function chooseRuntimeDirectory(): Promise<void> {
    try {
      const text = pickText(config.language)
      const selected = await boxApi.chooseDirectory(text.chooseTitle)
      if (selected === null || Array.isArray(selected)) return
      setConfig(await boxApi.saveRuntimeDirectory(selected))
      onError(null)
    } catch (reason) { onError(String(reason)) }
  }

  async function changeLanguage(language: Language): Promise<void> {
    try {
      setConfig(await boxApi.saveLanguage(language))
      onError(null)
    } catch (reason) { onError(String(reason)) }
  }

  async function saveGithubMirror(): Promise<void> {
    setSavingMirror(true)
    try {
      const registry = npmRegistry === '__custom__' ? npmRegistryCustom.trim() : npmRegistry
      const config = await boxApi.saveMirrorSettings(githubMirror.trim() || null, registry || null)
      setConfig(config)
      onError(null)
    } catch (reason) { onError(String(reason)) } finally { setSavingMirror(false) }
  }

  /// npm registry is a selection (not a free-form text field), so save it
  /// immediately — users do not want to pair a dropdown with a Save button.
  async function saveNpmRegistry(): Promise<void> {
    const registry = npmRegistry === '__custom__' ? npmRegistryCustom.trim() : npmRegistry
    if (registry === config.npmRegistry) return
    setSavingMirror(true)
    try {
      const config = await boxApi.saveMirrorSettings(githubMirror.trim() || null, registry || null)
      setConfig(config)
      onError(null)
    } catch (reason) { onError(String(reason)) } finally { setSavingMirror(false) }
  }

  async function restartServerService(): Promise<void> {
    try { await boxApi.restartServerService(); setServerService(await boxApi.getServerServiceStatus()); onError(null) } catch (reason) { onError(String(reason)) }
  }

  return {
    config, setConfig, loading, toolchains, detecting, expandedToolchain, setExpandedToolchain,
    dshVersions, loadingVersions, installingVersion, installedDshVersions, setInstalledDshVersions,
    upgradingResources, upgradeReport, upgradeResources,
    githubMirror, setGithubMirror, npmRegistry, setNpmRegistry, npmRegistryCustom, setNpmRegistryCustom,
    savingMirror, serverService, refreshToolchains, loadDshVersions, installDshVersion,
    refreshDshCatalog, uninstallDshVersion, loadInstalledDshVersions, chooseRuntimeDirectory,
    changeLanguage, saveGithubMirror, saveNpmRegistry, restartServerService,
  }
}
