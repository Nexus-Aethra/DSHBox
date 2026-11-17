//! command_surface for app.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    /// The command literals in `box-api.ts`, read from the source that ships.
    const FRONTEND: &str = include_str!("../../../src/shared/api/box-api.ts");

    fn quoted_literal_at(source: &str, open_paren: usize) -> Option<&str> {
        let rest = source.get(open_paren + 1..)?;
        let rest = rest.trim_start();
        let payload = rest.strip_prefix('\'')?;
        let end = payload.find('\'')?;
        Some(&payload[..end])
    }

    /// The first argument of every `ipc(...)` / `ipc<...>(...)` /
    /// `openContainerFront(...)` call: a command name the UI can ask for.
    fn frontend_commands() -> Vec<String> {
        let source = FRONTEND;
        let bytes = source.as_bytes();
        let mut names = Vec::new();
        for opener in ["ipc<", "ipc(", "openContainerFront("] {
            let mut from = 0;
            while let Some(offset) = source[from..].find(opener) {
                let start = from + offset;
                let after = start + opener.len();
                // `ipc<T>(` reaches its parenthesis through the generic list; a
                // declaration like `ipc<T>(command: string, …)` has no literal
                // first argument, so it is skipped by the quote check.
                let open_paren = if opener == "ipc<" {
                    match bytes[after..].iter().position(|byte| *byte == b'>') {
                        Some(relative) => {
                            let closing = after + relative;
                            source[closing..].find('(').map(|offset| closing + offset)
                        }
                        None => None,
                    }
                } else {
                    Some(after)
                };
                if let Some(name) = open_paren.and_then(|paren| quoted_literal_at(source, paren)) {
                    names.push(name.to_owned());
                }
                from = after;
            }
        }
        names
    }

    /// Command names in the `generate_handler!` list of this very file.
    fn registered_commands() -> Vec<String> {
        let source = include_str!("app.rs");
        let list = source
            .split("generate_handler![")
            .nth(1)
            .and_then(|rest| rest.split_once(']'))
            .map(|(list, _)| list)
            .expect("app.rs must contain a generate_handler! list");
        list.split(',')
            .map(|entry| entry.trim())
            .filter(|entry| !entry.is_empty())
            .map(|entry| entry.rsplit("::").next().unwrap_or(entry).to_owned())
            .collect()
    }

    #[test]
    fn every_command_the_frontend_names_is_registered() {
        let commands = frontend_commands();
        // A scan that quietly found nothing would make the assertion below vacuous.
        assert!(
            commands.len() > 50 && commands.iter().any(|name| name == "list_resource_type"),
            "the `box-api.ts` scan found {} commands, which means it stopped working: {commands:?}",
            commands.len()
        );
        let registered = registered_commands();
        let missing: Vec<String> = commands
            .into_iter()
            .filter(|name| !registered.iter().any(|entry| entry == name))
            .collect();
        assert!(
            missing.is_empty(),
            "commands the UI invokes but the desktop never registers (they work in \
             `pnpm dev` via the dev bridge and fail in a packaged build): {missing:?}"
        );
    }

    #[test]
    fn the_registration_list_has_no_duplicates() {
        let registered = registered_commands();
        let mut sorted = registered.clone();
        sorted.sort();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            before,
            "a command registered twice would shadow itself"
        );
    }
