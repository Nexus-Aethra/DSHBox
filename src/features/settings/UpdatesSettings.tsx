import { useEffect, useState } from 'react'
import { boxApi } from '../../shared/api/box-api'
import { useUpdates } from '../../state/useUpdates'
import type { UpdatePolicy, UpdateReleaseSummary } from '../../shared/types/domain'
import { Button } from '../../ui/Button'
import { Card } from '../../ui/Card'
import { Markdown } from '../../shared/ui/Markdown'
import { Stack } from '../../ui/Stack'

export type UpdatesText = {
  updatesTitle: string
  updatesNote: string
  updatesCurrent: string
  updatesNav: string
  updatesVersionsTitle: string
  updatesVersionsNote: string
  updatesVersionsLoading: string
  updatesVersionsFailed: (error: string) => string
  updatesVersionCurrent: string
  updatesVersionNewer: string
  updatesVersionOlder: string
  updatesNotesEmpty: string
  updatesUpgradeTo: string
  updatesUpdateRow: string
  updatesPolicy: string
  updatesPolicyNote: string
  updatesPolicyOff: string
  updatesPolicyCheck: string
  updatesPolicyDownload: string
  updatesPolicyAuto: string
  updatesPolicyOffNote: string
  updatesPolicyCheckNote: string
  updatesPolicyDownloadNote: string
  updatesPolicyAutoNote: string
  updatesCheck: string
  updatesChecking: string
  updatesAvailable: (version: string) => string
  updatesNeverChecked: string
  updatesLastChecked: (when: string) => string
  updatesDownloading: (stage: string, progress: number) => string
  updatesReady: (version: string) => string
  updatesReadyNote: (path: string) => string
  updatesSize: (megabytes: string) => string
  updatesOpenFolder: string
  updatesInstall: string
  updatesInstallNote: string
  updatesDiscard: string
  updatesSkip: string
  updatesUnskip: string
  updatesManual: string
  updatesManualReason: string
  updatesBelowMinimum: string
  updatesManifestUnavailable: string
  updatesManualCommand: (path: string) => string
  updatesReleaseNotes: string
  updatesInterval: (hours: number) => string
}

type Props = {
  text: UpdatesText
  onError: (message: string | null) => void
}

const POLICIES: UpdatePolicy[] = ['off', 'check', 'download', 'auto']

type RowState = 'current' | 'newer' | 'older'

/**
 * The Updates pane, as one card: the current version line, and behind the
 * fold the version history -- pick a release, read what it changed, and
 * update to it from the same panel. Plus the policy card.
 *
 * The history is read-only as history: only the newest release carries a
 * manifest, so only it can be installed from here; an older row links out to
 * its release page instead of pretending a downgrade is one verified click
 * away.
 */
