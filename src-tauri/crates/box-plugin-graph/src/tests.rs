//! Scanner and assembly tests.
//!
//! The extraction cases mirror the real forms found in a
//! `deepseek-ai/deepseek-harness` checkout, including the near-misses that must
//! not be admitted. Where a case comes from real code, the package it came from
//! is named in the test.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::extract::{mask, scan_source};
use crate::graph::{assemble, Discovered};
use crate::*;

fn scan(source: &str) -> extract::Scan {
    scan_source(source)
}

fn provides(source: &str) -> Vec<String> {
    scan(source).provides.into_iter().collect()
}

fn requires(source: &str) -> Vec<String> {
    scan(source).requires.into_iter().collect()
}

// ── masking ───────────────────────────────────────────────────────────────

#[test]
fn comments_are_blanked_from_both_views() {
    let source = "const a = 1 // inject: ['nope']\n/* super(ctx, 'nope') */\nconst b = 2\n";
    let masks = mask(source);
    let code = String::from_utf8(masks.code).unwrap();
    let literals = String::from_utf8(masks.literals).unwrap();
    assert_eq!(code.len(), source.len());
    assert_eq!(literals.len(), source.len());
    assert!(!code.contains("inject"));
    assert!(!literals.contains("inject"));
    assert!(!code.contains("nope"));
    // Line structure survives so diagnostics can report line numbers.
    assert_eq!(code.matches('\n').count(), 3);
}

#[test]
fn string_interiors_are_blanked_only_in_the_code_view() {
    let source = "const s = 'inject'\n";
    let masks = mask(source);
    let code = String::from_utf8(masks.code).unwrap();
    let literals = String::from_utf8(masks.literals).unwrap();
    assert!(!code.contains("inject"));
    assert!(literals.contains("inject"));
}

#[test]
fn escaped_quotes_do_not_end_a_literal() {
    let source = "const s = 'a\\'b' ; inject = ['after']\n";
    assert_eq!(requires(source), vec!["after"]);
}

// ── requires ──────────────────────────────────────────────────────────────

#[test]
fn module_level_export_is_read() {
    // e.g. packages/shell/tool-bash/src/index.ts
    assert_eq!(
        requires("export const inject = ['tools', 'shell', 'systemPrompt', 'shellEnv']\n"),
        vec!["shell", "shellEnv", "systemPrompt", "tools"]
    );
}

#[test]
fn class_static_and_static_override_are_read() {
    // e.g. packages/shell/pwsh-local/src/index.ts (static)
    assert_eq!(requires("  static inject = ['subprocess']\n"), vec!["subprocess"]);
    // e.g. packages/shell/bash-sandbox/src/index.ts (static override)
    assert_eq!(
        requires("  static override inject = ['subprocess', 'sandbox', 'sandboxPolicy']\n"),
        vec!["sandbox", "sandboxPolicy", "subprocess"]
    );
}

#[test]
fn a_type_annotation_before_the_value_is_stepped_over() {
    // e.g. packages/shell/shell-env/src/index.ts: `export const inject: string[] = []`
    assert_eq!(requires("export const inject: string[] = []\n"), Vec::<String>::new());
    assert_eq!(
        requires("export const inject: string[] = ['tools']\n"),
        vec!["tools"]
    );
}

#[test]
fn an_object_property_declares_only_what_something_registers() {
    // An exported binding is the plugin object the loader is handed, so its
    // property is a declaration — including when the export is an
    // `Object.assign`ed function, which is how DSH writes a plugin that is also
    // a callback.
    assert_eq!(
        requires("export const plugin = Object.assign(fn, {\n  inject: ['sessions'],\n})\n"),
        vec!["sessions"]
    );
    assert_eq!(
        requires("export const SessionMediaReferences = { inject: ['connection', 'fs'] }\n"),
        vec!["connection", "fs"]
    );
    // `ctx.plugin({ … })` hands the object to cordis directly.
    assert_eq!(
        requires("ctx.plugin({ name: 'tools', inject: ['tools', 'systemPrompt'] })\n"),
        vec!["systemPrompt", "tools"]
    );
}

