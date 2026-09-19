//! Graph assembly: turn per-package declaration sets into the service bipartite
//! graph, the derived plugin-to-plugin edges, and the diagnostics that make a
//! runtime hang visible at inspection time.

use std::collections::{BTreeMap, BTreeSet};

use crate::extract::Scan;
use crate::{GraphPlugin, GraphSource, PluginGraph, PluginLink, ServiceEdge, SharedService};

/// One discovered plugin package and what its sources declared.
#[derive(Clone, Debug)]
pub struct Discovered {
    pub name: String,
    pub version: Option<String>,
    /// Display path, relative to the scanned root where possible.
    pub source: String,
    pub scan: Scan,
}

/// Assemble the graph. `activated` is the profile's activation closure, which
/// the caller computes from `dsh.profile.bundles` and its dependency closure;
/// `None` means the profile tree is absent and activation is unknown, in which
/// case nothing is reported as inactive. Claiming a plugin is unloaded when the
/// graph simply cannot see the profile would send a reader hunting for a problem
/// that is not there.
pub fn assemble(
    source: GraphSource,
    source_id: String,
    profile: String,
    activated: Option<BTreeSet<String>>,
    discovered: Vec<Discovered>,
    diagnostics: Vec<String>,
    scanned_at: u64,
) -> PluginGraph {
    let is_activated = |name: &str| activated.as_ref().is_none_or(|names| names.contains(name));
    // Providers per service, and the full service set. Everything downstream is
    // built from these two maps.
    let mut providers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut services: BTreeSet<String> = BTreeSet::new();
    let mut provides_edges: Vec<ServiceEdge> = Vec::new();
    let mut requires_edges: Vec<ServiceEdge> = Vec::new();

    for plugin in &discovered {
        for service in &plugin.scan.provides {
            providers
                .entry(service.clone())
                .or_default()
                .insert(plugin.name.clone());
            services.insert(service.clone());
            provides_edges.push(ServiceEdge {
                plugin: plugin.name.clone(),
                service: service.clone(),
            });
        }
    }

    // A service that is required but provided by nobody means the requiring
    // plugin hangs forever at load: cordis waits rather than failing.
    let mut missing: Vec<ServiceEdge> = Vec::new();
    let mut links: Vec<PluginLink> = Vec::new();
    // Services required by an activated plugin but provided only from outside
    // the activation closure: the provider is installed but never loaded, so the
    // requirement can never be satisfied.
    let mut inactive_providers: Vec<ServiceEdge> = Vec::new();
    // Requirements resolved to a service nobody declared: the declared name, not
    // the request, is what a reader needs to see in the diagnostics.
    let mut unresolved: BTreeSet<String> = BTreeSet::new();

    for plugin in &discovered {
        // Only what the profile loads can hang. A plugin that is installed but not
        // activated neither waits for anything nor runs, so reporting its
        // requirements describes a tree that does not exist — for the real
        // container that was 16 rows of an unloaded plugin waiting on an unloaded
        // provider, while DSH's own startup audit reported nothing pending.
        let consumer_runs = is_activated(&plugin.name);
        for service in &plugin.scan.requires {
            let declared = service.as_str();
            match resolve_service(&providers, declared) {
                None => {
                    unresolved.insert(declared.to_owned());
                    requires_edges.push(ServiceEdge {
                        plugin: plugin.name.clone(),
                        service: declared.to_owned(),
                    });
                    if consumer_runs {
                        missing.push(ServiceEdge {
                            plugin: plugin.name.clone(),
                            service: declared.to_owned(),
                        });
                    }
                }
                Some((provided, names)) => {
                    requires_edges.push(ServiceEdge {
                        plugin: plugin.name.clone(),
                        service: provided.to_owned(),
                    });
                    // A plugin that provides the service itself is waiting for its
                    // own registration, not for somebody else's: that is cordis's
                    // idiom for "ready once my service exists". Deriving edges to
                    // the service's other providers anyway invented both directions
                    // between DSH's two `sessions` providers and reported a cycle
                    // the running tree does not have.
                    let self_provided = plugin.scan.provides.contains(provided);
                    let mut satisfied_here = self_provided;
                    if !self_provided {
                        for provider in names {
                            links.push(PluginLink {
                                from: plugin.name.clone(),
                                to: provider.clone(),
                                service: provided.to_owned(),
                            });
                            if is_activated(provider) {
                                satisfied_here = true;
                            }
                        }
                    }
                    if !satisfied_here && consumer_runs {
                        inactive_providers.push(ServiceEdge {
                            plugin: plugin.name.clone(),
                            service: provided.to_owned(),
                        });
                    }
                }
            }
        }
    }

    // Service nodes are the provided names plus whatever is left over unresolved;
    // a satisfied `a.b` is not a service of its own, it is a path into `a`.
    services.extend(unresolved);

    let plugins: Vec<GraphPlugin> = discovered
        .iter()
        .map(|plugin| GraphPlugin {
            name: plugin.name.clone(),
            version: plugin.version.clone(),
            activated: is_activated(&plugin.name),
            source: plugin.source.clone(),
            provides: plugin.scan.provides.iter().cloned().collect(),
            requires: plugin.scan.requires.iter().cloned().collect(),
        })
        .collect();

    let names: Vec<String> = discovered.iter().map(|plugin| plugin.name.clone()).collect();
    let (order, cycles) = sort_plugins(&names, &links);

    // Compared on the activated plugins only: one the profile never loads does not
    // register anything, so counting it would describe a tree that is not running.
    let shared_services: Vec<SharedService> = providers
        .iter()
        .filter_map(|(service, names)| {
            let loaded: Vec<String> = names
                .iter()
                .filter(|name| is_activated(name))
                .cloned()
                .collect();
            (loaded.len() > 1).then(|| SharedService {
                service: service.clone(),
                providers: loaded,
            })
        })
        .collect();

    PluginGraph {
        source,
        source_id,
        profile,
        plugins,
        services: services.into_iter().collect(),
        requires: requires_edges,
        provides: provides_edges,
        links,
        order,
        cycles,
        missing,
        inactive_providers,
        shared_services,
        diagnostics,
        scanned_at,
    }
}

