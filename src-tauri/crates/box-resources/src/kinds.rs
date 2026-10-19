//! The kind registry: where a resource lives in a container, whether it holds
//! secrets, and what shape its payload has (which decides what merging means).

use serde::{Deserialize, Serialize};

/// How an injection treats a destination that is already there. `Refuse` is the
/// default everywhere: overwriting container state is always an explicit act.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Conflict {
    Refuse,
    Overwrite,
    /// Merge entry by entry; the injected payload wins where both sides have
    /// the same entry. Only meaningful for [`Shape::Entries`] payloads and for
    /// YAML maps — anything else is rejected rather than silently replaced.
    Merge,
}

impl Conflict {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "refuse" | "" => Some(Self::Refuse),
            "overwrite" | "replace" => Some(Self::Overwrite),
            "merge" => Some(Self::Merge),
            _ => None,
        }
    }
}

/// Payload shape, which is what decides whether merging has a meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shape {
    /// A directory whose direct children are independent entries — extracting
    /// one chat session out of many, or injecting a session beside the ones
    /// that are already there.
    Entries,
    /// One file (or one directory) that is replaced or merged as a whole.
    Opaque,
}

impl Shape {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "entries" => Some(Self::Entries),
            "opaque" => Some(Self::Opaque),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Entries => "entries",
            Self::Opaque => "opaque",
        }
    }
}

/// One place a resource lives inside a container: a whole path, or one key path
/// inside a YAML document at that path.
///
/// A section exists because DSH splits some state across files: a provider route
/// is a section of `settings.yaml` (`llm-pi-ai.providers.<route>`) and its key
/// is a ref in `.credentials.yaml`, while the rest of `settings.yaml` belongs to
/// other plugins. Carrying the whole file would drag those along and clobber
/// them on the way back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KindPart {
    /// Container-relative path. `{profile}` is replaced with the container's
    /// profile, because DSH moved some state under `profiles/<name>/`.
    pub path: &'static str,
    /// YAML key path inside `path`; empty means the whole path travels. A step
    /// of the form `#<id>` selects the item whose `id` is `<id>` in a list of
    /// layers and travels that item's `config` — the shape Cordis uses for
    /// `cordis.patch.yml`, where a plugin's settings are one entry rather than
    /// a key of the document.
    pub section: &'static [&'static str],
    /// Names an alternative group: parts sharing a `slot` describe the same
    /// piece of state at different Harness versions, and exactly one travels.
    /// `None` means the part is unconditional.
    pub slot: Option<&'static str>,
    /// Lowest Harness version this part serves, inclusive. Compared as a
    /// numeric triple, so `0.1.7` also covers `0.1.7-alpha.1`.
    pub since: Option<&'static str>,
    /// Highest Harness version this part serves, exclusive.
    pub until: Option<&'static str>,
}

/// A built-in kind. `path` is container-relative; containers always keep their
/// profile at `<container>/profile`.
#[derive(Clone, Copy, Debug)]
pub struct Kind {
    pub id: &'static str,
    pub label: &'static str,
    pub secret: bool,
    pub shape: Shape,
    /// How deep the independent entries sit under the path. Chat history is two
    /// levels deep (`sessions/<workspace>/session-<id>`), so merging has to
    /// compare sessions, not whole workspaces — otherwise injecting one
    /// conversation would replace every conversation of that workspace.
    pub entry_depth: u8,
    /// Everything that has to move together. The first part is the primary one:
    /// it is the path the UI shows and the one a single-path caller means.
    pub parts: &'static [KindPart],
}

impl Kind {
    pub fn path(&self) -> &'static str {
        self.parts.first().map(|part| part.path).unwrap_or("")
    }
}

