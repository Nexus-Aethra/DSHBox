# @deepseek-ai/dsh-box-context

A DeepSeek Harness plugin that injects the DSH Box container context into
the agent system prompt via the official ctx.systemPrompt.context capability
seam. The context carries structured per-container metadata (id, name,
version, profile, paths, credentials) as a user-role history snapshot, so
the agent reliably treats it as fact rather than prompt decoration.

## Status

This iteration ships the host half only: Box has no WebView tab to extend,
so no client bundle is produced. The plugin reads a JSON file at runtime
and re-renders on file change.

## Build

    pnpm install
    pnpm build