/// The service a requirement actually waits on.
///
/// cordis resolves `inject: ['a.b']` against the service `a` and then reads the
/// `b` path off it — the client plugins inject `remote.session` on top of the
/// `remote` service the API gateway provides. So a requirement is satisfied by the
/// provider of its longest dotted prefix, and only a prefix someone really
/// provides counts: without that, every namespaced requirement is reported as
/// missing and the diagnostics drown in false positives.
fn resolve_service<'a>(
    providers: &'a BTreeMap<String, BTreeSet<String>>,
    declared: &str,
) -> Option<(&'a str, &'a BTreeSet<String>)> {
    let mut candidate = declared;
    loop {
        if let Some((name, names)) = providers.get_key_value(candidate) {
            return Some((name.as_str(), names));
        }
        // `rfind` returns a char boundary, so the slice is valid UTF-8.
        candidate = &candidate[..candidate.rfind('.')?];
    }
}

/// Topological order of the plugin graph, plus the strongly connected components
/// that make an order impossible.
///
/// Kahn's algorithm alone cannot report cycles usefully — its leftovers also
/// include every node that merely *depends* on a cycle. Tarjan is run instead so
/// each reported group is exactly the plugins that depend on each other, which is
/// the set cordis would leave silently pending.
///
/// A cycle does not cost the reader the whole order. cordis leaves the cycle's
/// members pending and loads everything else, so the order is the condensation's
/// — each component emitted where its dependencies put it, a cyclic component as
/// one block whose internal order is arbitrary. Dropping the order entirely
/// withheld a usable list because four plugins of 272 disagreed: on the real web
/// profile the load order panel was empty and said so.
fn sort_plugins(names: &[String], links: &[PluginLink]) -> (Vec<String>, Vec<Vec<String>>) {
    let mut adjacency: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for name in names {
        adjacency.entry(name.as_str()).or_default();
    }
    // `from` requires `to`, so `to` must load first: the edge points at the
    // dependency, and the order is produced by emitting dependencies first.
    for link in links {
        if link.from == link.to {
            continue;
        }
        adjacency
            .entry(link.from.as_str())
            .or_default()
            .insert(link.to.as_str());
    }

    let components = strongly_connected(&adjacency);
    let mut cycles: Vec<Vec<String>> = components
        .iter()
        .filter(|component| {
            component.len() > 1
                || component
                    .first()
                    .is_some_and(|name| adjacency.get(name).is_some_and(|set| set.contains(name)))
        })
        .map(|component| {
            let mut group: Vec<String> = component.iter().map(|name| (*name).to_owned()).collect();
            group.sort();
            group
        })
        .collect();
    cycles.sort();

    // Tarjan emits each component only after every component reachable from it,
    // and an edge points from a dependent at its dependency, so its output is
    // already dependencies-first. Names inside a component are sorted for stable
    // output — for a cyclic component that order carries no meaning beyond being
    // reproducible, which is why `cycles` names the group.
    let mut order: Vec<String> = Vec::new();
    for component in components.iter() {
        let mut group: Vec<&str> = component.to_vec();
        group.sort_unstable();
        order.extend(group.into_iter().map(str::to_owned));
    }
    (order, cycles)
}

