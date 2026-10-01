/**
 * dshbox daemon client: discovery record + loopback JSON-RPC transport.
 *
 * dshboxd binds a DYNAMIC loopback port and authenticates every call with a
 * token minted at launch. Both facts live in a discovery record the daemon
 * writes atomically to <config-dir>/server/discovery.json and removes on
 * shutdown. Nothing here is hardcoded: the record is re-read at call time, so
 * a daemon restart (new port, new token) is transparent to callers instead
 * of leaving them holding a stale endpoint.
 *
 * Wire contract, verified against dshboxd/src/main.rs:
 *
 * - POST /rpc is matched EXACTLY (path != "/rpc" -> 404), so the path
 *   carries no query string.
 * - The token travels in the JSON BODY, not in an Authorization header. The
 *   Rust client sends a Bearer header too, but the daemon never reads it --
 *   sending it is harmless, relying on it silently fails.
 * - Params are FLATTENED into the top-level request object alongside token
 *   and method; there is no nested params field.
 * - Replies are {ok: true, result: ...} for sync methods and
 *   {ok: true, task: ...} for async ones, or {ok: false, error: ...}.
 *   The daemon closes the connection after each frame.
 * - GET /events?token=... is the SSE stream, which takes the token from the
 *   query string instead.
 *
 * @module dsh-box-sandbox/rpc
 */

import { readFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { join } from 'node:path'

/**
 * The launch record dshboxd publishes while it is alive.
 *
 * 'endpoint' is a legacy Unix-domain-socket field kept only so that stale
 * records from older daemon builds still deserialize; the daemon never
 * writes it, so this interface does not model it.
 */
export interface Discovery {
  /** Bearer-equivalent secret minted at launch; required on every call. */
  token: string
  /** Owning process id, for liveness checks and diagnostics. */
  pid: number
  /** Unix seconds at launch; a client can use it to spot a stale record. */
  startedAt: number
  /** Dynamic loopback port the daemon is listening on. */
  port: number
}

/** Envelope the daemon wraps every /rpc reply in. */
interface Frame<T> {
  ok: boolean
  /** Present on synchronous replies. */
  result?: T
  /** Present instead of result when a method enqueued a background task. */
  task?: T
  error?: string
}

/** Constructor options for DshboxRpc. */
export interface DshboxRpcOptions {
  /**
   * Directory holding server/discovery.json. Defaults to DSHBOX_CONFIG_DIR
   * when set, otherwise ~/.dsh-box -- the same precedence that
   * box_foundation::config_path applies on the Rust side.
   */
  configDir?: string
  /** Per-attempt request timeout in milliseconds. Default 30000. */
  timeoutMs?: number
}

/**
 * Raised when no usable daemon can be reached: the discovery record is
 * absent, unreadable, or describes an endpoint that refuses connections.
 *
 * It is distinct from a daemon-reported failure -- those come back as a
 * plain Error carrying the daemon's own message, because the daemon was
 * reached and answered.
 */
export class DshboxUnavailableError extends Error {
  override readonly name = 'DshboxUnavailableError'

  constructor(message: string, options?: { cause?: unknown }) {
    super(message, options)
  }
}

/**
 * Resolve the config directory, honouring DSHBOX_CONFIG_DIR.
 *
 * An empty override counts as absent: the plugin config defaults
 * configDir to the empty string to mean "resolve it the way the daemon
 * does", and treating that as a real path would look for discovery.json
 * relative to the process working directory.
 */
export function resolveConfigDir(override?: string): string {
  if (override !== undefined && override !== '') return override
  return process.env.DSHBOX_CONFIG_DIR ?? join(homedir(), '.dsh-box')
}

/** Absolute path of the discovery record for a config directory. */
export function discoveryPath(configDir?: string): string {
  return join(resolveConfigDir(configDir), 'server', 'discovery.json')
}

function isDiscovery(value: unknown): value is Discovery {
  if (typeof value !== 'object' || value === null) return false
  const d = value as Record<string, unknown>
  return (
    typeof d.token === 'string' &&
    d.token.length > 0 &&
    typeof d.port === 'number' &&
    Number.isInteger(d.port) &&
    d.port > 0 &&
    d.port <= 65535 &&
    typeof d.pid === 'number' &&
    typeof d.startedAt === 'number'
  )
}

/**
 * Read the discovery record, or null when the daemon is not running.
 *
 * A missing file and a malformed one are treated the same way on purpose:
 * both mean "there is no endpoint to talk to", and the caller surfaces one
 * actionable message instead of two different failure modes.
 */
export function readDiscovery(configDir?: string): Discovery | null {
  let text: string
  try {
    text = readFileSync(discoveryPath(configDir), 'utf8')
  } catch {
    return null
  }
  try {
    const parsed: unknown = JSON.parse(text)
    return isDiscovery(parsed) ? parsed : null
  } catch {
    return null
  }
}

/** True when a failure means "the discovery record is probably stale". */
function isStaleEndpoint(error: unknown): boolean {
  if (error instanceof DshboxUnavailableError) return true
  if (!(error instanceof Error)) return false
  const code = (error as { cause?: { code?: string } }).cause?.code
  return code === 'ECONNREFUSED' || code === 'ECONNRESET' || code === 'EPIPE'
}

/** Transport for a single discovered daemon. */
export class DshboxRpc {
  readonly configDir: string
  private readonly timeoutMs: number

  constructor(options: DshboxRpcOptions = {}) {
    this.configDir = resolveConfigDir(options.configDir)
    this.timeoutMs = options.timeoutMs ?? 30_000
  }

  /**
   * Return a usable record or explain why there is none.
   *
   * The message names the exact path looked at: "dshbox is not running" is
   * the most common first-run failure, and the user needs to know WHICH
   * profile directory was consulted before they start the app.
   */
  discovery(): Discovery {
    const path = discoveryPath(this.configDir)
    const record = readDiscovery(this.configDir)
    if (record === null) {
      throw new DshboxUnavailableError(
        'dshbox daemon is not running (no usable discovery record at ' +
          path +
          '); start DSH Box first',
      )
    }
    return record
  }

  /** Loopback base URL for the currently discovered endpoint. */
  endpointUrl(record: Discovery): string {
    return 'http://127.0.0.1:' + record.port
  }

  /** SSE stream URL. The token goes in the query string, not a header. */
  eventsUrl(record: Discovery): string {
    return this.endpointUrl(record) + '/events?token=' + encodeURIComponent(record.token)
  }

  private async send<T>(
    record: Discovery,
    method: string,
    params?: Record<string, unknown>,
  ): Promise<T> {
    const response = await fetch(this.endpointUrl(record) + '/rpc', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      // token and method sit at the top level; caller params are flattened
      // alongside them, exactly as box-client builds the frame.
      body: JSON.stringify({ token: record.token, method, ...params }),
      signal: AbortSignal.timeout(this.timeoutMs),
    })

    if (response.status === 401) {
      // Reached the daemon but the token no longer matches, i.e. it
      // restarted between the read and the call. Retrying with a fresh
      // record is the only way forward.
      throw new DshboxUnavailableError(
        'dshbox daemon rejected the discovery token (it may have restarted)',
      )
    }
    if (!response.ok) {
      throw new Error('dshbox daemon returned HTTP ' + response.status + ' for ' + method)
    }

    const frame = (await response.json()) as Frame<T>
    if (!frame.ok) {
      throw new Error(frame.error ?? 'dshbox daemon reported an unknown error for ' + method)
    }
    // Async methods answer with task in place of result; surface whichever
    // one this call produced, matching box-client's behaviour.
    const payload = frame.result ?? frame.task
    return (payload ?? null) as T
  }

  /**
   * Invoke one daemon method.
   *
   * A transport failure is retried exactly once against a freshly read
   * discovery record. That single retry is what absorbs a daemon restart:
   * the first attempt uses the port from the old record, fails to connect,
   * and the second uses the new one. Failures the daemon itself reports are
   * never retried -- they are answers, not outages.
   */
  async call<T = unknown>(method: string, params?: Record<string, unknown>): Promise<T> {
    for (let attempt = 0; attempt < 2; attempt++) {
      const record = this.discovery()
      try {
        return await this.send<T>(record, method, params)
      } catch (error) {
        if (attempt === 0 && isStaleEndpoint(error)) continue
        throw error
      }
    }
    /* c8 ignore next 2 */
    throw new DshboxUnavailableError('dshbox daemon unreachable for ' + method)
  }

  /** Liveness probe. Resolves to the daemon's own status frame. */
  ping(): Promise<{ pid: number; status: string; runtime: string; startedAt: number }> {
    return this.call('ping')
  }
}
