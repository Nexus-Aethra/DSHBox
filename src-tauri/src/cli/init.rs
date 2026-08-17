//! `dshbox init` — generate a starter `boxfile.dsh` next to the user.
//!
//! Mirrors `docker init` / `npm init`: drop a complete, commented example
//! that the user can edit and immediately pass to `dshbox build`. The
//! script is intentionally short and opinionated so a first-time user can
//! see exactly what a boxfile looks like and how each line feeds into the
//! pull / build / run workflow.

use std::path::{Path, PathBuf};

const STARTER_BOXFILE: &str = "\
# DSH Box boxfile — a `.dsh` script describing one container.
#
# The layout mirrors a Dockerfile: `FROM` points at a base template,
# `PROFILE` selects the runtime context, and `ADD` lines layer plugins,
# skills, or data on top. Build & run with:
#
#   1. dshbox pull template <ref>              # fetch a base template once
#   2. dshbox build ./boxfile.dsh --name my-app # build a TEMPLATE (no container)
#   3. dshbox run my-app                        # create + start a container
#
# See `dshbox help` for the full workflow.
#
# ── Base template & runtime profile ──────────────────────────────────────
# A base template is itself a `.dsh` script (one you pulled with
# `dshbox pull template`). Profile picks the runtime layout (`web`,
# `cli`, ...) and decides which extension directories get mounted.
#
# Use a GitHub short-form ref (host + owner + repo + optional tag) so
# the base template is resolved by name; `:latest` is implied when the
# tag is omitted.
FROM github.com/deepseek-ai/deepseek-harness:latest
PROFILE web

# ── ADD directives ───────────────────────────────────────────────────────
# An `ADD` line layers one extension (plugin / skill / data) into the
# container. The source can be any of the following shapes; comment out
# the ones you don't need.
#
#   plugin     npm-style JavaScript plugin installed in the selected profile
#   skill      SKILL.md-style knowledge pack installed in profile/skills
#   data       payload copied into the container's data dir (never linked)
#
#   bare name          name of an entry already in the local repository;
#                      e.g. `ADD plugin my-plugin` or `ADD plugin @scope/my-plugin`
#   GitHub short form  github.com/<owner>/<repo>[@<ref>]; a tag is
#                      optional (defaults to HEAD); e.g.
#                      `ADD plugin github.com/foo/bar@v1.0.0`
#   npm registry       npm:<name>[@<version>]; e.g.
#                      `npm:@org/foo@^1.0.0` or `npm:react@18`
#   git / git+ssh      git+ssh://git@host/<owner>/repo[#<ref>] etc.
#   local absolute     /path/to/local/dir
#   local relative     ./relative/path  or  ../relative/path
#   local tarball      file:///path/to/archive.tar.gz
#   remote tarball     https://example.com/archive.tar.gz
#   workspace alias    workspace:* refers to a sibling workspace entry
#   pnpm alias         alias@npm:real-package@version
#
# Anything the upstream `pnpm add` accepts is accepted here: paste a URL
# someone shared with you, the GitHub URL from a release page, or an
# `npm:foo` form — DSH Box routes it through the install-handler crate
# so the source path, fetch strategy, and post-install reconciliation
# can never drift across surfaces.
#
# ── Examples (uncomment the ones you want) ───────────────────────────────
#
# From the local repository (already imported with `dshbox plugin import`)
# ADD plugin my-plugin
# ADD skill my-skill
#
# From a public GitHub repo (optionally pinned to a tag)
# ADD plugin github.com/deepseek-ai/deepseek-harness-plugin@v1.0.0
# ADD skill github.com/deepseek-ai/deepseek-harness-skill
#
# From the npm registry (any spec pnpm accepts)
# ADD plugin npm:@linxin666/dsh-web-ui-all
# ADD plugin npm:lodash@4.17.21
# ADD plugin npm:@types/node@latest
#
# From a local directory (absolute or relative to this boxfile)
# ADD plugin /home/me/code/my-plugin
# ADD skill ./local-skill
#
# From a tarball (local file or remote URL)
# ADD plugin file:///home/me/backups/my-plugin.tar.gz
# ADD skill https://example.com/skill-pack.tar.gz
#
# Data payloads are copied verbatim (never linked into the repository)
# ADD data file:///home/me/models/model.bin
# ADD data ./datasets/seed.csv
";

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    if matches!(
        arguments.first().map(String::as_str),
        Some("help" | "--help" | "-h")
    ) {
        println!("dshbox init [path] [--force]");
        println!();
        println!("Write a sample `boxfile.dsh` to PATH (default: ./boxfile.dsh).");
        println!("Refuses to overwrite an existing file; pass --force to clobber.");
        return Ok(());
    }
    let force = arguments.iter().any(|argument| argument == "--force");
    // The first non-flag argument is the destination path; flags like
    // `--force` must never be mistaken for it.
    let destination = PathBuf::from(
        arguments
            .iter()
            .filter(|argument| !argument.starts_with('-'))
            .next()
            .map(String::as_str)
            .unwrap_or("boxfile.dsh"),
    );
    write_starter(&destination, force)
}

fn write_starter(destination: &Path, force: bool) -> Result<(), String> {
    if destination.exists() && !force {
        return Err(format!(
            "refusing to overwrite existing file {} (pass --force to clobber)",
            destination.display()
        ));
    }
    if let Some(parent) = destination.parent() {
        if !parent.as_os_str().is_empty() && !parent.is_dir() {
            return Err(format!(
                "parent directory does not exist: {}",
                parent.display()
            ));
        }
    }
    std::fs::write(destination, STARTER_BOXFILE)
        .map_err(|error| format!("cannot write {}: {error}", destination.display()))?;
    println!("wrote starter boxfile to {}", destination.display());
    println!("next: `dshbox build {}`", destination.display());
    Ok(())
}
