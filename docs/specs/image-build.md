# Template build specification

Status: superseded and replaced by the prepared-template runtime model on 2026-08-20. The authoritative specification is [prepared-template-runtime.md](prepared-template-runtime.md).

`dshbox build` no longer creates a metadata-only image or a declaration that is materialized when a container starts. It creates a sealed physical template: the complete prepared Harness checkout, its built assets and dependencies, a profile with local plugin artifacts installed, template skills/data, and a manifest all live below `templates/sealed-<digest>/`.

`dshbox image` remains only a command alias for `build`/`template`; it does not refer to a separate image registry.

The historical `list.json`/`script.dsh`-only format and shared `runtimes/<version>/source` launch path must not be extended. New implementation work follows the transaction, storage, migration, and validation requirements in the authoritative specification.
