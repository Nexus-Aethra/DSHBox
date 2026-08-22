# Template system

Status: current model, 2026-08-20. Detailed contracts live in [prepared-template-runtime.md](specs/prepared-template-runtime.md).

## Terms

- **Prepared base**: an immutable Harness source/dependency cache. Its final-path `node_modules` is used only to seed and validate offline installation; it is never recursively copied.
- **Sealed template**: a physical source recipe derived from a prepared base, containing no `node_modules` and recording its local plugin artifacts.
- **Container**: the final, independently prepared copy of a sealed template. It is independent of the base and all other containers; it uses repository artifacts only during its one-time preparation.

## User flow

1. Pull a root Harness ref. Box clones it to staging, installs dependencies, builds it, runs a bounded health check, and publishes a prepared base.
2. Import extensions into the repository. Each import is converted to a checked local `artifact.tgz`; source preparation remains subject to approval.
3. Build a Boxfile. Box copies prepared source without `node_modules`, records its local artifact references, and publishes a sealed recipe.
4. Create a container. Box copies that recipe to the final instance path, runs `pnpm install --offline`, adds each local artifact, runs `pnpm run build`, then writes metadata.
5. Start it. Box invokes bundled `pnpm dsh web` from the already prepared container. It does not perform `pnpm install`, plugin add, or `pnpm run build`.

Pull and first container creation are intentionally expensive and visible. Ordinary startup is intentionally boring: process launch and readiness check.

## Template identity and safety

Template identity is digest-addressed and includes the exact base revision, bundled toolchain versions, plugin artifact digests, and materialized profile. All publish operations use a private staging directory followed by atomic rename. A failed task cannot create a visible template.

The reusable trees are source-only and link-free. Windows pnpm workspace junctions are created only during the final instance's offline install, so they cannot point back to staging, a repository cache, or another container.

## Breaking storage change

This model requires storage schema 10. Box does not support or mutate legacy metadata-only templates and `runtimes/<version>/source` directories. Users of an old data root must choose a new root after preserving any data they need.
