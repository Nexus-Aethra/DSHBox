// Generate `latest.json`, the release manifest the updater's *full* tier reads.
//
// `docs/specs/auto-update.md` is the contract and `box-updates::manifest` is the
// parser; both agree on camelCase keys and on a platform's value being an
// `array`, because Linux publishes both a `.deb` and an `.rpm` for one
// machine. A release without this asset is a release the app can see but
// cannot install, and a release with a wrong digest is worse than that: the
// digest is what makes "the bytes I fetched are the bytes CI built" checkable
// after the transfer.
//
// Everything here is derived from bytes on disk. The digest is computed while
// streaming the file CI is about to upload, and the filename comes from the
// staged artifact directory, because the bundler decides the name (it carries
// the version, and on Linux the target triple). A hardcoded filename would
// publish a URL that 404s the day Tauri renames an installer.
//
// Usage:
//   node scripts/release-manifest.mjs --installers installers --version "$GITHUB_REF_NAME" \
//     --repo "$GITHUB_REPOSITORY" [--notes "<paragraph>"] [--notes-file <path>] \
//     [--language en|zh] [--minimum-version <v>] [--released-at <iso8601>] > latest.json
//   node scripts/release-manifest.mjs --check latest.json --version "$GITHUB_REF_NAME" --repo "$GITHUB_REPOSITORY"
//   node scripts/release-manifest.mjs --self-test
//
// `--check` re-reads a manifest that was just written (a redirect can leave an
// empty file, and a JSON that never parsed never reaches the release) and
// reports every structural problem it finds, failing if there are any.
// `--self-test` needs no fixture checked in: it builds the fake installers tree
// in a temp directory and exits non-zero on the first broken assertion.

