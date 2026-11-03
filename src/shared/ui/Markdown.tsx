import { Fragment, type CSSProperties, type ReactNode } from 'react'

/**
 * A dependency-free Markdown renderer for release notes.
 *
 * A release body is remote, untrusted text, and this UI builds its widgets
 * from small primitives with no markdown dependency -- so this renders the
 * constructs a changelog actually uses (headings, lists, links, code, quotes,
 * emphasis) straight to React elements. Never `dangerouslySetInnerHTML`:
 * text is text, so a `<script>` in a note renders as visible characters --
 * the outcome a sanitizer aims for, reached by construction instead of by
 * filtering. Raw HTML renders as plain text. GFM pipe tables render as
 * tables, with the alignment the delimiter row asks for. Images render as
 * links: this pane has no business loading remote binaries.
 *
 * A line break inside a paragraph renders as a break, the way GitHub renders
 * comments -- changelog authors write one item per line and expect to see
 * exactly that.
 */

// --- inline ---------------------------------------------------------------
// Sticky regexes: every attempt pins lastIndex to the scan position, so a
// match can only start there. Sharing one lastIndex is safe because each
// attempt sets it immediately before its synchronous exec.

const RX_CODE = /(`+)([\s\S]*?)\1/y
const RX_ANGLE_URL = /<(https?:\/\/[^>\s]+)>/y
const RX_IMAGE = /!\[([^\]]*)\]\(\s*<?([^\s)>]+)>?(?:\s+"[^"]*")?\s*\)/y
const RX_LINK = /\[([^\]]*)\]\(\s*<?([^\s)>]+)>?(?:\s+"[^"]*")?\s*\)/y
const RX_URL = /https?:\/\/[^\s<>()\[\]{}"'\x60]+/y
const RX_STRONG = /\*\*(?=\S)([\s\S]*?\S)\*\*/y
const RX_STRONG_UNDERSCORE = /__(?=\S)([\s\S]*?\S)__/y
const RX_EM = /\*(?=\S)([^*\n]*\S)\*/y
const RX_EM_UNDERSCORE = /_(?=\S)([^_\n]*\S)_(?![\w`])/y
const RX_STRIKE = /~~(?=\S)([\s\S]*?\S)~~/y

/** Only http(s) is a link this pane will navigate to. */
function safeHref(url: string): string | null {
  return /^https?:\/\//i.test(url) ? url : null
}

/** Bare URLs swallow sentence punctuation and unbalanced closing parens. */
function trimUrlTail(url: string): string {
  let end = url.length
  while (end > 0) {
    const last = url[end - 1]
    if (/[,.;:!?\"'_]/.test(last)) { end -= 1; continue }
    if (last === ")") {
      const head = url.slice(0, end)
      if ((head.split("(").length - 1) < (head.split(")").length - 1)) { end -= 1; continue }
    }
    break
  }
  return url.slice(0, end)
}

const isWordChar = (ch: string) => /[\w`\\]/.test(ch)

function renderInline(src: string, key: string): ReactNode[] {
  const out: ReactNode[] = []
  let plain = ''
  let i = 0
  let n = 0
  const flush = () => { if (plain !== '') { out.push(plain); plain = '' } }
  const push = (node: ReactNode) => { flush(); out.push(<Fragment key={`${key}-i${n++}`}>{node}</Fragment>) }
  const tryMatch = (rx: RegExp): RegExpExecArray | null => {
    rx.lastIndex = i
    return rx.exec(src)
  }

  while (i < src.length) {
    const ch = src[i]
    // A backslash escapes the punctuation that would otherwise mark up text.
    if (ch === '\\' && i + 1 < src.length && /[-\\`*_\[\]()#+.!~>]/.test(src[i + 1])) {
      plain += src[i + 1]
      i += 2
      continue
    }

    let m = tryMatch(RX_CODE)
    if (m !== null) {
      push(<code>{m[2]}</code>)
      i += m[0].length
      continue
    }

    m = tryMatch(RX_ANGLE_URL)
    if (m === null) m = tryMatch(RX_IMAGE)
    if (m === null) m = tryMatch(RX_LINK)
    if (m !== null) {
      const href = safeHref(m[2])
      const label = m[1] !== '' ? renderInline(m[1], `${key}-a${n}`) : [m[2]]
      if (href === null) {
        // Not an address this pane navigates to: show the source as text.
        plain += m[0]
      } else {
        push(<a href={href} target="_blank" rel="noreferrer">{label}</a>)
      }
      i += m[0].length
      continue
    }

    m = tryMatch(RX_URL)
    if (m !== null) {
      const url = trimUrlTail(m[0])
      push(<a href={url} target="_blank" rel="noreferrer">{url}</a>)
      i += url.length
      continue
    }

    m = tryMatch(RX_STRONG) ?? tryMatch(RX_STRONG_UNDERSCORE)
    if (m === null) m = tryMatch(RX_EM)
    if (m === null && (i === 0 || !isWordChar(src[i - 1]))) m = tryMatch(RX_EM_UNDERSCORE)
    if (m === null) m = tryMatch(RX_STRIKE)
    if (m !== null) {
      const inner = renderInline(m[1], `${key}-e${n}`)
      if (m[0].startsWith('~~')) push(<del>{inner}</del>)
      else if (m[0].startsWith('**') || m[0].startsWith('__')) push(<strong>{inner}</strong>)
      else push(<em>{inner}</em>)
      i += m[0].length
      continue
    }

    plain += ch
    i += 1
  }
  flush()
  return out
}