#[test]
fn an_unregistered_object_property_is_not_a_dependency() {
    // The invariant companions of a real checkout: the `inject` here is attached
    // to an `InvariantInstaller` callback and documents the services a child
    // installer fiber may reach. It is read by the invariants service, never by
    // cordis, so the package does not wait on it. 26 of that checkout's 39
    // `invariant.ts` files are written this way, 17 of them naming `sessions`,
    // and reading them invented a load cycle the container never had.
    assert_eq!(
        requires(concat!(
            "const install: InvariantInstaller = Object.assign((ctx: Context, fail) => {\n",
            "  ctx.on('session/event', () => fail('x'))\n",
            "}, { inject: ['sessions'] })\n",
        )),
        Vec::<String>::new()
    );
    // A bare local object is not a plugin either.
    assert_eq!(requires("const p = { inject: ['llm', 'tools'] }\n"), Vec::<String>::new());
    // Nor is a descriptor passed to something else — the slot registry takes an
    // `inject` key that names no service anything provides.
    assert_eq!(
        requires("ctx.slots.register({ id: 'language', inject: injected }, LanguageRow)\n"),
        Vec::<String>::new()
    );
}

#[test]
fn a_returned_plugin_object_is_read() {
    // e.g. dsh-client-test-runtime/src/assembly/remote-proxies.ts: the factory
    // builds a `ClientPluginModule` and its caller mounts it, so this `inject` is
    // a real wait even though nothing at the site registers it.
    assert_eq!(
        requires(concat!(
            "export function remoteProxiesPlugin(namespaces: readonly string[]): ClientPluginModule {\n",
            "  return {\n",
            "    inject: ['connection'],\n",
            "    apply(ctx: Context) { ctx.provide('x', null) },\n",
            "  }\n",
            "}\n",
        )),
        vec!["connection"]
    );
    // An arrow's implicit return is the same thing.
    assert_eq!(
        requires("const make = () => ({ inject: ['tools'], apply(ctx) {} })\n"),
        vec!["tools"]
    );
}

#[test]
fn an_unregistered_property_is_not_reported_as_unresolved() {
    // Staying silent is the point: these sites are routine, so a note per site
    // would be noise in the diagnostics rather than something to act on.
    assert!(scan("const p = { inject: ['llm'] }\n").unresolved.is_empty());
    // An *exported* property that cannot be read as a literal still reports,
    // because that one really is a declaration.
    assert_eq!(scan("export const plugin = { inject: [name] }\n").unresolved.len(), 1);
}

#[test]
fn object_form_keys_are_read() {
    assert_eq!(
        requires("static inject = { shell: true, 'token-meter': {} }\n"),
        vec!["shell", "token-meter"]
    );
}

#[test]
fn scoped_inject_call_is_read() {
    // e.g. packages/shell/bash-local/src/index.ts: ctx.inject(['settings'], cb)
    assert_eq!(
        requires("ctx.inject(['settings'], (settingsCtx) => { use(settingsCtx) })\n"),
        vec!["settings"]
    );
    // Other context spellings in the real sources: `_ctx`, `agentCtx`, `this.ctx`.
    assert_eq!(requires("_ctx.inject(['tools'], cb)\n"), vec!["tools"]);
    assert_eq!(requires("agentCtx.inject(['sessions'], cb)\n"), vec!["sessions"]);
    assert_eq!(requires("this.ctx.inject(['settings'], cb)\n"), vec!["settings"]);
}

#[test]
fn the_slot_api_is_not_a_service_requirement() {
    // DSH's slot API has the same call shape as scoped injection but claims a UI
    // extension point, not a service. Real sites: packages/client/ui-sidebar/
    // src/client/index.ts:71 `ctx.slots.inject('sidebar', ...)` and ui-brand-official
    // `ctx.slots.inject('sidebar.brand.mark', ...)`. Reading these as services put
    // 15 names on the missing list for a container that starts fine.
    assert!(requires("ctx.slots.inject('sidebar', () => ctx.slots.register({ name: 'sidebar' }, Seat))\n").is_empty());
    assert!(requires("slots.inject('main', function* () { yield slots.register({ name: 'main' }, Panel) })\n").is_empty());
    assert!(requires("scope.slots.inject('tool.call.images', () => {})\n").is_empty());
    // A third-party collaborator object is not a context either.
    assert!(requires("controller.inject()\n").is_empty());
    assert!(requires("agent.inject(createUserMessage({ content: [] }))\n").is_empty());
}

