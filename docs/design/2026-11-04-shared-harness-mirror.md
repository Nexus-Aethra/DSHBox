# Shared Harness mirror repository (常量仓库)

- Date: 2026-11-04
- Status: proposed
- Related: `docs/design/2026-11-04-node-artifact.md` (Phase B), `docs/specs/prepared-template-runtime.md`

## Problem

Every `pull_template` performs a fresh shallow clone of
`github.com/deepseek-ai/deepseek-harness` from the network into a private
staging directory (`shallow_clone_with_cancel` in `box-runtime/src/lib.rs`,
called from `dshboxd/src/versions.rs`). Pulling N versions of the same
repository pays the full transfer N times — several hundred megabytes and
minutes each — and each pull re-exposes itself to network failures (proxy
timeouts, TLS resets) that show up to the user as a stuck clone stage.

The same machine-local lesson applies as with WSL2 images: **move whole
objects over a fast path once, never re-fetch a full tree per version.**

## Design

A persistent bare mirror repository lives under the runtime directory:

```
<runtime>/harness-mirror.git/          # bare repo, one per machine
```

`pull_template_with_cancel` becomes:

1. **Ensure mirror.** If `<runtime>/harness-mirror.git` does not exist,
   clone the remote into it once (this is the only full transfer ever).
   A `MIRROR_OWNER` marker inside records the remote URL the mirror was
   built from; if the URL changed, the mirror is rebuilt from scratch.
2. **Fetch incrementally.** Open the bare repo with libgit2 and fetch the
   target tag (`refs/tags/<tag>` — plus `refs/tags/latest` semantics
   resolved first via the existing `list_remote_tags` on the mirror, or the
   network when the mirror cannot answer). An incremental fetch of one new
   tag transfers only new objects — typically tens of megabytes.
3. **Check out locally.** Clone from the *local mirror* into the staging
   directory (libgit2 local clone, no network), checkout the resolved
   commit. Local-disk speed, cancel still honoured.
4. **Copy into the prepared base** exactly as today; the rest of the
   pipeline (install/build/validate/publish, stage progress) is unchanged.

The mirror is an internal cache, not user state: it can be deleted at any
time and the next pull rebuilds it. It is not part of exports/copies.

## Concurrency

Fetch is the only writer. A dedicated resource lock key
`harness-mirror` (same mechanism as `repository:dsh-versions`) serialises
fetches; checkout is a reader against a bare repo and runs without the
lock. Two template pulls of different tags therefore run concurrently
except during the (short) fetch.

## Self-healing

Any libgit2 error attributed to the mirror (corrupt pack, missing object,
failed lookup) deletes `<runtime>/harness-mirror.git` and retries the pull
once from a fresh mirror clone. The retry is logged as a task-log line; a
mirror failure is never silently absorbed — the second failure surfaces to
the task like the network failure it is.

## What does not change

- Prepared-base index, dedup and `validate_prepared_harness` are untouched.
- The `client_artifacts_present` platform rule is untouched.
- Stage progress (PR #28) is untouched; clone stage keeps its budget.
- Local-directory refs (`Path::new(url).is_dir()` branch) bypass the
  mirror entirely — they are already local.

## Cost

- Disk: the mirror keeps a full object set of the Harness repository
  (~1.3 GB pack, close to one `wsl --import` image). Acceptable: one copy
  per machine replaces one full network clone per version.
- Code: `box-runtime` gains `ensure_bare_mirror` + `fetch_tag_to_mirror`
  (libgit2); `versions.rs` swaps `shallow_clone_with_cancel(remote)` for
  mirror + local clone. Estimated small, contained change.

## Alternatives considered

- *Shallow-fetch per version into the mirror*: keeps the mirror smaller
  but loses the ability to resolve arbitrary older tags later without a
  network round trip; a full mirror is simpler and the size is bounded by
  the repository itself.
- *Reuse the container store/`data/<digest>` dedup for clones*: wrong
  layer — those are artifact caches, not git object caches.