/// The kinds Box knows how to move without any plugin cooperation. Both were
/// chosen because they are the state a user actually wants to carry across
/// containers: their conversations and their model credentials.
pub const BUILTIN: &[Kind] = &[
    Kind {
        id: "sessions",
        label: "Chat history",
        secret: false,
        shape: Shape::Entries,
        entry_depth: 2,
        parts: &[KindPart {
            path: "profile/sessions",
            section: &[],
            slot: None,
            since: None,
            until: None,
        }],
    },
    Kind {
        id: "credentials",
        label: "AI provider credentials",
        secret: true,
        shape: Shape::Opaque,
        entry_depth: 1,
        // The key alone authenticates nothing: a provider route has to name the
        // environment variable that holds it. That half is `llm-pi-ai`, and
        // where it lives is a version fact — up to 0.1.6 it is a section of
        // `profile/settings.yaml`; from 0.1.7 the profile keeps its overrides in
        // a Cordis layer list and DSH archives the old file as
        // `settings.yaml.imported`. Both are declared, and the container's
        // Harness version picks one.
        parts: &[
            KindPart {
                path: "profile/.credentials.yaml",
                section: &[],
                slot: None,
                since: None,
                until: None,
            },
            KindPart {
                path: "profile/settings.yaml",
                section: &["llm-pi-ai"],
                slot: Some("provider-route"),
                since: None,
                until: Some("0.1.7"),
            },
            KindPart {
                path: "profile/profiles/{profile}/cordis.patch.yml",
                section: &["#llm-pi-ai"],
                slot: Some("provider-route"),
                since: Some("0.1.7"),
                until: None,
            },
        ],
    },
];

pub fn builtins() -> &'static [Kind] {
    BUILTIN
}

pub fn builtin(id: &str) -> Option<&'static Kind> {
    BUILTIN.iter().find(|kind| kind.id == id)
}

/// One part of a resolved kind: the same as [`KindPart`] with owned strings,
/// since a plugin declaration or an inferred path is not `'static`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedPart {
    pub path: String,
    #[serde(default)]
    pub section: Vec<String>,
    /// The slot this part was chosen from, when it was chosen from one. It is
    /// what lets an injection re-pick the destination's own version layout
    /// instead of writing where the copy's source kept it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
}

/// A kind resolved for one container: a built-in, a plugin declaration, or a
/// caller-supplied kind with an explicit destination.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedKind {
    pub id: String,
    pub label: String,
    /// The primary part's path, which is what the UI shows and what a
    /// single-path caller means. `parts` is what an extraction walks.
    pub path: String,
    pub secret: bool,
    pub shape: Shape,
    #[serde(default = "one")]
    pub entry_depth: u8,
    /// `true` when nothing declared this kind and Box only knows the path the
    /// caller gave it — the UI shows those as unverified.
    pub inferred: bool,
    /// Every part that has to move together; empty means "just `path`".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<ResolvedPart>,
}

fn one() -> u8 {
    1
}

/// `dsh-v0.1.7-alpha.1`, `0.1.7`, `v0.1.7` → `(0, 1, 7)`. A prerelease tag is
/// compared by its numeric triple only: the layout a slot exists for changes
/// with the release line, not with its alphas.
pub fn version_triple(raw: &str) -> Option<(u32, u32, u32)> {
    let trimmed = raw.trim().trim_start_matches("dsh-").trim_start_matches('v');
    let head = trimmed.split(['-', '+']).next().unwrap_or(trimmed);
    let mut numbers = head.split('.').map(|part| part.parse::<u32>().ok());
    match (numbers.next()?, numbers.next()?, numbers.next()?) {
        (Some(major), Some(minor), Some(patch)) => Some((major, minor, patch)),
        _ => None,
    }
}

/// A part with owned strings: what a plugin's `dshbox.resources` entry and a
/// built-in's [`KindPart`] both reduce to, so one selection rule serves both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartSpec {
    pub path: String,
    pub section: Vec<String>,
    pub slot: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
}

impl KindPart {
    pub fn to_spec(&self) -> PartSpec {
        PartSpec {
            path: (*self.path).to_owned(),
            section: self.section.iter().map(|key| (*key).to_owned()).collect(),
            slot: self.slot.map(str::to_owned),
            since: self.since.map(str::to_owned),
            until: self.until.map(str::to_owned),
        }
    }
}

