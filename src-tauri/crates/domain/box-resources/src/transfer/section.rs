//! YAML section operations: read one block by key path, write it back,
//! merge it or replace it.
//!
//! This is the same operation the UI's YAML editor performs, which is why
//! copy and edit are one mechanism -- a new YAML-shaped resource needs no
//! new code.

use super::*;
use super::tree::read_entries;

pub(super) fn extract_section(
    container_root: &Path,
    part: &PayloadPart,
    target: &Path,
) -> Result<(u64, u64), String> {
    let source = safe_join(container_root, &part.path)?;
    let label = part.section.join(".");
    let text = fs::read_to_string(&source)
        .map_err(|error| format!("cannot read {}: {error}", source.display()))?;
    let document: serde_yaml::Value = serde_yaml::from_str(&text)
        .map_err(|error| format!("{} is not valid YAML: {error}", source.display()))?;
    let value = section_value(&document, &part.section)
        .ok_or_else(|| format!("{} has no `{label}` section", source.display()))?;
    let body = serde_yaml::to_string(value).map_err(|error| error.to_string())?;
    fs::create_dir_all(target).map_err(|error| error.to_string())?;
    fs::write(target.join(SECTION_FILE), &body).map_err(|error| error.to_string())?;
    Ok((body.len() as u64, 1))
}

/// The file a section part travels as, inside its part directory.
pub(super) const SECTION_FILE: &str = "section.yaml";

/// The item of a layer list whose `id` is `id`. Cordis keeps a profile's
/// overrides as `- id: <plugin>\n  config: {…}`, so a plugin's settings are one
/// item rather than a key of the document.
fn layer_item<'a>(document: &'a serde_yaml::Value, id: &str) -> Option<&'a serde_yaml::Value> {
    document.as_sequence()?.iter().find(|item| {
        item.get("id").and_then(|value| value.as_str()) == Some(id)
    })
}

/// The value at a YAML key path, when the document has one. A `#<id>` step names
/// a layer item and continues inside its `config`.
pub(super) fn section_value<'a>(document: &'a serde_yaml::Value, section: &[String]) -> Option<&'a serde_yaml::Value> {
    let mut current = document;
    for key in section {
        current = match key.strip_prefix('#') {
            Some(id) => layer_item(current, id)?.get("config")?,
            None => current.get(key.as_str())?,
        };
    }
    Some(current)
}

/// Wrap a value back up in its section keys, so merging it into the destination
/// document is one deep merge. A `#<id>` step wraps in a one-item layer list
/// carrying `id` and `config`.
pub(super) fn nest(section: &[String], value: serde_yaml::Value) -> serde_yaml::Value {
    let mut current = value;
    for key in section.iter().rev() {
        current = match key.strip_prefix('#') {
            Some(id) => {
                let mut entry = serde_yaml::Mapping::new();
                entry.insert(
                    serde_yaml::Value::String("id".to_owned()),
                    serde_yaml::Value::String(id.to_owned()),
                );
                entry.insert(serde_yaml::Value::String("config".to_owned()), current);
                serde_yaml::Value::Sequence(vec![serde_yaml::Value::Mapping(entry)])
            }
            None => {
                let mut mapping = serde_yaml::Mapping::new();
                mapping.insert(serde_yaml::Value::String(key.clone()), current);
                serde_yaml::Value::Mapping(mapping)
            }
        };
    }
    current
}

/// Drop the value a section step names: a key of a mapping, or the `config` of
/// one layer item. A layer keeps its `id` and `name` when its settings are
/// replaced — those two are what Cordis uses to find the plugin, and dropping
/// them would leave an entry that resolves to nothing.
pub(super) fn remove_step(document: &mut serde_yaml::Value, step: &str) -> bool {
    match step.strip_prefix('#') {
        Some(id) => match document {
            serde_yaml::Value::Sequence(items) => {
                let Some(item) = items
                    .iter_mut()
                    .find(|entry| entry.get("id").and_then(|value| value.as_str()) == Some(id))
                else {
                    return false;
                };
                match item {
                    serde_yaml::Value::Mapping(mapping) => mapping
                        .remove(serde_yaml::Value::String("config".to_owned()))
                        .is_some(),
                    _ => false,
                }
            }
            _ => false,
        },
        None => match document {
            serde_yaml::Value::Mapping(mapping) => {
                mapping.remove(serde_yaml::Value::String(step.to_owned())).is_some()
            }
            _ => false,
        },
    }
}

/// Descend one step for a mutable walk, creating nothing.
pub(super) fn descend_mut<'a>(document: &'a mut serde_yaml::Value, step: &str) -> Option<&'a mut serde_yaml::Value> {
    match step.strip_prefix('#') {
        Some(id) => document
            .as_sequence_mut()?
            .iter_mut()
            .find(|item| item.get("id").and_then(|value| value.as_str()) == Some(id))?
            .get_mut("config"),
        None => document.get_mut(step),
    }
}

pub(super) fn remove_section(document: &mut serde_yaml::Value, section: &[String]) -> bool {
    let Some((last, parents)) = section.split_last() else {
        return false;
    };
    let mut current = document;
    for key in parents {
        match descend_mut(current, key) {
            Some(next) => current = next,
            None => return false,
        }
    }
    remove_step(current, last)
}

/// The independent entries of a tree at the kind's depth, as `(relative, path)`
/// pairs. A file at any level is an entry in its own right, so a stray file
/// beside the entries is carried instead of being silently dropped.
pub fn entries_at(root: &Path, depth: u8) -> Result<Vec<(String, PathBuf)>, String> {
    let mut entries = Vec::new();
    walk_entries(root, Path::new(""), depth.max(1), &mut entries)?;
    entries.sort();
    Ok(entries)
}

