import { useState } from 'react'
import { Button } from '../../ui/Button'
import { Card } from '../../ui/Card'
import type { Text } from '../../i18n'

const INSTALL = 'pnpm add @nexus-aethra/dsh-box-sandbox'

/**
 * How to put the agent surface inside a DSH desktop profile.
 *
 * The install line is the whole point of this pane, so it is a real copyable
 * command rather than prose with a package name in it -- a person reading a
 * paragraph has to retype it correctly, and a typo in a package name resolves
 * to a different package or to nothing at all.
 *
 * Every claim here is checked against the plugin as published: 20 tools, nine
 * for the page and eleven for the box itself. The tool lists are named
 * individually rather than summarised, because "20 tools" tells a reader
 * nothing about whether the one they need is there.
 */
export function AgentGuide({ text }: { text: Text }) {
  const [copied, setCopied] = useState(false)

  async function copy(): Promise<void> {
    try {
      await navigator.clipboard.writeText(INSTALL)
      setCopied(true)
      window.setTimeout(() => setCopied(false), 2000)
    } catch {
      // A clipboard the page was not granted is not worth an error banner: the
      // command is on screen and selectable either way.
    }
  }

  return (
    <div className="agent-guide">
      <div className="workspace-heading">
        <div>
          <p className="eyebrow">AGENT</p>
          <h2>{text.agentGuideTitle}</h2>
        </div>
      </div>

      <Card padding="md">
        <p className="agent-guide-lede">{text.agentGuideIntro}</p>
      </Card>

      <Card padding="md">
        <p className="label">{text.agentGuideInstallTitle}</p>
        <p className="agent-guide-note">{text.agentGuideInstallViaBox}</p>
        <div className="install-command">
          <code>{INSTALL}</code>
          <Button variant="secondary" size="sm" onClick={() => { void copy() }}>{copied ? text.agentGuideCopied : text.agentGuideCopy}</Button>
        </div>
        <p className="agent-guide-note">{text.agentGuideInstallNote}</p>
      </Card>

      <Card padding="md">
        <p className="label">{text.agentGuideToolsTitle}</p>
        <dl className="tool-groups">
          <dt>{text.agentGuideToolsPage}</dt>
          <dd className="tool-names">{text.agentGuideToolsPageNote}</dd>
          <dt>{text.agentGuideToolsManage}</dt>
          <dd className="tool-names">{text.agentGuideToolsManageNote}</dd>
        </dl>
      </Card>
    </div>
  )
}