/// Tarjan's strongly connected components, iterative so a deep dependency chain
/// cannot overflow the stack.
fn strongly_connected<'a>(adjacency: &BTreeMap<&'a str, BTreeSet<&'a str>>) -> Vec<Vec<&'a str>> {
    struct Frame<'a> {
        node: &'a str,
        successors: Vec<&'a str>,
        next: usize,
    }

    let mut index_of: BTreeMap<&str, usize> = BTreeMap::new();
    let mut low: BTreeMap<&str, usize> = BTreeMap::new();
    let mut on_stack: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut components: Vec<Vec<&str>> = Vec::new();
    let mut counter = 0usize;

    for root in adjacency.keys().copied() {
        if index_of.contains_key(root) {
            continue;
        }
        let successors: Vec<&str> = adjacency
            .get(root)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default();
        let mut frames = vec![Frame {
            node: root,
            successors,
            next: 0,
        }];
        index_of.insert(root, counter);
        low.insert(root, counter);
        counter += 1;
        stack.push(root);
        on_stack.insert(root);

        while let Some(frame) = frames.last_mut() {
            if frame.next < frame.successors.len() {
                let successor = frame.successors[frame.next];
                frame.next += 1;
                if !index_of.contains_key(successor) {
                    index_of.insert(successor, counter);
                    low.insert(successor, counter);
                    counter += 1;
                    stack.push(successor);
                    on_stack.insert(successor);
                    let successors: Vec<&str> = adjacency
                        .get(successor)
                        .map(|set| set.iter().copied().collect())
                        .unwrap_or_default();
                    frames.push(Frame {
                        node: successor,
                        successors,
                        next: 0,
                    });
                } else if on_stack.contains(successor) {
                    let node = frame.node;
                    let candidate = index_of[successor];
                    let entry = low.entry(node).or_insert(usize::MAX);
                    *entry = (*entry).min(candidate);
                }
                continue;
            }

            let finished = frames.pop().expect("frame is present");
            let node = finished.node;
            if let Some(parent) = frames.last() {
                let parent_node = parent.node;
                let value = low[&node];
                let entry = low.entry(parent_node).or_insert(usize::MAX);
                *entry = (*entry).min(value);
            }
            if low[&node] == index_of[&node] {
                let mut component = Vec::new();
                while let Some(popped) = stack.pop() {
                    on_stack.remove(popped);
                    component.push(popped);
                    if popped == node {
                        break;
                    }
                }
                component.sort_unstable();
                components.push(component);
            }
        }
    }

    components
}