pub(super) fn walk_entries(
    dir: &Path,
    prefix: &Path,
    depth: u8,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    for entry in read_entries(dir)? {
        let Some(name) = entry.file_name() else {
            continue;
        };
        let relative = prefix.join(name);
        let relative_text = relative.to_string_lossy().replace('\\', "/");
        if depth <= 1 || !entry.is_dir() {
            out.push((relative_text, entry));
        } else {
            walk_entries(&entry, &relative, depth - 1, out)?;
        }
    }
    Ok(())
}

/// Merge one YAML section of a payload back into the document at `part.path`.
///
/// `Refuse` refuses when the destination already has that key path (the section
/// exists or not, nothing finer: it is one value), `Overwrite` drops the
/// destination's section first, and `Merge` deep-merges into it — which is what
/// adding a provider route to a `settings.yaml` that other plugins also write to
/// has to be.
pub(super) fn inject_section(
    payload_file: &Path,
    container_root: &Path,
    part: &PayloadPart,
    conflict: Conflict,
) -> Result<Injected, String> {
    let text = fs::read_to_string(payload_file)
        .map_err(|error| format!("cannot read {}: {error}", payload_file.display()))?;
    write_section(container_root, &part.path, &part.section, &text, conflict)
}

/// Write one YAML block into a container file, at the key path `section`
/// (empty = the whole file).
///
/// This is the same operation as injecting a section part, which is the point:
/// a provider route the user edits by hand and one that arrives inside a copy
/// are written by the same code, with the same conflict policy.
pub fn write_section(
    container_root: &Path,
    rel: &str,
    section: &[String],
    text: &str,
    conflict: Conflict,
) -> Result<Injected, String> {
    let dest = safe_join(container_root, rel)?;
    let label = if section.is_empty() {
        rel.to_owned()
    } else {
        section.join(".")
    };
    let incoming: serde_yaml::Value = serde_yaml::from_str(text)
        .map_err(|error| format!("the YAML to write is not valid: {error}"))?;

    let existing = fs::read_to_string(&dest).unwrap_or_default();
    let mut document: serde_yaml::Value = if existing.trim().is_empty() {
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
    } else {
        serde_yaml::from_str(&existing)
            .map_err(|error| format!("{} is not valid YAML: {error}", dest.display()))?
    };

    let present = if section.is_empty() {
        !existing.trim().is_empty()
    } else {
        section_value(&document, section).is_some()
    };
    if conflict == Conflict::Refuse && present {
        return Err(format!(
            "{} already has `{label}`; pass --overwrite to replace that section or --merge to add \
             to it",
            dest.display()
        ));
    }
    let merged = if section.is_empty() {
        // The whole file: an overlay of the file itself is just the file.
        if conflict == Conflict::Overwrite || !present {
            incoming
        } else {
            merge_value(document, incoming)
        }
    } else {
        if conflict == Conflict::Overwrite {
            remove_section(&mut document, section);
        }
        merge_value(document, nest(section, incoming))
    };
    let body = serde_yaml::to_string(&merged).map_err(|error| error.to_string())?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = dest.with_extension("yaml.tmp");
    fs::write(&temporary, &body).map_err(|error| error.to_string())?;
    fs::rename(&temporary, &dest).map_err(|error| error.to_string())?;

    let entry = label.clone();
    let mut replaced = Vec::new();
    let mut added = Vec::new();
    if present {
        replaced.push(entry);
    } else {
        added.push(entry);
    }
    Ok(Injected {
        bytes: body.len() as u64,
        files: 1,
        replaced,
        added,
    })
}

/// Merge two YAML documents: mappings merge key by key, the incoming value wins
/// on a scalar or sequence conflict.
pub(super) fn merge_yaml(target: &str, incoming: &str) -> Result<String, String> {
    let base: serde_yaml::Value =
        serde_yaml::from_str(target).map_err(|error| format!("invalid YAML at destination: {error}"))?;
    let overlay: serde_yaml::Value =
        serde_yaml::from_str(incoming).map_err(|error| format!("invalid YAML payload: {error}"))?;
    let merged = merge_value(base, overlay);
    serde_yaml::to_string(&merged).map_err(|error| error.to_string())
}

pub(super) fn merge_value(base: serde_yaml::Value, overlay: serde_yaml::Value) -> serde_yaml::Value {
    use serde_yaml::Value;
    match (base, overlay) {
        (Value::Mapping(mut base), Value::Mapping(overlay)) => {
            for (key, value) in overlay {
                let merged = match base.remove(&key) {
                    Some(existing) => merge_value(existing, value),
                    None => value,
                };
                base.insert(key, merged);
            }
            Value::Mapping(base)
        }
        (Value::Sequence(mut base), Value::Sequence(overlay)) => {
            // A layer list merges by `id`: carrying one provider route into a
            // container must leave the other plugins' layers it already had.
            for item in overlay {
                match item.get("id").and_then(|value| value.as_str()).map(str::to_owned) {
                    Some(id) => {
                        let existing = base.iter_mut().find(|entry| {
                            entry.get("id").and_then(|value| value.as_str()) == Some(id.as_str())
                        });
                        match existing {
                            Some(slot) => *slot = merge_value(slot.clone(), item),
                            None => base.push(item),
                        }
                    }
                    None => base.push(item),
                }
            }
            Value::Sequence(base)
        }
        (_, overlay) => overlay,
    }
}
