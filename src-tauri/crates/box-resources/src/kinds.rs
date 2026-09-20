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

/// A built-in kind. `path` is container-relative; containers always keep their
/// profile at `<container>/profile`.
#[derive(Clone, Copy, Debug)]
pub struct Kind {
    pub id: &'static str,
    pub label: &'static str,
    pub path: &'static str,
    pub secret: bool,
    pub shape: Shape,
    /// How deep the independent entries sit under `path`. Chat history is two
    /// levels deep (`sessions/<workspace>/session-<id>`), so merging has to
    /// compare sessions, not whole workspaces — otherwise injecting one
    /// conversation would replace every conversation of that workspace.
    pub entry_depth: u8,
}

/// The kinds Box knows how to move without any plugin cooperation. Both were
/// chosen because they are the state a user actually wants to carry across
/// containers: their conversations and their model credentials.
pub const BUILTIN: &[Kind] = &[
    Kind {
        id: "sessions",
        label: "Chat history",
        path: "profile/sessions",
        secret: false,
        shape: Shape::Entries,
        entry_depth: 2,
    },
    Kind {
        id: "credentials",
        label: "AI provider credentials",
        path: "profile/.credentials.yaml",
        secret: true,
        shape: Shape::Opaque,
        entry_depth: 1,
    },
];

pub fn builtins() -> &'static [Kind] {
    BUILTIN
}

pub fn builtin(id: &str) -> Option<&'static Kind> {
    BUILTIN.iter().find(|kind| kind.id == id)
}

/// A kind resolved for one container: a built-in, a plugin declaration, or a
/// caller-supplied kind with an explicit destination.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedKind {
    pub id: String,
    pub label: String,
    pub path: String,
    pub secret: bool,
    pub shape: Shape,
    #[serde(default = "one")]
    pub entry_depth: u8,
    /// `true` when nothing declared this kind and Box only knows the path the
    /// caller gave it — the UI shows those as unverified.
    pub inferred: bool,
}

fn one() -> u8 {
    1
}

impl ResolvedKind {
    pub fn from_builtin(kind: &Kind) -> Self {
        Self {
            id: kind.id.to_owned(),
            label: kind.label.to_owned(),
            path: kind.path.to_owned(),
            secret: kind.secret,
            shape: kind.shape,
            entry_depth: kind.entry_depth,
            inferred: false,
        }
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
        }
    }
}
