import { useState } from 'react'
import { Button } from '../../ui/Button'
import { Card } from '../../ui/Card'
import type { Text } from '../../i18n'

const INSTALL = 'pnpm add @nexus-aethra/dsh-box-sandbox'

/**
 * What DSH Box offers a DSH desktop install, and how it gets there.
 *
 * DSH (DeepSeek Harness) has its own desktop app; DSH Box is the thing that
 * runs Containers, Templates, plugins and resource copies. This pane is the
 * handshake between them: the sandbox plugin publishes those operations as
 * typed `box_*` tools, so an agent running in DSH can drive DSH Box without a
 * shell and without parsing a CLI's stdout. That is the service this pane is
 * about, which is why the install line comes first and the container mount
 * second -- the second is a convenience Box performs on its own.
 *
 * The install line is a real copyable command rather than prose with a package
 * name in it: a person reading a paragraph has to retype it correctly, and a
 * typo in a package name resolves to a different package or to nothing at all.
 *
 * Every count here is checked against what the plugin registers: ten page tools
 * in `tools.ts`, eleven box tools in `manage.ts`. The tool lists are named
 * individually rather than summarised, because "21 tools" tells a reader
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
          <p className="eyebrow">DSH</p>
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
        <p className="label">{text.agentGuideMountTitle}</p>
        <p className="agent-guide-note">{text.agentGuideMountNote}</p>
      </Card>

      <Card padding="md">
        <p className="label">{text.agentGuideSkillTitle}</p>
        <p className="agent-guide-note">{text.agentGuideSkillNote}</p>
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