export function UpdatesSettings({ text, onError }: Props) {
  const updates = useUpdates(onError)
  const [installing, setInstalling] = useState(false)
  const [releases, setReleases] = useState<UpdateReleaseSummary[] | null>(null)
  const [releasesError, setReleasesError] = useState<string | null>(null)
  const [expanded, setExpanded] = useState(false)
  const [selected, setSelected] = useState<string | null>(null)
  const state = updates.state

  useEffect(() => {
    let alive = true
    boxApi.listUpdateReleases()
      .then((answer) => { if (alive) setReleases(answer.releases) })
      .catch((reason) => { if (alive) setReleasesError(String(reason)) })
    return () => { alive = false }
  }, [])

  if (state === null) {
    return (
      <Card>
        <p className="label">{text.updatesTitle}</p>
        <p className="path">{text.updatesNote}</p>
      </Card>
    )
  }

  const policy = state.policy
  const policyLabels: Record<UpdatePolicy, string> = {
    off: text.updatesPolicyOff,
    check: text.updatesPolicyCheck,
    download: text.updatesPolicyDownload,
    auto: text.updatesPolicyAuto,
  }
  const policyNotes: Record<UpdatePolicy, string> = {
    off: text.updatesPolicyOffNote,
    check: text.updatesPolicyCheckNote,
    download: text.updatesPolicyDownloadNote,
    auto: text.updatesPolicyAutoNote,
  }

  const available = state.available
  const downloading = updates.downloading
  const download = state.download
  const skipped = state.skippedVersion
  // A skipped release stays visible and stays installable -- it just does not
  // announce itself. "I do not want this one" is not "hide the feature", and
  // the badge reads the same flag.
  const isSkipped = available !== null && skipped !== null && available.version === skipped
  const newer = available !== null && available.newer
  const showUpdate = newer && !isSkipped
  // A release that predates the manifest has no digest, so there is nothing to
  // verify and nothing this app will run. Say that, and point at the release.
  const manualReason = available?.manualReason ?? null
  const canDownload = showUpdate && download === null && downloading === null && available?.asset !== null
  const megabytes = (bytes: number) => (bytes / (1024 * 1024)).toFixed(1)

  // Why a release cannot be installed from here, in one honest line: the
  // reasons are different facts (too old to replace in place, a manifest
  // that could not be read just now, an installer that needs root).
  const manualReasonText = (reason: string | null) => reason === 'below-minimum'
    ? text.updatesBelowMinimum
    : reason === 'manifest-unavailable'
      ? text.updatesManifestUnavailable
      : text.updatesManualReason

  const rowStateOf = (version: string): RowState => version === state.currentVersion
    ? 'current'
    : newer && available !== null && version === available.version
      ? 'newer'
      : 'older'
  // The detail panel opens on the newest release by default -- that is the one
  // a person came here to read about -- and stays on whatever was picked last.
  const selectedVersion = selected
    ?? (releases !== null && releases.length > 0 ? releases[0].version : null)
  const selectedRelease = releases?.find((release) => release.version === selectedVersion) ?? null
  const selectedIsUpdatable = selectedRelease !== null
    && showUpdate && selectedRelease.version === available?.version

  const install = async () => {
    if (download === null) return
    // Ending every running container is part of this, so it is stated first.
    // A native dialog, not window.confirm: the webview no-ops confirm on
    // macOS, which would turn this button into a silent nothing.
    if (!(await boxApi.confirmDialog(text.updatesInstallNote))) return
    setInstalling(true)
    try {
      await boxApi.installUpdate(download.path)
      // The desktop closes this process before the installer runs, so there is
      // normally no chance to reach the next line.
      onError(null)
    } catch (reason) {
      onError(String(reason))
      setInstalling(false)
    }
  }

  return (
    <Stack gap={4}>
      <Card>
        <div className="settings-row">
          <div>
            <p className="label">{text.updatesCurrent} {state.currentVersion}</p>
            <p className="path">
              {state.checkedAt === null
                ? text.updatesNeverChecked
                : text.updatesLastChecked(new Date(state.checkedAt * 1000).toLocaleString())}
              {state.lastError !== null && <span className="error"> · {state.lastError}</span>}
            </p>
          </div>
          <Button
            variant="secondary"
            size="sm"
            loading={updates.checking}
            disabled={updates.checking || updates.busy}
            onClick={() => { void updates.check() }}
          >
            {updates.checking ? text.updatesChecking : text.updatesCheck}
          </Button>
        </div>

        {downloading !== null && (
          <div className="settings-row">
            <div>
              <p className="label">{text.updatesDownloading(downloading.stage, downloading.progress)}</p>
              <progress value={downloading.progress} max={100} />
            </div>
          </div>
        )}

        <div className="settings-row fold-row">
          <button type="button" className="fold-toggle" aria-expanded={expanded} onClick={() => { setExpanded((was) => !was) }}>
            <span className={expanded ? 'fold-arrow open' : 'fold-arrow'}>›</span>
            {text.updatesVersionsTitle}
            {releases !== null && <span className="fold-count">{releases.length}</span>}
          </button>
        </div>
        {expanded && (
          <div className="version-block">
            <p className="path">{text.updatesVersionsNote}</p>
            {releasesError !== null && (
              <p className="error" role="alert">{text.updatesVersionsFailed(releasesError)}</p>
            )}
            {releases === null && releasesError === null && (
              <p className="path">{text.updatesVersionsLoading}</p>
            )}
            {releases !== null && (
              <div className="version-list">
                {releases.map((release) => {
                  const rowState = rowStateOf(release.version)
                  const isPicked = release.version === selectedVersion
                  // The row button installs only the release the manifest
                  // makes installable from here; every other row keeps it
                  // grey instead of pretending a downgrade is one click away.
                  const rowUpdateable = rowState === 'newer' && available !== null && available.asset !== null
                  return (
                    <div
                      key={release.tag}
                      role="button"
                      tabIndex={0}
                      className={isPicked ? 'version-head picked' : 'version-head'}
                      onClick={() => { setSelected(release.version) }}
                      onKeyDown={(event) => {
                        if (event.key === 'Enter' || event.key === ' ') {
                          event.preventDefault()
                          setSelected(release.version)
                        }
                      }}
                    >
                      <span className="version-name">
                        {release.version}
                        {release.prerelease && <span className="version-prerelease">pre</span>}
                      </span>
                      <span className={'version-badge ' + rowState}>
                        {rowState === 'current'
                          ? text.updatesVersionCurrent
                          : rowState === 'newer'
                            ? text.updatesVersionNewer
                            : text.updatesVersionOlder}
                      </span>
                      <span className="version-update">
                        <Button
                          variant="primary"
                          size="sm"
                          disabled={!rowUpdateable || updates.busy}
                          onClick={(event) => { event.stopPropagation(); void updates.download() }}
                        >
                          {text.updatesUpdateRow}
                        </Button>
                      </span>
                    </div>
                  )
                })}
              </div>
            )}
            {selectedRelease !== null && (
              <div className="version-detail">
                <div className="version-detail-head">
                  <span className="version-name">
                    {(() => {
                      const rowState = rowStateOf(selectedRelease.version)
                      return rowState === 'current'
                        ? text.updatesCurrent
                        : rowState === 'newer'
                          ? text.updatesVersionNewer
                          : text.updatesVersionOlder
                    })()}
                    {' '}{selectedRelease.version}
                  </span>
                  {selectedRelease.publishedAt !== null && (
                    <span className="version-date">{new Date(selectedRelease.publishedAt).toLocaleDateString()}</span>
                  )}
                  {selectedIsUpdatable && canDownload && (
                    <Button variant="primary" size="sm" disabled={updates.busy} onClick={() => { void updates.download() }}>
                      {text.updatesUpgradeTo}
                    </Button>
                  )}
                  {selectedIsUpdatable && isSkipped && (
                    <Button variant="ghost" size="sm" disabled={updates.busy} onClick={() => { void updates.unskip() }}>
                      {text.updatesUnskip}
                    </Button>
                  )}
                  {selectedIsUpdatable && !isSkipped && download === null && downloading === null && (
                    <Button variant="ghost" size="sm" disabled={updates.busy} onClick={() => { void updates.skip() }}>
                      {text.updatesSkip}
                    </Button>
                  )}
                  {selectedRelease.notesUrl !== null && (
                    <a href={selectedRelease.notesUrl} target="_blank" rel="noreferrer">{text.updatesReleaseNotes}</a>
                  )}
                </div>
                {newer && selectedIsUpdatable && manualReason !== null && (
                  <p className="path">{manualReasonText(manualReason)}</p>
                )}
                {download !== null && selectedRelease.version === download.version && (
                  <p className="path">{text.updatesReadyNote(download.path)} · {text.updatesSize(megabytes(download.bytes))}</p>
                )}
                <div className="version-notes">
                  {selectedRelease.notes?.trim()
                    ? <Markdown source={selectedRelease.notes} />
                    : <p className="md-empty">{text.updatesNotesEmpty}</p>}
                </div>
              </div>
            )}
          </div>
        )}

        {download !== null && (
          <div className="settings-row">
            <div>
              <p className="label">{text.updatesReady(download.version)}</p>
              <p className="path">{text.updatesReadyNote(download.path)}</p>
              <p className="path">{text.updatesSize(megabytes(download.bytes))}</p>
              {state.available?.asset?.installMode === 'manual' && (
                <p className="path">{text.updatesManualCommand(download.path)}</p>
              )}
              {manualReason !== null && state.available?.asset?.installMode !== 'manual' && (
                <p className="path">{manualReasonText(manualReason)}</p>
              )}
            </div>
            <div className="settings-controls">
              <Button variant="ghost" size="sm" onClick={() => { void boxApi.openUpdateFolder(download.path) }}>
                {text.updatesOpenFolder}
              </Button>
              {state.available?.asset?.installMode === 'manual'
                // No digest-less install, and no silent sudo from a GUI: the
                // path and the command are the honest half of a manual update.
                ? <p className="path">{text.updatesManual}</p>
                : manualReason === null && <Button variant="primary" size="sm" loading={installing} disabled={installing || updates.busy} onClick={() => { void install() }}>
                  {text.updatesInstall}
                </Button>}
              <Button variant="ghost" size="sm" disabled={updates.busy || installing} onClick={() => { void updates.discard() }}>
                {text.updatesDiscard}
              </Button>
            </div>
          </div>
        )}
      </Card>

      <Card>
        <div className="settings-row">
          <div>
            <p className="label">{text.updatesPolicy}</p>
            <p className="path">{text.updatesPolicyNote}</p>
          </div>
        </div>
        <div className="settings-row">
          <div>
            <p className="label">{policyLabels[policy]}</p>
            <p className="path">{policyNotes[policy]}</p>
            {policy !== 'off' && (
              <p className="path">{text.updatesInterval(Math.round(state.checkIntervalSeconds / 3600))}</p>
            )}
          </div>
          <div className="segmented" role="radiogroup" aria-label={text.updatesPolicy}>
            {POLICIES.map((entry) => (
              <button
                key={entry}
                type="button"
                role="radio"
                aria-checked={entry === policy}
                className={entry === policy ? 'segment selected' : 'segment'}
                disabled={updates.busy}
                onClick={() => { void updates.setPolicy(entry) }}
              >
                {policyLabels[entry]}
              </button>
            ))}
          </div>
        </div>
      </Card>
    </Stack>
  )
}