import { createHash } from 'node:crypto'
import { createReadStream } from 'node:fs'
import { mkdtemp, mkdir, readFile, readdir, rm, stat, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { basename, dirname, extname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

// The repository this app releases from. Overridable for a fork, but the
// updater reads the published asset from the canonical repository, so CI
// passes `${{ github.repository }}` and a fork must pass its own.
const REPOSITORY = 'Nexus-Aethra/DSHBox'

const MANIFEST_SCHEMA = 1
const MANIFEST_ASSET_NAME = 'latest.json'

// One installer kind per platform. `rank` is the published order within a
// platform, which the manifest documents as the preference order: Linux lists
// `.deb` before `.rpm`. The Rust selector prefers the caller's package
// manager when it has a preference and otherwise takes the first entry, so a
// stable order is what keeps a re-run from changing which file a machine with
// neither preference picks.
const INSTALLER_KINDS = {
  msi: { platform: 'win-x64', installer: 'msi', installMode: 'auto', rank: 0 },
  deb: { platform: 'linux-x64', installer: 'deb', installMode: 'manual', rank: 0 },
  rpm: { platform: 'linux-x64', installer: 'rpm', installMode: 'manual', rank: 1 },
  dmg: { platform: 'macos-arm64', installer: 'dmg', installMode: 'auto', rank: 0 },
}

const EXTENSION_TO_KIND = {
  '.msi': 'msi',
  '.deb': 'deb',
  '.rpm': 'rpm',
  '.dmg': 'dmg',
}

// The platform keys are `bundled_target()` names, and the manifest is written
// in this order so two runs of the same tag produce the same bytes.
const PLATFORM_ORDER = ['win-x64', 'linux-x64', 'macos-arm64']

const NOTES_MAX_LENGTH = 300

const VERSION_PATTERN = /^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$/
const DIGEST_PATTERN = /^[0-9a-f]{64}$/

const USAGE = `Generate the updater's latest.json release manifest.

  --installers <dir>       directory the release job staged the artifacts in (default: installers)
  --version <v>            release version, with or without a leading 'v'
  --repo <owner/repo>      GitHub repository the release lives in (default: ${REPOSITORY})
  --notes <text>           a one-paragraph release summary
  --notes-file <path>      read the summary from a notes file (e.g. docs/releases/v0.2.0.md)
  --minimum-version <v>    oldest build allowed to update in place (default: the version itself)
  --language <code>        which notes section to summarize: en (default) or zh
  --released-at <iso8601>  release time (default: now), normalized to ISO-8601 UTC
  --check <path>           parse and validate an existing manifest instead of generating one
  --self-test              build a fake installers tree in a temp dir and assert the contract
  --help                   print this message

The manifest is printed to stdout; diagnostics go to stderr. An empty --notes-file
value means "no notes file", which is what CI passes when the tag has no notes.`

function fail(message) {
  throw new Error(message)
}

// ---------------------------------------------------------------------------
// Versions
// ---------------------------------------------------------------------------

// A tag is `v0.1.9` and the manifest carries `0.1.9`: the 'v' is a tag
// convention, not part of the version the updater compares.
function normalizeVersion(input) {
  const text = String(input ?? '').trim()
  if (!text) fail('a version is required (for example --version v0.2.0)')
  const withoutTag = /^[vV]/.test(text) ? text.slice(1) : text
  if (!VERSION_PATTERN.test(withoutTag)) {
    fail(`not a release version: ${text} (expected something like v0.2.0 or 0.2.0-rc.1)`)
  }
  return withoutTag
}

// ISO-8601 UTC, seconds precision: the spec's example is
// "2026-11-02T00:00:00Z", and a millisecond field the consumer never reads
// is noise in a file a person is meant to be able to read.
function normalizeReleasedAt(input) {
  const text = String(input ?? '').trim()
  const date = text ? new Date(text) : new Date()
  if (Number.isNaN(date.getTime())) fail(`not an ISO-8601 timestamp: ${text}`)
  return date.toISOString().replace(/\.\d{3}Z$/, 'Z')
}

// ---------------------------------------------------------------------------
// Release notes
// ---------------------------------------------------------------------------

// Release notes are written for people in `docs/releases/<tag>.md`, and they
// are bilingual: a `## 简体中文` section first, then `## English` (v0.1.9's notes
// do exactly this). The manifest carries one short string and the app's UI
// defaults to English, so the string comes from the section named by
// `language`. A notes file that is not bilingual -- or that has no section for
// this language -- falls back to the first prose paragraph in the file rather
// than to nothing, because an untranslated release still has notes.
const LANGUAGE_SECTIONS = {
  en: /english/i,
  zh: /中文|chinese/i,
}

// `en-US`, `zh_Hans`, `EN` -- a regional tag is the same language for the
// purpose of picking a section, so match on the base tag only.
function languagePattern(language) {
  const base = String(language ?? '').trim().toLowerCase().split(/[-_]/)[0]
  return LANGUAGE_SECTIONS[base] ?? null
}

function summarizeNotes(markdown, maxLength = NOTES_MAX_LENGTH, language = 'en') {
  const paragraphs = proseParagraphs(markdown)
  const wanted = languagePattern(language)
  if (wanted) {
    const match = paragraphs.find((paragraph) => wanted.test(paragraph.section))
    if (match) return truncate(match.text, maxLength)
  }
  return paragraphs.length ? truncate(paragraphs[0].text, maxLength) : ''
}

// Every prose paragraph in reading order, each tagged with the top-level
// section it sits under. Headings, horizontal rules and the
// `**简体中文** · [English](#english)` switcher are not prose, and a `###`
// subheading does not change which language section a paragraph belongs to.
function proseParagraphs(markdown) {
  const paragraphs = []
  let section = ''
  const blocks = String(markdown ?? '').replace(/\r\n/g, '\n').split(/\n\s*\n/)
  for (const block of blocks) {
    const lines = block.split('\n').map((line) => line.trim()).filter(Boolean)
    if (!lines.length) continue
    if (lines.every((line) => /^[-=*_]{3,}$/.test(line))) continue
    const heading = /^(#{1,6})\s+(.+)$/.exec(lines[0])
    if (heading) {
      if (heading[1].length <= 2) section = collapseMarkup(heading[2])
      continue
    }
    if (lines.every((line) => /^\*\*.+\*\*\s*·/.test(line))) continue
    const text = collapseMarkup(lines.join(' '))
    if (text) paragraphs.push({ section, text })
  }
  return paragraphs
}

// Strip the markup that a one-paragraph summary cannot keep: links become
// their text, images disappear with their alt text, emphasis markers and
// list bullets are typography rather than meaning, and a wrapped source line
// is one space in the string the app shows.
function collapseMarkup(text) {
  return String(text)
    .replace(/!\[[^\]]*\]\([^)]*\)/g, ' ')
    .replace(/\[([^\]]+)\]\([^)]*\)/g, '$1')
    .replace(/```/g, ' ')
    .replace(/`([^`]+)`/g, '$1')
    .replace(/\*\*(.+?)\*\*/g, '$1')
    .replace(/\*(.+?)\*/g, '$1')
    .replace(/\s+/g, ' ')
    .replace(/^[-*+]\s+/, '')
    .replace(/^\d+\.\s+/, '')
    .trim()
}

function truncate(text, maxLength) {
  if (text.length <= maxLength) return text
  let cut = text.slice(0, maxLength - 1)
  const space = cut.lastIndexOf(' ')
  // Break on a word when the sentence has spaces; scripts without them
  // (Chinese, the notes' first language) are cut at the limit.
  if (space > cut.length / 2) cut = cut.slice(0, space)
  return `${cut.trimEnd()}…`
}

// ---------------------------------------------------------------------------
// The installers on disk
// ---------------------------------------------------------------------------

// Every regular file under the directory, sorted. The artifact bundles keep
// the bundler's own layout -- Linux arrives as `deb/` and `rpm/` children
// beside the Windows MSI -- so a flat glob would miss half of Linux.
async function walkFiles(directory) {
  const found = []
  const visit = async (current) => {
    const entries = await readdir(current, { withFileTypes: true })
    for (const entry of entries) {
      const path = join(current, entry.name)
      if (entry.isDirectory()) await visit(path)
      else if (entry.isFile()) found.push(path)
    }
  }
  await visit(directory)
  return found.sort()
}

function installerKindFor(path) {
  return EXTENSION_TO_KIND[extname(path).toLowerCase()] ?? null
}

async function collectInstallers(directory) {
  let stats
  try {
    stats = await stat(directory)
  } catch {
    fail(`installers directory not found: ${directory}`)
  }
  if (!stats.isDirectory()) fail(`not a directory: ${directory}`)

  const installers = []
  const skipped = []
  for (const path of await walkFiles(directory)) {
    const kind = installerKindFor(path)
    if (!kind) {
      // `latest.json` from a previous run and a checksum file are not
      // installers; a release job that re-runs must not describe them.
      skipped.push(path)
      continue
    }
    installers.push({ path, assetName: basename(path), kind })
  }

  if (!installers.length) {
    fail(
      `no installers found under ${directory}; refusing to publish a manifest that describes nothing`,
    )
  }

  // A GitHub release stores assets by name, so two installers sharing a
  // basename would upload as one asset and leave the manifest pointing at
  // bytes that were overwritten.
  const byName = new Map()
  for (const installer of installers) {
    const previous = byName.get(installer.assetName)
    if (previous) {
      fail(
        `two installers would upload as the same asset name ${installer.assetName}: ${previous.path} and ${installer.path}`,
      )
    }
    byName.set(installer.assetName, installer)
  }

  return { installers, skipped }
}

// Hash while streaming, and count the same bytes that were hashed: a `stat`
// taken separately could disagree with the digest if the file moved under us,
// and the manifest's size is what the downloader checks its progress against.
function digestFile(path) {
  return new Promise((resolvePromise, rejectPromise) => {
    const hash = createHash('sha256')
    let size = 0
    const stream = createReadStream(path)
    stream.on('data', (chunk) => {
      hash.update(chunk)
      size += chunk.length
    })
    stream.on('error', rejectPromise)
    stream.on('end', () => resolvePromise({ sha256: hash.digest('hex'), size }))
  })
}

// ---------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------

function assetUrlBase(repo, tag) {
  return `https://github.com/${repo}/releases/download/${tag}/`
}

function assetUrl(repo, tag, assetName) {
  return `${assetUrlBase(repo, tag)}${assetName}`
}

function notesUrl(repo, tag) {
  return `https://github.com/${repo}/releases/tag/${tag}`
}

async function buildManifest(options) {
  const version = normalizeVersion(options.version)
  const repo = String(options.repo ?? REPOSITORY).trim().replace(/\/+$/, '')
  if (!/^[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/.test(repo)) {
    fail(`not a GitHub repository: ${repo} (expected owner/repo)`)
  }
  const tag = `v${version}`

  const { installers, skipped } = await collectInstallers(options.installers ?? 'installers')
  for (const path of skipped) console.error(`release-manifest: skipping non-installer ${path}`)

  // Grouped by platform, then ordered by the kind's rank and the asset name,
  // so the published order is the same on every runner.
  const groups = new Map(PLATFORM_ORDER.map((platform) => [platform, []]))
  for (const installer of installers) {
    const kind = INSTALLER_KINDS[installer.kind]
    const { sha256, size } = await digestFile(installer.path)
    if (!size) fail(`installer is empty: ${installer.path}`)
    groups.get(kind.platform).push({
      rank: kind.rank,
      asset: {
        url: assetUrl(repo, tag, installer.assetName),
        sha256,
        size,
        installer: kind.installer,
        installMode: kind.installMode,
      },
    })
  }

  const platforms = {}
  for (const platform of PLATFORM_ORDER) {
    const entries = groups.get(platform)
    // A platform whose build produced nothing must not appear: its installer
    // does not exist, so a manifest entry for it would only fail the download.
    if (!entries.length) continue
    entries.sort(
      (left, right) =>
        left.rank - right.rank || (left.asset.url < right.asset.url ? -1 : 1),
    )
    platforms[platform] = entries.map((entry) => entry.asset)
  }

  const notes = await resolveNotes(options)
  const manifest = {
    schema: MANIFEST_SCHEMA,
    version,
    releasedAt: normalizeReleasedAt(options.releasedAt),
    notes,
    notesUrl: notesUrl(repo, tag),
    // The first build that ships the updater is the tag being released, so
    // nothing older may replace itself in place into a layout it never had.
    // `--minimum-version` exists only for a later release that cannot update
    // from the current one; CI does not pass it.
    minimumVersion: normalizeVersion(options.minimumVersion || version),
    platforms,
  }

  const problems = validateManifest(manifest, { repo })
  if (problems.length) {
    fail(`the generated manifest is invalid:\n  - ${problems.join('\n  - ')}`)
  }
  return manifest
}

async function resolveNotes(options) {
  const notesFile = String(options.notesFile ?? '').trim()
  if (notesFile) {
    let markdown
    try {
      markdown = await readFile(notesFile, 'utf8')
    } catch {
      fail(`cannot read the notes file: ${notesFile}`)
    }
    const summary = summarizeNotes(markdown, NOTES_MAX_LENGTH, options.language)
    if (summary) return summary
    console.error(
      `release-manifest: ${notesFile} has no paragraph to summarize; using the default note`,
    )
  }
  const inline = collapseMarkup(options.notes ?? '')
  if (inline) return truncate(inline, NOTES_MAX_LENGTH)
  const version = normalizeVersion(options.version)
  return `DSH Box ${version} is available. See the release page for the full notes.`
}

// The contract, checked against the object that is about to be written -- and
// re-checked over the bytes on disk by `--check`. Returns the problems rather
// than throwing, so the caller can report all of them at once.
function validateManifest(manifest, { repo = REPOSITORY } = {}) {
  const problems = []
  const require = (condition, message) => {
    if (!condition) problems.push(message)
  }

  require(manifest && typeof manifest === 'object' && !Array.isArray(manifest), 'the manifest is not a JSON object')
  if (!manifest || typeof manifest !== 'object' || Array.isArray(manifest)) return problems

  const topLevel = ['schema', 'version', 'releasedAt', 'notes', 'notesUrl', 'minimumVersion', 'platforms']
  for (const key of topLevel) {
    require(Object.hasOwn(manifest, key), `missing required field: ${key}`)
  }
  for (const key of Object.keys(manifest)) {
    require(topLevel.includes(key), `unknown field: ${key}`)
  }

  require(manifest.schema === MANIFEST_SCHEMA, `schema must be ${MANIFEST_SCHEMA}, found ${String(manifest.schema)}`)

  const version = typeof manifest.version === 'string' ? manifest.version : ''
  require(Boolean(version), 'version is missing')
  require(VERSION_PATTERN.test(version), `version is not a release version: ${version}`)

  const minimum = typeof manifest.minimumVersion === 'string' ? manifest.minimumVersion : ''
  require(Boolean(minimum), 'minimumVersion is missing')
  require(VERSION_PATTERN.test(minimum), `minimumVersion is not a release version: ${minimum}`)

  const releasedAt = typeof manifest.releasedAt === 'string' ? manifest.releasedAt : ''
  require(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/.test(releasedAt), `releasedAt is not ISO-8601 UTC: ${releasedAt}`)

  const notes = typeof manifest.notes === 'string' ? manifest.notes.trim() : ''
  require(Boolean(notes), 'notes is missing')

  const tag = `v${version}`
  require(
    manifest.notesUrl === notesUrl(repo, tag),
    `notesUrl must be ${notesUrl(repo, tag)}, found ${String(manifest.notesUrl)}`,
  )

  const platforms = manifest.platforms
  require(platforms && typeof platforms === 'object' && !Array.isArray(platforms), 'platforms is not an object')
  if (platforms && typeof platforms === 'object' && !Array.isArray(platforms)) {
    const keys = Object.keys(platforms)
    require(keys.length > 0, 'platforms is empty: the release describes no machine')
    for (const key of keys) {
      require(PLATFORM_ORDER.includes(key), `unknown platform key: ${key}`)
      const entries = platforms[key]
      // Every platform's value is an array, even with one installer: a
      // Linux machine has two, and the reader must not branch on the shape.
      require(Array.isArray(entries), `platforms.${key} is not an array`)
      if (!Array.isArray(entries)) continue
      require(entries.length > 0, `platforms.${key} is an empty array; drop the platform instead`)
      const assetBase = assetUrlBase(repo, tag)
      for (const [index, entry] of entries.entries()) {
        const where = `platforms.${key}[${index}]`
        validateAsset(entry, where, assetBase).forEach((problem) => problems.push(problem))
      }
    }
  }
  return problems
}

function validateAsset(asset, where, assetBase) {
  const problems = []
  const require = (condition, message) => {
    if (!condition) problems.push(`${where}: ${message}`)
  }
  require(asset && typeof asset === 'object' && !Array.isArray(asset), 'the entry is not an object')
  if (!asset || typeof asset !== 'object' || Array.isArray(asset)) return problems

  const fields = ['url', 'sha256', 'size', 'installer', 'installMode']
  for (const field of fields) require(Object.hasOwn(asset, field), `missing ${field}`)
  for (const field of Object.keys(asset)) require(fields.includes(field), `unknown field: ${field}`)

  // The url must name this manifest's own tag: an asset pointing at another
  // release would download bytes this manifest never described.
  require(
    typeof asset.url === 'string' && asset.url.startsWith(assetBase) && !asset.url.slice(assetBase.length).includes('/'),
    `url is not an asset of ${assetBase}`,
  )
  require(typeof asset.sha256 === 'string' && DIGEST_PATTERN.test(asset.sha256), 'sha256 is not 64 lower-case hex')
  require(Number.isInteger(asset.size) && asset.size > 0, 'size is not a positive integer')

  const kind = Object.values(INSTALLER_KINDS).find((entry) => entry.installer === asset.installer)
  require(Boolean(kind), `unknown installer: ${String(asset.installer)}`)
  if (kind) {
    require(asset.installMode === kind.installMode, `installMode must be "${kind.installMode}" for ${kind.installer}`)
  }
  const expectedExtension = Object.keys(EXTENSION_TO_KIND).find((extension) => EXTENSION_TO_KIND[extension] === asset.installer)
  require(
    typeof asset.url === 'string' && asset.url.toLowerCase().endsWith(expectedExtension),
    `url must end with ${String(expectedExtension)} for installer ${String(asset.installer)}`,
  )
  return problems
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

const VALUE_FLAGS = {
  '--installers': 'installers',
  '--version': 'version',
  '--repo': 'repo',
  '--notes': 'notes',
  '--notes-file': 'notesFile',
  '--released-at': 'releasedAt',
  '--minimum-version': 'minimumVersion',
  '--language': 'language',
  '--check': 'check',
}

function parseArgs(argv) {
  const options = {
    installers: 'installers',
    repo: REPOSITORY,
    notes: '',
    notesFile: '',
    releasedAt: '',
    minimumVersion: '',
    language: 'en',
    check: '',
    version: '',
    selfTest: false,
    help: false,
  }
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--self-test') {
      options.selfTest = true
      continue
    }
    if (argument === '--help' || argument === '-h') {
      options.help = true
      continue
    }
    const key = VALUE_FLAGS[argument]
    // A flag silently dropped makes a typo look like success.
    if (!key) fail(`unknown argument: ${argument}`)
    if (index + 1 >= argv.length) fail(`${argument} needs a value`)
    options[key] = argv[index + 1]
    index += 1
  }
  return options
}

// `--check` is what CI runs over the file it just redirected: it proves the
// bytes parse and carry every field the updater dereferences without
// unwrapping an `Option`.
async function checkManifestFile(path, { repo, version }) {
  let text
  try {
    text = await readFile(path, 'utf8')
  } catch {
    return { manifest: null, problems: [`cannot read the manifest: ${path}`] }
  }
  let manifest
  try {
    manifest = JSON.parse(text)
  } catch (error) {
    return { manifest: null, problems: [`${path} is not valid JSON: ${error.message}`] }
  }
  const problems = validateManifest(manifest, { repo })
  if (version !== undefined && version !== '') {
    const expected = normalizeVersion(version)
    if (manifest.version !== expected) {
      problems.unshift(`version in the manifest is ${String(manifest.version)}, expected ${expected} (the tag being released)`)
    }
  }
  return { manifest, problems }
}

async function main(argv) {
  const options = parseArgs(argv)
  if (options.help) {
    console.log(USAGE)
    return
  }
  if (options.selfTest) {
    await runSelfTest()
    return
  }

  if (options.check) {
    const { manifest, problems } = await checkManifestFile(options.check, {
      repo: options.repo,
      version: options.version,
    })
    if (problems.length) {
      for (const problem of problems) console.error(`latest.json: ${problem}`)
      fail(`${options.check} does not satisfy schema-${MANIFEST_SCHEMA}`)
    }
    const platforms = Object.entries(manifest.platforms)
      .map(([platform, entries]) => `${platform} (${entries.length})`)
      .join(', ')
    console.log(`${MANIFEST_ASSET_NAME} is valid: version ${manifest.version}, ${platforms}`)
    return
  }

  const manifest = await buildManifest(options)
  const summary = Object.entries(manifest.platforms)
    .map(([platform, entries]) => `${platform}=${entries.map((entry) => entry.installer).join('+')}`)
    .join(' ')
  console.error(
    `release-manifest: ${manifest.version} ${summary} minimumVersion=${manifest.minimumVersion}`,
  )
  // Trailing newline: the file is read by people as well as by the updater.
  process.stdout.write(`${JSON.stringify(manifest, null, 2)}\n`)
}

// ---------------------------------------------------------------------------
// Self-test
// ---------------------------------------------------------------------------
//
// Builds a fake installers directory in a temp dir -- no fixture is checked
// in, because a fixture is a copy of the contract that drifts from it -- and
// asserts the shape, the digests, the filenames, the order and the refusals.
// Run with: node scripts/release-manifest.mjs --self-test

const FIXTURE_BYTES = {
  'dshbox_0.1.9_x64_en-US.msi': 'windows msi fixture\n',
  'dshbox_0.1.9_aarch64.dmg': 'macos dmg fixture\n',
  'deb/dshbox_0.1.9_amd64.deb': 'linux deb fixture\n',
  'rpm/dshbox-0.1.9-1.x86_64.rpm': 'linux rpm fixture\n',
  // Not installers, and a previous run's manifest: all three must be ignored.
  'SHA256SUMS.txt': 'not an installer\n',
  'latest.json': '{"schema":1,"version":"0.0.0"}\n',
  'notes.md': '# heading only\n',
}

const NOTES_FIXTURE = [
  '# DSH Box 0.1.9',
  '',
  '**简体中文** · [English](#english)',
  '',
  '---',
  '',
  '## 简体中文',
  '',
  '这个版本  让 `DSH Box` 成为一个沙盒，',
  '改坏了重启一个容器就好。',
  '',
  '### 更多',
  '',
  '- 列表不该被当成摘要。',
].join('\n')

let checks = 0
let section = ''

function assert(condition, message) {
  checks += 1
  if (!condition) throw new Error(`${section}: ${message}`)
}

function assertEqual(actual, expected, message) {
  assert(
    JSON.stringify(actual) === JSON.stringify(expected),
    `${message} (expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)})`,
  )
}

function assertThrows(action, pattern, message) {
  try {
    action()
  } catch (error) {
    checks += 1
    if (pattern && !pattern.test(error.message)) {
      throw new Error(`${section}: ${message} (error was "${error.message}")`)
    }
    return
  }
  throw new Error(`${section}: ${message} (nothing was thrown)`)
}

async function assertRejects(action, pattern, message) {
  try {
    await action()
  } catch (error) {
    checks += 1
    if (pattern && !pattern.test(error.message)) {
      throw new Error(`${section}: ${message} (error was "${error.message}")`)
    }
    return
  }
  throw new Error(`${section}: ${message} (nothing was thrown)`)
}

const REPO = 'Nexus-Aethra/DSHBox'
const FIXTURE_VERSION = '0.1.9'
const FIXTURE_TAG = 'v0.1.9'
const FIXTURE_AT = '2026-11-02T00:00:00Z'
const ASSET_FIELDS = ['installMode', 'installer', 'sha256', 'size', 'url']
const TOP_FIELDS = ['minimumVersion', 'notes', 'notesUrl', 'platforms', 'releasedAt', 'schema', 'version']

async function writeFixture(directory, files) {
  for (const [relative, bytes] of Object.entries(files)) {
    const path = join(directory, relative)
    await mkdir(dirname(path), { recursive: true })
    await writeFile(path, bytes)
  }
  return directory
}

function digestHex(bytes) {
  // Deliberately not `digestFile`: the self-test must check the streamed
  // digest against an independent one, not against itself.
  return createHash('sha256').update(bytes).digest('hex')
}

async function testManifestShape(root) {
  section = 'nested artifact layout'
  const installers = await writeFixture(join(root, 'installers'), FIXTURE_BYTES)
  const manifest = await buildManifest({
    installers,
    version: FIXTURE_TAG,
    repo: REPO,
    releasedAt: FIXTURE_AT,
  })

  assertEqual(Object.keys(manifest).sort(), TOP_FIELDS, 'top-level fields are exactly the contract')
  assertEqual(manifest.schema, 1, 'schema is 1')
  assertEqual(manifest.version, FIXTURE_VERSION, 'version loses the tag prefix')
  assertEqual(manifest.releasedAt, FIXTURE_AT, 'releasedAt is normalized ISO-8601 UTC')
  assertEqual(manifest.notesUrl, `https://github.com/${REPO}/releases/tag/${FIXTURE_TAG}`, 'notesUrl names the release tag')
  assertEqual(manifest.minimumVersion, FIXTURE_VERSION, 'minimumVersion is the released version')
  assertEqual(
    manifest.platforms && Object.keys(manifest.platforms),
    ['win-x64', 'linux-x64', 'macos-arm64'],
    'platform keys are the bundled targets, in a stable order',
  )
  for (const platform of Object.keys(manifest.platforms)) {
    assert(Array.isArray(manifest.platforms[platform]), `${platform} is an array`)
  }
  assertEqual(manifest.platforms['linux-x64'].length, 2, 'linux-x64 carries both installers')
  assertEqual(manifest.platforms['win-x64'].length, 1, 'win-x64 carries one installer')
  assertEqual(manifest.platforms['macos-arm64'].length, 1, 'macos-arm64 carries one installer')
  assertEqual(
    manifest.platforms['linux-x64'].map((entry) => entry.installer),
    ['deb', 'rpm'],
    'linux-x64 publishes the deb before the rpm',
  )
  assertEqual(
    manifest.platforms['win-x64'][0].installer,
    'msi',
    'the windows entry is an msi',
  )

  const expectations = {
    'win-x64': { installer: 'msi', installMode: 'auto', extension: '.msi', asset: 'dshbox_0.1.9_x64_en-US.msi' },
    'linux-x64': [
      { installer: 'deb', installMode: 'manual', extension: '.deb', asset: 'dshbox_0.1.9_amd64.deb' },
      { installer: 'rpm', installMode: 'manual', extension: '.rpm', asset: 'dshbox-0.1.9-1.x86_64.rpm' },
    ],
    'macos-arm64': { installer: 'dmg', installMode: 'auto', extension: '.dmg', asset: 'dshbox_0.1.9_aarch64.dmg' },
  }
  for (const [platform, wanted] of Object.entries(expectations)) {
    const entries = Array.isArray(wanted) ? wanted : [wanted]
    for (const [index, entry] of manifest.platforms[platform].entries()) {
      const expected = entries[index]
      section = `${platform}[${index}]`
      assertEqual(Object.keys(entry).sort(), ASSET_FIELDS, 'asset fields are exactly the contract')
      assert(DIGEST_PATTERN.test(entry.sha256), 'sha256 is 64 lower-case hex')
      assertEqual(entry.installer, expected.installer, 'installer matches the file kind')
      assertEqual(entry.installMode, expected.installMode, 'installMode follows the contract mapping')
      assertEqual(
        entry.url,
        `https://github.com/${REPO}/releases/download/${FIXTURE_TAG}/${expected.asset}`,
        'url uses the real basename the bundler produced',
      )
      assert(!entry.url.includes('/deb/') && !entry.url.includes('/rpm/'), 'the url has no artifact subdirectory')
      assert(entry.url.endsWith(expected.extension), 'url ends with the installer extension')
      assertEqual(
        entry.sha256,
        digestHex(FIXTURE_BYTES[Object.keys(FIXTURE_BYTES).find((key) => key.endsWith(expected.asset))]),
        'sha256 is the digest of the file bytes',
      )
      const filePath = Object.keys(FIXTURE_BYTES).find((key) => key.endsWith(expected.asset))
      assertEqual(entry.size, Buffer.byteLength(FIXTURE_BYTES[filePath]), 'size is the byte count of the file')
    }
  }

  section = 'contract checks'
  assertEqual(validateManifest(manifest, { repo: REPO }), [], 'the generated manifest validates')
  assertEqual(JSON.parse(JSON.stringify(manifest)), manifest, 'the manifest is JSON round-trippable')
  assertEqual(JSON.parse(JSON.stringify(manifest, null, 2)), JSON.parse(JSON.stringify(manifest)), 'pretty JSON parses back')

  // A wrong digest is the failure that matters, so prove the check sees it.
  section = 'corrupt manifest'
  const corrupt = JSON.parse(JSON.stringify(manifest))
  corrupt.platforms['win-x64'][0].sha256 = 'deadbeef'
  assert(
    validateManifest(corrupt, { repo: REPO }).some((problem) => problem.includes('sha256')),
    'a short digest is reported',
  )
  const missing = JSON.parse(JSON.stringify(manifest))
  delete missing.minimumVersion
  assert(
    validateManifest(missing, { repo: REPO }).some((problem) => problem.includes('minimumVersion')),
    'a missing minimumVersion is reported',
  )
  const wrongUrl = JSON.parse(JSON.stringify(manifest))
  wrongUrl.platforms['linux-x64'][0].url = wrongUrl.platforms['linux-x64'][0].url.replace(FIXTURE_TAG, 'v9.9.9')
  assert(
    validateManifest(wrongUrl, { repo: REPO }).some((problem) => problem.includes('url')),
    'a url pointing at another tag is reported',
  )
}

async function testOrderAndFlatLayout(root) {
  section = 'flat artifact layout'
  // Alphabetical order puts the rpm first; the published order must not.
  const installers = await writeFixture(join(root, 'flat'), {
    'dshbox-0.1.9-1.x86_64.rpm': 'rpm\n',
    'dshbox_0.1.9_amd64.deb': 'deb\n',
  })
  const manifest = await buildManifest({
    installers,
    version: FIXTURE_VERSION,
    repo: REPO,
    releasedAt: FIXTURE_AT,
  })
  assertEqual(
    manifest.platforms['linux-x64'].map((entry) => entry.installer),
    ['deb', 'rpm'],
    'publish order is deb then rpm regardless of the traversal order',
  )
  assertEqual(Object.keys(manifest.platforms), ['linux-x64'], 'only the platform that produced an asset appears')
}

async function testNotes() {
  section = 'notes summary'
  const expected = '这个版本 让 DSH Box 成为一个沙盒， 改坏了重启一个容器就好。'
  // The fixture is Chinese-only, so the English default falls back to it.
  assertEqual(summarizeNotes(NOTES_FIXTURE), expected, 'the first prose paragraph is the summary')
  assertEqual(summarizeNotes(NOTES_FIXTURE, NOTES_MAX_LENGTH, 'zh'), expected, 'the zh section is selected explicitly')
  assert(!summarizeNotes(NOTES_FIXTURE).includes('简体'), 'the language switcher is not the summary')
  assert(!summarizeNotes(NOTES_FIXTURE).includes('列表'), 'a list under a later heading is not the summary')
  assertEqual(summarizeNotes('# only a heading\n\n---'), '', 'a file with no prose has no summary')

  section = 'bilingual notes'
  const bilingual = [
    '# DSH Box 0.1.9',
    '',
    '**简体中文** \u00b7 [English](#english)',
    '',
    '## 简体中文',
    '',
    '中文段落。',
    '',
    '## English<a id="english"></a>',
    '',
    'This release makes DSH Box a sandbox with both hands free.',
    '',
    '### A detail',
    '',
    'Detail prose that is not the summary.',
  ].join('\n')
  assertEqual(
    summarizeNotes(bilingual),
    'This release makes DSH Box a sandbox with both hands free.',
    'the English section is preferred, because the UI defaults to English',
  )
  assertEqual(summarizeNotes(bilingual, NOTES_MAX_LENGTH, 'zh'), '中文段落。', 'the zh section can still be requested')
  assertEqual(
    summarizeNotes(bilingual, NOTES_MAX_LENGTH, 'en-US'),
    'This release makes DSH Box a sandbox with both hands free.',
    'a regional tag matches its base language',
  )
  assertEqual(
    summarizeNotes(bilingual, NOTES_MAX_LENGTH, 'fr'),
    '中文段落。',
    'an untranslated language falls back to the first prose paragraph',
  )
  assertEqual(languagePattern('ZH_hans'), languagePattern('zh'), 'language tags are case- and separator-insensitive')
  assertEqual(languagePattern('fr'), null, 'an unknown language has no section pattern')

  section = 'notes truncation'
  const long = `${'word '.repeat(120)}end`
  const summary = summarizeNotes(long)
  assert(summary.length <= NOTES_MAX_LENGTH, 'the summary is capped')
  assert(summary.endsWith('…'), 'a truncated summary ends with an ellipsis')
  assert(summary.startsWith('word word'), 'truncation keeps the beginning')
  assertEqual(summarizeNotes(long, 20).length <= 20, true, 'the cap is the argument')

  section = 'notes fallback'
  const manifest = await buildManifest({
    installers: join(tmpdir(), 'dshbox-release-manifest-unused'),
    version: FIXTURE_VERSION,
    repo: REPO,
    releasedAt: FIXTURE_AT,
  }).catch((error) => error)
  assert(manifest instanceof Error, 'a missing installers directory is an error, not a default')
  assert(/installers directory not found/.test(manifest.message), 'the error names the missing directory')

  section = 'notes from a file'
  const notesPath = join(tmpdir(), 'dshbox-release-manifest-notes.md')
  await writeFile(notesPath, NOTES_FIXTURE)
  const parsed = parseArgs(['--version', FIXTURE_TAG, '--notes-file', notesPath])
  assertEqual(parsed.notesFile, notesPath, '--notes-file is parsed')
  assertEqual(summarizeNotes(await readFile(notesPath, 'utf8')), expected, 'the file path yields the same summary')
  await rm(notesPath, { force: true })
}

async function testVersionsAndArgs() {
  section = 'version and time normalization'
  assertEqual(normalizeVersion('v0.1.9'), '0.1.9', 'a leading v is dropped')
  assertEqual(normalizeVersion('  V0.1.9  '), '0.1.9', 'surrounding space and a capital V are dropped')
  assertEqual(normalizeVersion('0.2.0-rc.1'), '0.2.0-rc.1', 'a pre-release version survives')
  assertThrows(() => normalizeVersion(''), /version is required/, 'an empty version is refused')
  assertThrows(() => normalizeVersion('release-1'), /not a release version/, 'a non-version is refused')
  assertEqual(normalizeReleasedAt('2026-11-02'), '2026-11-02T00:00:00Z', 'a date is normalized to UTC midnight')
  assertEqual(normalizeReleasedAt('2026-11-02T00:00:00Z'), '2026-11-02T00:00:00Z', 'a UTC timestamp round-trips')
  assertThrows(() => normalizeReleasedAt('not a date'), /ISO-8601/, 'a bad timestamp is refused')
  const now = normalizeReleasedAt()
  assert(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/.test(now), 'the default releasedAt is ISO-8601 UTC')
  assert(Math.abs(new Date(now).getTime() - Date.now()) < 60_000, 'the default releasedAt is now')

  section = 'argument parsing'
  assertEqual(parseArgs([]).installers, 'installers', 'the installers directory has a default')
  assertEqual(parseArgs([]).repo, REPO, 'the repository has the canonical default')
  assertThrows(() => parseArgs(['--nope']), /unknown argument/, 'an unknown flag is refused')
  assertThrows(() => parseArgs(['--version']), /needs a value/, 'a value flag without a value is refused')
  const parsed = parseArgs(['--installers', 'x', '--version', 'v0.1.9', '--repo', 'Me/Fork', '--notes', 'hello'])
  assertEqual([parsed.installers, parsed.version, parsed.repo, parsed.notes], ['x', 'v0.1.9', 'Me/Fork', 'hello'], 'value flags are parsed in order')
  assertEqual(parseArgs(['--notes-file', '']).notesFile, '', 'an empty notes-file means no notes file')
}

async function testCheckMode(root) {
  section = 'check mode'
  const path = join(root, 'latest.json')
  const manifest = await buildManifest({
    installers: await writeFixture(join(root, 'check-installers'), FIXTURE_BYTES),
    version: FIXTURE_TAG,
    repo: REPO,
    releasedAt: FIXTURE_AT,
  })
  await writeFile(path, JSON.stringify(manifest, null, 2))

  const good = await checkManifestFile(path, { repo: REPO, version: FIXTURE_TAG })
  assertEqual(good.problems, [], 'a manifest written by the generator passes --check')

  const otherTag = await checkManifestFile(path, { repo: REPO, version: 'v9.9.9' })
  assert(otherTag.problems.length > 0, 'a manifest for another tag fails --check')

  await writeFile(join(root, 'empty.json'), '')
  const empty = await checkManifestFile(join(root, 'empty.json'), { repo: REPO, version: FIXTURE_TAG })
  assert(empty.problems[0].includes('not valid JSON'), 'an empty file fails --check')

  const truncated = JSON.stringify(manifest).slice(0, 40)
  await writeFile(join(root, 'truncated.json'), truncated)
  const bad = await checkManifestFile(join(root, 'truncated.json'), { repo: REPO, version: FIXTURE_TAG })
  assert(bad.problems[0].includes('not valid JSON'), 'truncated JSON fails --check')

  await writeFile(join(root, 'schema2.json'), JSON.stringify({ ...manifest, schema: 2 }))
  const future = await checkManifestFile(join(root, 'schema2.json'), { repo: REPO, version: FIXTURE_TAG })
  assert(future.problems.some((problem) => problem.includes('schema')), 'an unknown schema fails --check')

  await writeFile(join(root, 'no-platforms.json'), JSON.stringify({ ...manifest, platforms: {} }))
  const none = await checkManifestFile(join(root, 'no-platforms.json'), { repo: REPO, version: FIXTURE_TAG })
  assert(none.problems.some((problem) => problem.includes('platforms')), 'an empty platforms map fails --check')
}

async function testRefusals(root) {
  section = 'refusals'
  const emptyDir = join(root, 'empty')
  await mkdir(emptyDir, { recursive: true })
  await assertRejects(
    () => buildManifest({ installers: emptyDir, version: FIXTURE_VERSION, repo: REPO }),
    /no installers/,
    'an empty installers directory is refused',
  )
  await assertRejects(
    () => buildManifest({ installers: join(root, 'missing'), version: FIXTURE_VERSION, repo: REPO }),
    /installers directory not found/,
    'a missing installers directory is refused',
  )

  const duplicate = join(root, 'duplicate')
  await writeFixture(duplicate, {
    'a/x.msi': 'one\n',
    'b/x.msi': 'two\n',
  })
  await assertRejects(
    () => buildManifest({ installers: duplicate, version: FIXTURE_VERSION, repo: REPO }),
    /same asset name/,
    'two files that would collide as one asset are refused',
  )

  await assertRejects(
    () => buildManifest({ installers: emptyDir, version: FIXTURE_VERSION, repo: 'not a repo' }),
    /not a GitHub repository/,
    'a malformed repository is refused',
  )
}

async function runSelfTest() {
  const root = await mkdtemp(join(tmpdir(), 'dshbox-release-manifest-'))
  try {
    await testManifestShape(root)
    await testOrderAndFlatLayout(root)
    await testNotes()
    await testVersionsAndArgs()
    await testCheckMode(root)
    await testRefusals(root)
  } finally {
    await rm(root, { recursive: true, force: true })
  }
  console.log(`release-manifest self-test passed: ${checks} checks`)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

// Only run when executed as a script, so an importer never gets the side
// effects of argv parsing.
const invokedDirectly = () => {
  const script = fileURLToPath(import.meta.url)
  const argument = resolve(process.argv[1] ?? '')
  if (process.platform === 'win32') return script.toLowerCase() === argument.toLowerCase()
  return script === argument
}

if (invokedDirectly()) {
  main(process.argv.slice(2)).catch((error) => {
    console.error(`release-manifest: ${error.message}`)
    process.exit(1)
  })
}