/// A section step of the form `#<id>` addresses one item of a layer list.
pub fn is_layer_step(step: &str) -> bool {
    step.starts_with('#')
}

fn version_bound(raw: &Option<String>) -> Option<(u32, u32, u32)> {
    raw.as_deref().and_then(version_triple)
}

fn spec_applies(spec: &PartSpec, version: Option<(u32, u32, u32)>) -> bool {
    let since = version_bound(&spec.since);
    let until = version_bound(&spec.until);
    if since.is_some() && version.is_none() {
        // A tree whose version we cannot read gets the evergreen declaration.
        return false;
    }
    let version = version.unwrap_or((0, 0, 0));
    since.map_or(true, |bound| version >= bound) && until.map_or(true, |bound| version < bound)
}

/// The parts that travel for one container: the unconditional ones, plus for
/// each `slot` the single candidate matching this Harness version — highest
/// `since` wins, so a newer layout takes precedence over the one it replaced.
/// A slot with no matching candidate keeps its first declaration, which is what
/// a container predating the split still reads.
pub fn select_specs(specs: &[PartSpec], profile: &str, harness_version: Option<&str>) -> Vec<ResolvedPart> {
    let version = harness_version.and_then(version_triple);
    let resolve = |spec: &PartSpec| ResolvedPart {
        path: spec.path.replace("{profile}", profile),
        section: spec.section.clone(),
        slot: spec.slot.clone(),
    };
    let mut chosen: Vec<ResolvedPart> = Vec::new();
    let mut seen_slots: Vec<String> = Vec::new();
    for spec in specs {
        let Some(slot) = spec.slot.as_deref() else {
            chosen.push(resolve(spec));
            continue;
        };
        if seen_slots.iter().any(|seen| seen == slot) {
            continue;
        }
        seen_slots.push(slot.to_owned());
        let candidates: Vec<&PartSpec> = specs
            .iter()
            .filter(|other| other.slot.as_deref() == Some(slot))
            .collect();
        let best = candidates
            .iter()
            .copied()
            .filter(|candidate| spec_applies(candidate, version))
            .max_by_key(|candidate| version_bound(&candidate.since).unwrap_or((0, 0, 0)))
            .or_else(|| candidates.first().copied())
            .unwrap_or(spec);
        chosen.push(resolve(best));
    }
    chosen
}

impl ResolvedKind {
    /// Resolve a built-in against one container: which slot version applies, and
    /// what `{profile}` means here. `harness_version` is the container's own
    /// Harness version; `None` (an unreadable tree) keeps the unbounded parts
    /// and the first candidate of each slot, which is the older layout.
    pub fn from_builtin(kind: &Kind, profile: &str, harness_version: Option<&str>) -> Self {
        let specs: Vec<PartSpec> = kind.parts.iter().map(|part| part.to_spec()).collect();
        let parts = select_specs(&specs, profile, harness_version);
        Self {
            id: kind.id.to_owned(),
            label: kind.label.to_owned(),
            path: parts
                .first()
                .map(|part| part.path.clone())
                .unwrap_or_default(),
            secret: kind.secret,
            shape: kind.shape,
            entry_depth: kind.entry_depth,
            inferred: false,
            parts,
        }
    }

    /// What an extraction or an injection walks: the declared parts, or the one
    /// path this kind is.
    pub fn payload_parts(&self) -> Vec<ResolvedPart> {
        if self.parts.is_empty() {
            return vec![ResolvedPart {
                path: self.path.clone(),
                section: Vec::new(),
                slot: None,
            }];
        }
        self.parts.clone()
    }

    /// An explicit `<kind> <src> @<dest>` from a boxfile or `--dest`: the path
    /// is whatever the caller said, so the kind is only trusted as far as that
    /// destination is.
    pub fn explicit(id: &str, path: &str, secret: bool) -> Self {
        Self {
            id: id.to_owned(),
            label: id.to_owned(),
            path: path.to_owned(),
            secret,
            shape: Shape::Opaque,
            entry_depth: 1,
            inferred: true,
            parts: Vec::new(),
        }
    }
}