#[test]
fn a_type_position_declares_nothing_and_is_not_reported() {
    // cordis itself declares `inject?: Inject`; that is a type, not a plugin
    // requirement, so neither a requirement nor a diagnostic may come out of it.
    let result = scan("export interface Base {\n  inject?: Inject\n}\n");
    assert!(result.requires.is_empty());
    assert!(result.unresolved.is_empty());
}

#[test]
fn a_method_named_inject_is_not_a_dependency_set() {
    // e.g. packages/core/agent/src/runtime-types.ts declares
    // `inject(message: UserMessage): void`. Only a property call is a scoped
    // injection; a declaration must not be read as one.
    let result = scan("export interface Agent {\n  inject(message: UserMessage): void\n}\n");
    assert!(result.requires.is_empty());
    assert!(result.unresolved.is_empty());
    // The call form still reads.
    assert_eq!(
        requires("ctx.inject(['typert'], (typeCtx) => { use(typeCtx) })\n"),
        vec!["typert"]
    );
}

#[test]
fn a_computed_name_is_reported_rather_than_guessed() {
    let result = scan("export const inject = [SERVICE_NAME]\n");
    assert!(result.requires.is_empty());
    assert_eq!(result.unresolved.len(), 1);
    assert!(result.unresolved.iter().next().unwrap().contains("non-literal"));
}

// ── provides ──────────────────────────────────────────────────────────────

#[test]
fn a_literal_super_call_names_a_service() {
    // e.g. packages/shell/shell/src/index.ts: ShellExecutor extends Service
    assert_eq!(
        provides("export abstract class ShellExecutor extends Service {\n  constructor(ctx: Context) {\n    super(ctx, 'shell')\n  }\n}\n"),
        vec!["shell"]
    );
    assert_eq!(
        provides("class ToolRuntime extends Service {\n  constructor(ctx: Context) {\n    super(ctx, 'tools')\n  }\n}\n"),
        vec!["tools"]
    );
}

#[test]
fn a_config_passthrough_super_call_is_not_a_service() {
    // e.g. packages/shell/bash-sandbox/src/index.ts:
    // SandboxBashExecutor extends LocalBashExecutor and calls super(ctx, config).
    // Reading that as a service name would invent a provider.
    let result = scan("class SandboxBashExecutor extends LocalBashExecutor {\n  constructor(ctx: Context, config: Config) {\n    super(ctx, config)\n  }\n}\n");
    assert!(result.provides.is_empty());
    assert!(result.unresolved.is_empty());
}

#[test]
fn an_error_class_super_call_is_not_a_service() {
    // e.g. packages/core/tools/src/ptc.ts:
    // `class PtcError extends Error { super(message, 'CODE_RUN_FAILED') }`.
    // The trailing literal is an error code; a rule keyed on the literal alone
    // would turn every error code in the tree into a service.
    let result = scan("export class PtcError extends Error {\n  constructor(message: string) {\n    super(message, 'CODE_RUN_FAILED')\n  }\n}\n");
    assert!(result.provides.is_empty());
    assert!(result.unresolved.is_empty());
}

#[test]
fn provide_call_and_field_are_read() {
    assert_eq!(provides("ctx.provide('extra', value)\n"), vec!["extra"]);
    assert_eq!(provides("ctx.reflect.provide('reflected', value)\n"), vec!["reflected"]);
    assert_eq!(provides("const plugin = { provide: 'declared' }\n"), vec!["declared"]);
    assert_eq!(provides("const plugin = { provide: ['a', 'b'] }\n"), vec!["a", "b"]);
}

#[test]
fn provide_like_identifiers_are_not_services() {
    // `provider` must not match `provide`, and a `provide` in a comment (which is
    // how the real DSH sources mention ctx.provide) must not either.
    let result = scan("const provider = makeProvider()\n// set with ctx.provide('ghost') before load\n");
    assert!(result.provides.is_empty());
    assert!(result.unresolved.is_empty());
}

