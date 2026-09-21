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
    /// Container-relative path.
    pub path: &'static str,
    /// YAML key path inside `path`; empty means the whole path travels.
    pub section: &'static [&'static str],
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
        }],
    },
    Kind {
        id: "credentials",
        label: "AI provider credentials",
        secret: true,
        shape: Shape::Opaque,
        entry_depth: 1,
        // The key alone authenticates nothing: a provider route has to name the
        // environment variable that holds it. `llm-pi-ai` is that half.
        parts: &[
            KindPart {
                path: "profile/.credentials.yaml",
                section: &[],
            },
            KindPart {
                path: "profile/settings.yaml",
                section: &["llm-pi-ai"],
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

impl ResolvedKind {
    pub fn from_builtin(kind: &Kind) -> Self {
        let parts: Vec<ResolvedPart> = kind
            .parts
            .iter()
            .map(|part| ResolvedPart {
                path: part.path.to_owned(),
                section: part.section.iter().map(|key| (*key).to_owned()).collect(),
            })
            .collect();
        Self {
            id: kind.id.to_owned(),
            label: kind.label.to_owned(),
            path: kind.path().to_owned(),
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