// --- blocks ----------------------------------------------------------------
const RX_FENCE = /^ {0,3}(`{3,}|~{3,})/
const RX_HEADING = /^ {0,3}(#{1,6})\s+(.*?)\s*#*\s*$/
const RX_HR = /^ {0,3}([-*_])(?:\s*\1){2,}\s*$/
const RX_QUOTE = /^ {0,3}>/
const RX_ITEM = /^(\s*)([-*+]|\d{1,9}[.)])(\s+)/
const RX_TABLE_DELIM = /^\s*\|?\s*:?-{1,}:?\s*(?:\|\s*:?-{1,}:?\s*)*\|?\s*$/

/** A pipe row's cells: edge pipes trimmed, escaped pipes stitched back. */
function splitTableRow(row: string): string[] {
  let text = row.trim()
  if (text.startsWith('|')) text = text.slice(1)
  if (text.endsWith('|') && !text.endsWith('\\|')) text = text.slice(0, -1)
  const cells: string[] = []
  for (const piece of text.split('|')) {
    if (cells.length > 0 && cells[cells.length - 1].endsWith('\\')) {
      cells[cells.length - 1] = cells[cells.length - 1].slice(0, -1) + '|' + piece
    } else {
      cells.push(piece)
    }
  }
  return cells.map((cell) => cell.trim())
}

/** The alignment each delimiter cell asks for: `:--` left, `:--:` center, `--:` right. */
function tableAligns(delimiterRow: string): ('left' | 'center' | 'right')[] {
  return splitTableRow(delimiterRow).map((cell) => {
    const left = cell.startsWith(':')
    const right = cell.endsWith(':')
    if (left && right) return 'center' as const
    if (right) return 'right' as const
    return 'left' as const
  })
}

function renderTable(header: string[], aligns: ('left' | 'center' | 'right')[], rows: string[][], key: string): ReactNode {
  const cellStyle = (index: number): CSSProperties | undefined =>
    aligns[index] === undefined || aligns[index] === 'left' ? undefined : { textAlign: aligns[index] }
  return (
    <table key={key}>
      <thead>
        <tr>
          {header.map((cell, index) => (
            <th key={index} style={cellStyle(index)}>{renderInline(cell, `${key}-th${index}`)}</th>
          ))}
        </tr>
      </thead>
      <tbody>
        {rows.map((row, rowIndex) => (
          <tr key={rowIndex}>
            {header.map((_, index) => (
              <td key={index} style={cellStyle(index)}>{renderInline(row[index] ?? '', `${key}-td${rowIndex}-${index}`)}</td>
            ))}
          </tr>
        ))}
      </tbody>
    </table>
  )
}

function parseBlocks(src: string, key: string): ReactNode[] {
  const lines = src.replace(/\r\n?/g, '\n').split('\n')
  const out: ReactNode[] = []
  let i = 0
  let n = 0
  const k = () => `${key}-b${n++}`
  const isBlockStart = (line: string) =>
    line.trim() === '' ||
    RX_FENCE.test(line) ||
    RX_HEADING.test(line) ||
    RX_HR.test(line) ||
    RX_QUOTE.test(line) ||
    RX_ITEM.test(line)
  // A table starts at a pipe line whose next line is a delimiter row -- and
  // only when the cell counts agree, so a prose line over a thematic break
  // stays a paragraph over an <hr>.
  const isTableStart = (index: number) =>
    lines[index].includes('|') &&
    index + 1 < lines.length &&
    RX_TABLE_DELIM.test(lines[index + 1]) &&
    splitTableRow(lines[index]).length === splitTableRow(lines[index + 1]).length

  while (i < lines.length) {
    const line = lines[i]
    if (line.trim() === '') { i += 1; continue }

    const fence = RX_FENCE.exec(line)
    if (fence !== null) {
      const marker = fence[1]
      const close = new RegExp(`^ {0,3}\\${marker[0]}{${marker.length},}\\s*$`)
      const body: string[] = []
      i += 1
      while (i < lines.length && !close.test(lines[i])) { body.push(lines[i]); i += 1 }
      i += 1 // the closing fence -- or the end of the text
      out.push(<pre key={k()}><code>{body.join('\n')}</code></pre>)
      continue
    }

    const heading = RX_HEADING.exec(line)
    if (heading !== null) {
      const level = heading[1].length as 1 | 2 | 3 | 4 | 5 | 6
      const Tag = `h${level}` as 'h1' | 'h2' | 'h3' | 'h4' | 'h5' | 'h6'
      out.push(<Tag key={k()}>{renderInline(heading[2], `${key}-h`)}</Tag>)
      i += 1
      continue
    }

    if (RX_HR.test(line)) { out.push(<hr key={k()} />); i += 1; continue }

    if (RX_QUOTE.test(line)) {
      const body: string[] = []
      while (i < lines.length && RX_QUOTE.test(lines[i])) {
        body.push(lines[i].replace(/^ {0,3}> ?/, ''))
        i += 1
      }
      out.push(<blockquote key={k()}>{parseBlocks(body.join('\n'), `${key}-q`)}</blockquote>)
      continue
    }

    if (isTableStart(i)) {
      const header = splitTableRow(line)
      const aligns = tableAligns(lines[i + 1])
      const rows: string[][] = []
      i += 2
      while (i < lines.length && lines[i].trim() !== '' && lines[i].includes('|')) {
        rows.push(splitTableRow(lines[i]))
        i += 1
      }
      out.push(renderTable(header, aligns, rows, k()))
      continue
    }

    const first = RX_ITEM.exec(line)
    if (first !== null) {
      const ordered = /\d/.test(first[2])
      const baseIndent = first[1].length
      const markerWidth = first[2].length + first[3].length
      const items: string[][] = []
      while (i < lines.length) {
        const cur = RX_ITEM.exec(lines[i])
        if (cur !== null && cur[1].length <= baseIndent) {
          items.push([lines[i].slice(cur[0].length)])
          i += 1
          continue
        }
        if (items.length === 0 || lines[i].trim() === '' || /^\S/.test(lines[i])) break
        // A deeper-indented line continues the item: a nested list, or text
        // the author wrapped. Strip back to the item column first.
        const actual = lines[i].length - lines[i].trimStart().length
        const strip = Math.min(actual, baseIndent + markerWidth)
        items[items.length - 1].push(lines[i].slice(strip))
        i += 1
      }
      const children = items.map((raw, index) => (
        <li key={`${key}-li${index}`}>{parseBlocks(raw.join("\n"), `${key}-li${index}`)}</li>
      ))
      out.push(ordered ? <ol key={k()}>{children}</ol> : <ul key={k()}>{children}</ul>)
      continue
    }

    const para: string[] = []
    while (i < lines.length && !isBlockStart(lines[i]) && !isTableStart(i)) {
      para.push(lines[i].trim())
      i += 1
    }
    out.push(
      <p key={k()}>
        {para.map((entry, index) => (
          <Fragment key={`${key}-p${index}`}>
            {index > 0 && <br />}
            {renderInline(entry, `${key}-p${index}`)}
          </Fragment>
        ))}
      </p>
    )
  }
  return out
}

export type MarkdownProps = {
  source: string
}

export function Markdown({ source }: MarkdownProps) {
  return <div className="md">{parseBlocks(source, 'md')}</div>
}