// ── catalogue ─────────────────────────────────────────────────────────────

#[test]
fn the_context_augmentation_is_read_and_events_are_not() {
    // e.g. packages/core/tools/src/index.ts declares both interfaces in one
    // `declare module`; only Context keys are services.
    let source = "declare module '@deepseek-ai/cordis' {\n  interface Context {\n    tools: ToolRuntime\n  }\n\n  interface Events {\n    'tools/approve': (data: Data) => void\n  }\n}\n";
    let result = scan(source);
    let catalogue: Vec<String> = result.catalogue.into_iter().collect();
    assert_eq!(catalogue, vec!["tools"]);
}

// ── assembly ──────────────────────────────────────────────────────────────

fn discovered(name: &str, provides: &[&str], requires: &[&str]) -> Discovered {
    Discovered {
        name: name.to_owned(),
        version: Some("1.0.0".to_owned()),
        source: format!("packages/{name}"),
        scan: extract::Scan {
            provides: provides.iter().map(|value| (*value).to_owned()).collect(),
            requires: requires.iter().map(|value| (*value).to_owned()).collect(),
            ..Default::default()
        },
    }
}

fn activated(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn build(activated_names: &[&str], plugins: Vec<Discovered>) -> PluginGraph {
    assemble(
        GraphSource::Template,
        "demo".to_owned(),
        "web".to_owned(),
        Some(activated(activated_names)),
        plugins,
        Vec::new(),
        1_700_000_000,
    )
}

/// A profile tree that is not on disk yet: activation is unknown, so the graph
/// reports nothing as inactive instead of reporting everything.
fn build_unknown_activation(plugins: Vec<Discovered>) -> PluginGraph {
    assemble(
        GraphSource::Template,
        "demo".to_owned(),
        "web".to_owned(),
        None,
        plugins,
        Vec::new(),
        1_700_000_000,
    )
}

#[test]
fn a_namespaced_requirement_is_satisfied_by_its_root_service() {
    // The client plugins inject `remote.session` on top of the `remote` service
    // the API gateway provides (packages/api/session-controller/src/client/
    // transport.ts, "Complete generated ctx.remote.session namespace"). Reading
    // the dotted name as a service of its own reported 68 services as missing
    // when the real graph has 4.
    let graph = build(
        &["gateway", "chat"],
        vec![
            discovered("gateway", &["remote"], &[]),
            discovered("chat", &[], &["remote", "remote.session"]),
        ],
    );
    assert!(graph.missing.is_empty());
    // Both requirements resolve to the one service, so both link to its provider.
    assert_eq!(graph.links.len(), 2);
    assert!(graph.links.iter().all(|link| link.to == "gateway"));
    assert!(graph.links.iter().all(|link| link.service == "remote"));
    // `remote.session` is a path into `remote`, not a service node of its own.
    assert_eq!(graph.services, vec!["remote".to_owned()]);
    assert!(graph.requires.iter().all(|edge| edge.service == "remote"));
}

#[test]
fn a_namespaced_requirement_without_its_root_is_still_missing() {
    // Only a prefix somebody really provides counts. `settings.section` must not
    // be satisfied by the unrelated `settings.general` provider next to it.
    let graph = build(
        &["settings", "panel"],
        vec![
            discovered("settings", &["settings.general"], &[]),
            discovered("panel", &[], &["settings.section"]),
        ],
    );
    assert_eq!(graph.missing.len(), 1);
    assert_eq!(graph.missing[0].service, "settings.section");
    // An unresolved requirement keeps its declared name, which is what it needs.
    assert!(graph.services.contains(&"settings.section".to_owned()));
}

#[test]
fn a_root_requirement_is_not_satisfied_by_a_namespaced_provider() {
    // The relationship is directional: `a.b` lives inside `a`, so providing the
    // path does not provide the root.
    let graph = build(
        &["inner", "outer"],
        vec![
            discovered("inner", &["remote.session"], &[]),
            discovered("outer", &[], &["remote"]),
        ],
    );
    assert_eq!(graph.missing.len(), 1);
    assert_eq!(graph.missing[0].service, "remote");
}

#[test]
fn a_derived_link_points_from_the_dependent_at_the_provider() {
    let graph = build(
        &["a", "b"],
        vec![
            discovered("a", &[], &["tools"]),
            discovered("b", &["tools"], &[]),
        ],
    );
    assert_eq!(graph.links.len(), 1);
    assert_eq!(graph.links[0].from, "a");
    assert_eq!(graph.links[0].to, "b");
    assert_eq!(graph.links[0].service, "tools");
}

#[test]
fn the_order_puts_dependencies_before_dependents() {
    // `a` requires what `b` provides, so `b` must load first. This is the one
    // place a topological sort can be silently backwards.
    let graph = build(
        &["a", "b"],
        vec![
            discovered("a", &[], &["tools"]),
            discovered("b", &["tools"], &[]),
        ],
    );
    assert!(graph.cycles.is_empty());
    assert_eq!(graph.order, vec!["b", "a"]);
}

#[test]
fn a_chain_is_ordered_end_to_end() {
    let graph = build(
        &["a", "b", "c"],
        vec![
            discovered("a", &[], &["s-b"]),
            discovered("b", &["s-b"], &["s-c"]),
            discovered("c", &["s-c"], &[]),
        ],
    );
    assert_eq!(graph.order, vec!["c", "b", "a"]);
}

#[test]
fn a_service_nobody_provides_is_reported_as_missing() {
    let graph = build(&["a"], vec![discovered("a", &[], &["nowhere"])]);
    assert_eq!(graph.missing.len(), 1);
    assert_eq!(graph.missing[0].plugin, "a");
    assert_eq!(graph.missing[0].service, "nowhere");
    assert!(graph.links.is_empty());
}

#[test]
fn a_provider_outside_the_activation_closure_is_reported() {
    // `b` is installed but not activated, so the service it provides is never
    // registered and `a` would wait forever.
    let graph = build(
        &["a"],
        vec![
            discovered("a", &[], &["tools"]),
            discovered("b", &["tools"], &[]),
        ],
    );
    assert_eq!(graph.inactive_providers.len(), 1);
    assert_eq!(graph.inactive_providers[0].plugin, "a");
    assert_eq!(graph.inactive_providers[0].service, "tools");
    // The provider is active in the equivalent graph, so nothing is reported.
    let healthy = build(
        &["a", "b"],
        vec![
            discovered("a", &[], &["tools"]),
            discovered("b", &["tools"], &[]),
        ],
    );
    assert!(healthy.inactive_providers.is_empty());
}

#[test]
fn plugins_that_depend_on_each_other_are_reported_as_a_cycle() {
    let graph = build(
        &["a", "b"],
        vec![
            discovered("a", &["s-b"], &["s-a"]),
            discovered("b", &["s-a"], &["s-b"]),
        ],
    );
    assert_eq!(graph.cycles, vec![vec!["a".to_owned(), "b".to_owned()]]);
    // The four plugins of a 272-plugin graph disagreed, and that cost the reader
    // the whole load order panel. The order is still emitted, with the cycle as
    // one block: `cycles` is what says the order inside it means nothing.
    assert_eq!(graph.order.len(), 2);
    assert!(graph.order.contains(&"a".to_owned()));
    assert!(graph.order.contains(&"b".to_owned()));
}

#[test]
fn plugins_outside_a_cycle_keep_their_order() {
    // The contract the cycle must not break: every edge that does not stay inside
    // one cycle group still points forwards. `c` consumes from the cycle, so it
    // reads after both members however the block itself is ordered.
    let graph = build(
        &["a", "b", "c", "d"],
        vec![
            discovered("a", &["s-b"], &["s-a"]),
            discovered("b", &["s-a"], &["s-b"]),
            discovered("c", &[], &["s-a"]),
            discovered("d", &[], &[]),
        ],
    );
    assert_eq!(graph.cycles.len(), 1);
    assert_eq!(graph.order.len(), 4, "every plugin is still listed");
    let position = |name: &str| graph.order.iter().position(|entry| entry == name).unwrap();
    let cycled: BTreeSet<&str> = graph.cycles[0].iter().map(String::as_str).collect();
    for link in &graph.links {
        if link.from == link.to || (cycled.contains(link.from.as_str()) && cycled.contains(link.to.as_str())) {
            continue;
        }
        assert!(
            position(&link.to) < position(&link.from),
            "{} provides for {} but is listed after it",
            link.to,
            link.from,
        );
    }
}

#[test]
fn a_plugin_depending_on_a_cycle_is_not_reported_as_part_of_it() {
    // `c` merely consumes a service from the cycle. Cordis would leave `c`
    // pending too, but the cycle itself is `a`/`b`, and saying otherwise would
    // send the reader to the wrong plugin.
    let graph = build(
        &["a", "b", "c"],
        vec![
            discovered("a", &["s-b"], &["s-a"]),
            discovered("b", &["s-a"], &["s-b"]),
            discovered("c", &[], &["s-a"]),
        ],
    );
    assert_eq!(graph.cycles, vec![vec!["a".to_owned(), "b".to_owned()]]);
}

#[test]
fn a_plugin_that_provides_what_it_requires_is_satisfied() {
    let graph = build(&["a"], vec![discovered("a", &["own"], &["own"])]);
    assert!(graph.missing.is_empty());
    assert!(graph.inactive_providers.is_empty());
    assert!(graph.links.is_empty());
}

// ── service conflicts ─────────────────────────────────────────────────────

#[test]
fn a_service_with_two_loaded_providers_is_reported_as_a_conflict() {
    // The real web profile registers `sessions` from both the session controller
    // and the core session plugin. Nothing in the drawing says so, and the
    // fan-out it causes is what a reader sees as a hairball.
    let graph = build(
        &["a", "b", "consumer"],
        vec![
            discovered("a", &["s"], &[]),
            discovered("b", &["s"], &[]),
            discovered("consumer", &[], &["s"]),
        ],
    );
    assert_eq!(graph.conflicts.len(), 1);
    assert_eq!(graph.conflicts[0].service, "s");
    assert_eq!(graph.conflicts[0].providers, vec!["a".to_owned(), "b".to_owned()]);
}

#[test]
fn an_unloaded_second_provider_is_not_a_conflict() {
    // `b` never registers `s`, so there is nothing to conflict with. Counting it
    // would report a conflict on the real container for `remote`, `fileUpload`
    // and `workspaces`, whose only other provider is the unloaded test runtime.
    let graph = build(
        &["a", "consumer"],
        vec![
            discovered("a", &["s"], &[]),
            discovered("b", &["s"], &[]),
            discovered("consumer", &[], &["s"]),
        ],
    );
    assert!(graph.conflicts.is_empty());
}

#[test]
fn a_single_provider_is_not_a_conflict() {
    let graph = build(
        &["a", "consumer"],
        vec![
            discovered("a", &["s"], &[]),
            discovered("consumer", &[], &["s"]),
        ],
    );
    assert!(graph.conflicts.is_empty());
}

#[test]
fn a_conflict_is_reported_even_when_providing_one_side_is_also_the_consumer() {
    // The real `sessions` pair are both self-providers: each registers the service
    // and injects it. That idiom is why neither is resolved for the other, but it
    // does not make two owners of one name any less of a conflict.
    let graph = build(
        &["a", "b"],
        vec![
            discovered("a", &["s"], &["s"]),
            discovered("b", &["s"], &["s"]),
        ],
    );
    assert_eq!(graph.conflicts.len(), 1);
    assert_eq!(graph.conflicts[0].service, "s");
}

// ── end to end over a tree ────────────────────────────────────────────────

fn sandbox(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("dshbox-graph-{label}-{nanos}"));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

fn write_package(directory: &PathBuf, name: &str, dependencies: &[&str], source: &str) {
    fs::create_dir_all(directory.join("src")).unwrap();
    let mut manifest = serde_json::json!({ "name": name, "version": "1.0.0" });
    if !dependencies.is_empty() {
        let mut map = serde_json::Map::new();
        for dependency in dependencies {
            map.insert((*dependency).to_owned(), serde_json::json!("workspace:^"));
        }
        manifest["dependencies"] = serde_json::Value::Object(map);
    }
    fs::write(
        directory.join("package.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(directory.join("src").join("index.ts"), source).unwrap();
}

#[test]
fn builds_a_graph_from_a_template_tree() {
    let root = sandbox("tree");
    let harness = root.join("harness/packages");
    // Core plugins, one per group directory, as the real harness lays them out.
    write_package(
        &harness.join("core/subprocess"),
        "@deepseek-ai/dsh-subprocess",
        &[],
        "export class Subprocess extends Service {\n  constructor(ctx) { super(ctx, 'subprocess') }\n}\n",
    );
    write_package(
        &harness.join("core/tools"),
        "@deepseek-ai/dsh-tools",
        &["@deepseek-ai/dsh-subprocess"],
        "export const inject = ['subprocess']\nexport class ToolRuntime extends Service {\n  constructor(ctx) { super(ctx, 'tools') }\n}\n",
    );
    let profile = root.join("profile/profiles/web");
    fs::create_dir_all(&profile).unwrap();
    // The boxfile activated only the tools bundle, which depends on subprocess.
    fs::write(
        profile.join("package.json"),
        serde_json::json!({
            "name": "dsh-profile-web",
            "dsh": { "profile": { "bundles": ["@deepseek-ai/dsh-tools"] } }
        })
        .to_string(),
    )
    .unwrap();

    let graph = build_graph(
        GraphSource::Template,
        "demo",
        "web",
        &ScanRoots {
            harness: Some(root.join("harness")),
            profile: Some(profile),
            repository: None,
        },
        1_700_000_000,
    );

    let names: Vec<&str> = graph.plugins.iter().map(|plugin| plugin.name.as_str()).collect();
    assert_eq!(names, vec!["@deepseek-ai/dsh-subprocess", "@deepseek-ai/dsh-tools"]);
    // Activation follows the bundle's own dependencies, not just the layer name.
    assert!(graph
        .plugins
        .iter()
        .all(|plugin| plugin.activated));
    assert_eq!(graph.order, vec!["@deepseek-ai/dsh-subprocess", "@deepseek-ai/dsh-tools"]);
    assert!(graph.missing.is_empty());
    assert!(graph.inactive_providers.is_empty());
    assert_eq!(graph.services, vec!["subprocess", "tools"]);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_profile_without_a_manifest_reports_a_diagnostic_instead_of_failing() {
    let root = sandbox("no-manifest");
    fs::create_dir_all(root.join("profile/profiles/web")).unwrap();
    let graph = build_graph(
        GraphSource::Container,
        "container-1",
        "web",
        &ScanRoots {
            harness: None,
            profile: Some(root.join("profile/profiles/web")),
            repository: None,
        },
        1_700_000_000,
    );
    assert!(graph.plugins.is_empty());
    assert!(graph
        .diagnostics
        .iter()
        .any(|line| line.contains("no profile manifest")));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn activation_comes_from_the_patch_layers_in_order() {
    // The composition DSH uses: each bundle's `cordis.patch.yml` inserts rows, the
    // profile's own layer is applied last, and a later row addressed at an id
    // replaces the earlier one — including switching it off. Reading the bundle's
    // `package.json` dependencies instead counted packages that are never entries,
    // and never saw `disabled`, which is how the graph came to call plugins
    // unloaded that DSH itself loads.
    let root = sandbox("patch-layers");
    let profile = root.join("profile/profiles/web");
    let bundle = root.join("packages/base");
    let timer = root.join("packages/cordis/plugin-timer");
    let bash = root.join("packages/tool/bash");
    let dependency = root.join("packages/util/never-inserted");
    for directory in [&profile, &bundle, &timer, &bash, &dependency] {
        fs::create_dir_all(directory).unwrap();
    }
    fs::write(
        profile.join("package.json"),
        r#"{"name":"profile","dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base"]}}}"#,
    )
    .unwrap();
    fs::write(
        bundle.join("package.json"),
        r#"{"name":"@deepseek-ai/dsh-base","dependencies":{"@deepseek-ai/dsh-never-inserted":"1"}}"#,
    )
    .unwrap();
    fs::write(
        bundle.join("cordis.patch.yml"),
        "- insert:\n    - id: timer\n      name: '@deepseek-ai/cordis-plugin-timer'\n\n    - id: tool-bash\n      name: '@deepseek-ai/dsh-tool-bash'\n",
    )
    .unwrap();
    for (directory, name) in [
        (&timer, "@deepseek-ai/cordis-plugin-timer"),
        (&bash, "@deepseek-ai/dsh-tool-bash"),
        (&dependency, "@deepseek-ai/dsh-never-inserted"),
    ] {
        fs::write(
            directory.join("package.json"),
            format!(r#"{{"name":"{name}","version":"1.0.0"}}"#),
        )
        .unwrap();
    }
    // The profile layer disables one of the rows the bundle inserted.
    fs::write(profile.join("cordis.patch.yml"), "- id: tool-bash\n  disabled: true\n").unwrap();

    let graph = build_graph(
        GraphSource::Container,
        "container-1",
        "web",
        &ScanRoots {
            // The harness root: the scanner appends `packages` and `vendor` itself.
            harness: Some(root.clone()),
            profile: Some(profile),
            repository: None,
        },
        1_700_000_000,
    );
    let activated = |name: &str| {
        graph
            .plugins
            .iter()
            .find(|plugin| plugin.name == name)
            .is_some_and(|plugin| plugin.activated)
    };
    // The bundle's row is enabled by default and stays enabled.
    assert!(activated("@deepseek-ai/cordis-plugin-timer"));
    // The profile layer switched this row off, and a disabled row outranks the
    // dependency closure, which reaches the same package.
    assert!(!activated("@deepseek-ai/dsh-tool-bash"));
    // The closure is deliberately a superset: a dependency that no patch inserts
    // still counts as loaded, because that is the only way to see the builtins DSH
    // mounts without a patch row. The cost is a few extra nodes, never a plugin
    // wrongly reported as unloaded.
    assert!(activated("@deepseek-ai/dsh-never-inserted"));
    assert!(graph.inactive_providers.is_empty());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_wire_shape_round_trips() {
    let graph = build(
        &["a", "b"],
        vec![
            discovered("a", &[], &["tools"]),
            discovered("b", &["tools"], &[]),
        ],
    );
    let rendered = serde_json::to_string(&graph).unwrap();
    let parsed: PluginGraph = serde_json::from_str(&rendered).unwrap();
    assert_eq!(parsed, graph);
    // The desktop adapter deserializes this shape, so a rename here is a wire
    // break for the UI.
    let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert!(value.get("sourceId").is_some());
    assert!(value.get("inactiveProviders").is_some());
    assert!(value.get("conflicts").is_some());
    assert!(value.get("scannedAt").is_some());
    assert!(value["plugins"][0].get("activated").is_some());
}

#[test]
fn an_unknown_profile_reports_nothing_as_inactive() {
    // A prepared Harness base has no profile tree, so the graph cannot tell which
    // plugins the profile loads. Reading that as "none are loaded" reported all
    // 685 providers in the real container graph as inactive providers; the honest
    // answer is that the question is unanswerable here.
    let graph = build_unknown_activation(vec![
        discovered("provider", &["s"], &[]),
        discovered("consumer", &[], &["s"]),
    ]);
    assert!(graph.inactive_providers.is_empty());
    assert!(graph.missing.is_empty());
    assert!(graph.plugins.iter().all(|plugin| plugin.activated));
    // The dependency itself is still reported, so the graph keeps its value.
    assert_eq!(graph.links.len(), 1);
}

#[test]
fn an_unloaded_plugin_reports_no_diagnostics() {
    // Diagnostics describe the tree the profile loads. A plugin that is installed
    // but not activated neither waits nor runs, so a requirement of its own says
    // nothing about the container — the real one listed 16 rows of an unloaded
    // plugin waiting on an unloaded provider while DSH's startup audit reported
    // nothing pending at all.
    let graph = build(
        &["provider"],
        vec![
            discovered("provider", &["s"], &[]),
            discovered("spare", &[], &["s", "nowhere"]),
        ],
    );
    assert!(graph.missing.is_empty());
    assert!(graph.inactive_providers.is_empty());
    // The edges stay, so the UI can still show the plugin when asked for it.
    assert_eq!(graph.links.len(), 1);
}
