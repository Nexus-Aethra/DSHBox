//! Text-level extraction of cordis service declarations from plugin sources.
//!
//! There is no TypeScript parser in this workspace, so declarations are read by
//! a hand-written scanner over comment-blanked text instead of an AST. The rules
//! below were derived from a real `deepseek-ai/deepseek-harness` checkout
//! (`master`, 2026-09) rather than from the cordis type surface alone: the forms
//! that actually occur, and the near-misses that must not be admitted, are
//! pinned in `tests.rs`.
//!
//! Two views of every file are produced. `code` blanks comments and string
//! *interiors*, so brace/paren matching and identifier search cannot be confused
//! by a `{` inside a string or an `inject` inside a comment. `literals` blanks
//! comments only, so service names can still be read at offsets taken from
//! `code`. Both are byte-for-byte the same length as the source, and therefore
//! as each other.
//!
//! Deliberately not recognised: computed service names (`inject: [name]`),
//! template-literal service names, and services contributed by a plugin loaded
//! at runtime through a computed specifier. Each of those lands in
//! [`Scan::unresolved`] instead of being guessed at, so a miss shows up in the
//! graph diagnostics rather than being silently absent from it.

use std::collections::BTreeSet;

/// One source file's declaration set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// Services the plugin waits for before it loads.
    pub requires: BTreeSet<String>,
    /// Services the plugin registers on the context.
    pub provides: BTreeSet<String>,
    /// Service names named by an `interface Context` augmentation. This is the
    /// type-level catalogue that decides whether a name *exists*; it says
    /// nothing about which plugin provides it.
    pub catalogue: BTreeSet<String>,
    /// Declaration sites that exist but could not be reduced to literals,
    /// rendered as `line <n>: <what>` for the graph diagnostics.
    pub unresolved: BTreeSet<String>,
}

impl Scan {
    fn merge(&mut self, other: Scan) {
        self.requires.extend(other.requires);
        self.provides.extend(other.provides);
        self.catalogue.extend(other.catalogue);
        self.unresolved.extend(other.unresolved);
    }
}

/// Comment-blanked views of one source file, both the same length as the input.
#[derive(Clone, Debug, Default)]
pub struct Masks {
    pub code: Vec<u8>,
    pub literals: Vec<u8>,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum State {
    Code,
    LineComment,
    BlockComment,
    Single,
    Double,
    Template,
}

/// Produce the two masked views described in the module docs.
pub fn mask(source: &str) -> Masks {
    let bytes = source.as_bytes();
    let mut masks = Masks {
        code: bytes.to_vec(),
        literals: bytes.to_vec(),
    };
    let mut state = State::Code;
    let mut i = 0usize;
    while i < bytes.len() {
        let byte = bytes[i];
        match state {
            State::Code => {
                if byte == b'/' && bytes.get(i + 1) == Some(&b'/') {
                    state = State::LineComment;
                    masks.code[i] = b' ';
                    masks.literals[i] = b' ';
                    i += 1;
                    masks.code[i] = b' ';
                    masks.literals[i] = b' ';
                } else if byte == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    state = State::BlockComment;
                    masks.code[i] = b' ';
                    masks.literals[i] = b' ';
                    i += 1;
                    masks.code[i] = b' ';
                    masks.literals[i] = b' ';
                } else if byte == b'\'' {
                    state = State::Single;
                } else if byte == b'"' {
                    state = State::Double;
                } else if byte == b'`' {
                    state = State::Template;
                }
            }
            State::LineComment => {
                if byte == b'\n' {
                    state = State::Code;
                } else {
                    masks.code[i] = b' ';
                    masks.literals[i] = b' ';
                }
            }
            State::BlockComment => {
                if byte == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    masks.code[i] = b' ';
                    masks.literals[i] = b' ';
                    i += 1;
                    masks.code[i] = b' ';
                    masks.literals[i] = b' ';
                    state = State::Code;
                } else if byte != b'\n' {
                    masks.code[i] = b' ';
                    masks.literals[i] = b' ';
                }
            }
            State::Single | State::Double | State::Template => {
                let terminator = match state {
                    State::Single => b'\'',
                    State::Double => b'"',
                    _ => b'`',
                };
                if byte == b'\\' {
                    // Keep the backslash so callers can still tell an escaped
                    // quote apart from a closing one; blank what it escapes.
                    if let Some(next) = bytes.get(i + 1) {
                        masks.code[i + 1] = if *next == b'\n' { *next } else { b' ' };
                        i += 1;
                    }
                } else if byte == terminator {
                    state = State::Code;
                } else if byte != b'\n' {
                    masks.code[i] = b' ';
                }
            }
        }
        i += 1;
    }
    masks
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

/// Whether the `<receiver>.inject(` at `dot` is called on a cordis context.
///
/// Cordis's scoped form takes a context: `ctx.inject(['tools'], cb)`,
/// `this.ctx.inject(...)`, `agentCtx.inject(...)`. DSH also has a **slot** API
/// with the same call shape — `ctx.slots.inject('sidebar', cb)` claims a UI
/// extension point and `ctx.slots.register({ name }, Component)` fills it — and a
/// slot name is not a service anything can `provide`. In the real client sources
/// the slot form outnumbers the scoped form roughly three to one, so reading both
/// as service requirements invented two thirds of the graph's "missing services"
/// (every `sidebar.*`, `main`, `rightbar`, `tool.call.*` row) for a container that
/// starts perfectly well. The receiver is what tells the two apart.
fn receiver_is_context(code: &[u8], dot: usize) -> bool {
    let mut start = dot;
    while start > 0 && is_ident(code[start - 1]) {
        start -= 1;
    }
    let name = &code[start..dot];
    name.ends_with(b"ctx") || name.ends_with(b"Ctx")
}

/// Find `word` in `hay` at or after `from`, requiring identifier boundaries so
/// `inject` does not match `injections` and `provide` does not match `provider`.
fn find_word(hay: &[u8], word: &str, from: usize) -> Option<usize> {
    let needle = word.as_bytes();
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let mut i = from;
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            let before_ok = i == 0 || !is_ident(hay[i - 1]);
            let after = i + needle.len();
            let after_ok = after >= hay.len() || !is_ident(hay[after]);
            if before_ok && after_ok {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn skip_ws(bytes: &[u8], mut at: usize) -> usize {
    while at < bytes.len() && bytes[at].is_ascii_whitespace() {
        at += 1;
    }
    at
}

/// Index just past the `close` matching the `open` at `at`.
fn skip_balanced(bytes: &[u8], at: usize, open: u8, close: u8) -> Option<usize> {
    if bytes.get(at) != Some(&open) {
        return None;
    }
    let mut depth = 0i32;
    let mut i = at;
    while i < bytes.len() {
        if bytes[i] == open {
            depth += 1;
        } else if bytes[i] == close {
            depth -= 1;
            if depth == 0 {
                return Some(i + 1);
            }
        }
        i += 1;
    }
    None
}

/// The dotted callee written immediately before the `(` at `open`, e.g.
/// `ctx.plugin` for `ctx.plugin({ … })`.
fn callee_before(code: &[u8], open: usize) -> &[u8] {
    let mut start = open;
    while start > 0 && code[start - 1].is_ascii_whitespace() {
        start -= 1;
    }
    let end = start;
    while start > 0 && (is_ident(code[start - 1]) || code[start - 1] == b'.') {
        start -= 1;
    }
    &code[start..end]
}

/// Whether the declaration at `at` is an argument of a `*.plugin(…)` call, which
/// is how cordis is handed a plugin that is not a module's own export.
///
/// Walking outwards and deciding at the first unclosed `(` is what keeps a
/// callback's *body* from counting: an `inject` inside `Object.assign(cb, { … })`
/// never reaches a `.plugin` callee, because the first unclosed `(` it meets is
/// `Object.assign`'s own.
fn argument_of_plugin_call(code: &[u8], at: usize) -> bool {
    let mut depth = 0i32;
    let mut i = at;
    while i > 0 {
        i -= 1;
        match code[i] {
            b'}' | b')' | b']' => depth += 1,
            // An object or array literal is not what is being called; keep going
            // outwards to whatever it is an argument of.
            b'{' | b'[' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            b'(' => {
                if depth == 0 {
                    return callee_before(code, i).ends_with(b".plugin");
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    false
}

/// Whether the `=` at `at` introduces a binding, as opposed to `==`, `=>` or a
/// compound assignment.
fn is_plain_assignment(code: &[u8], at: usize) -> bool {
    let before = code.get(at.wrapping_sub(1)).copied();
    let after = code.get(at + 1).copied();
    let compound = matches!(before, Some(b'=' | b'!' | b'<' | b'>' | b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|' | b'^' | b'?'));
    !compound && after != Some(b'=') && after != Some(b'>')
}

/// The `=` binding the value that contains `at`, when that value is a plain
/// expression or an `Object.assign(…)` of one.
///
/// `None` for anything else, and that is the point: a property inside some other
/// call — `ctx.slots.register({ inject: … }, Row)` — has no binding to speak of,
/// and reporting `None` keeps the previous statement's `=` from being read as
/// this value's.
fn assignment_before(code: &[u8], at: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = at;
    while i > 0 {
        i -= 1;
        match code[i] {
            b'}' | b')' | b']' => depth += 1,
            b'{' | b'[' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            b'(' => {
                if depth == 0 && callee_before(code, i) != b"Object.assign" {
                    return None;
                }
                if depth > 0 {
                    depth -= 1;
                }
            }
            b';' if depth == 0 => return None,
            b'=' if depth == 0 && is_plain_assignment(code, i) => return Some(i),
            _ => {}
        }
    }
    None
}

/// Whether the value that contains `at` is bound by an exported declaration,
/// e.g. `export const plugin = { … }`.
///
/// A module's export is what the loader is handed, so only an exported binding
/// registers what it holds. The `export` is looked for on the binding's own
/// source line: DSH writes one declaration per line, and a bounded window cannot
/// be fooled by the previous statement the way a walk to the file start would —
/// these sources carry no semicolons, so "the next `;` going backwards" is the
/// top of the file.
fn binding_is_exported(code: &[u8], at: usize) -> bool {
    let Some(equals) = assignment_before(code, at) else {
        return false;
    };
    let line_start = code[..equals]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    find_word(&code[line_start..equals], "export", 0).is_some()
}

/// The `{` opening the object literal that contains `at`.
fn enclosing_object_brace(code: &[u8], at: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = at;
    while i > 0 {
        i -= 1;
        match code[i] {
            b'}' | b')' | b']' => depth += 1,
            b'{' => {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
            b'[' | b'(' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether `code[..end]` ends with `word` at an identifier boundary.
fn ends_with_word(code: &[u8], end: usize, word: &[u8]) -> bool {
    if end < word.len() || &code[end - word.len()..end] != word {
        return false;
    }
    let before = end - word.len();
    before == 0 || !is_ident(code[before - 1])
}

/// Whether the object literal containing `at` is a function's return value.
///
/// The factory idiom builds a plugin and hands it back —
/// `function remoteProxiesPlugin(…): ClientPluginModule { return { inject:
/// ['connection'], apply(ctx) { … } } }` in `dsh-client-test-runtime` — and the
/// registration happens in whoever mounts the result. Nothing at this site says
/// so, so the `return` has to stand in for it; dropping the declaration would
/// hide a real wait, which is the one failure mode this scanner must not have.
fn is_returned_object(code: &[u8], at: usize) -> bool {
    let Some(brace) = enclosing_object_brace(code, at) else {
        return false;
    };
    let mut i = brace;
    while i > 0 && code[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if ends_with_word(code, i, b"return") {
        return true;
    }
    // An arrow's implicit return: `() => ({ … })`.
    if code.get(i.wrapping_sub(1)) == Some(&b'(') {
        let mut j = i - 1;
        while j > 0 && code[j - 1].is_ascii_whitespace() {
            j -= 1;
        }
        return code[..j].ends_with(b"=>");
    }
    false
}

/// Whether an object property such as `inject: […]` belongs to something cordis
/// actually registers.
///
/// The same property syntax carries declarations that are not plugins at all.
/// DSH writes every package's invariant companion as
/// `const install: InvariantInstaller = Object.assign((ctx, fail) => { … },
/// { inject: ['sessions'] })`, where that `inject` is documented as the services
/// a child installer fiber may access and is read by the invariants service, not
/// by cordis. Reading it as a package requirement invented one dependency per
/// companion — 26 of the 39 `invariant.ts` files in a real checkout, 17 of them
/// on `sessions` — and with them a load cycle that does not exist; the container
/// never had it. The property declares a dependency only when it sits in a value
/// cordis is given: an argument of `*.plugin(…)`, an exported binding, or a
/// value a factory returns for its caller to mount.
fn object_property_is_plugin_declaration(code: &[u8], at: usize) -> bool {
    argument_of_plugin_call(code, at) || binding_is_exported(code, at) || is_returned_object(code, at)
}

/// Read the string literal starting at `at` in the literals view.
fn read_literal(literals: &[u8], at: usize) -> Option<(String, usize)> {
    let quote = *literals.get(at)?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let mut value = Vec::new();
    let mut i = at + 1;
    while i < literals.len() {
        let byte = literals[i];
        if byte == b'\\' {
            if let Some(next) = literals.get(i + 1) {
                value.push(*next);
                i += 2;
                continue;
            }
        }
        if byte == quote {
            return Some((String::from_utf8_lossy(&value).into_owned(), i + 1));
        }
        value.push(byte);
        i += 1;
    }
    None
}

/// Top-level `=` that introduces a value, skipping `==`, `===` and `=>`.
fn find_assignment(code: &[u8], from: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = from;
    while i < code.len() {
        match code[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
            }
            b';' | b',' if depth == 0 => return None,
            b'=' if depth == 0 => {
                let next = code.get(i + 1).copied().unwrap_or(b' ');
                if next != b'=' && next != b'>' {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Result of reading a declaration value.
enum Value {
    /// Names read, plus whether every element was a literal. A partially
    /// resolved collection still contributes the names it did yield.
    Parsed(Vec<String>, bool),
    NotAValue,
}

/// A value position that starts a literal collection or string.
fn starts_value(byte: Option<&u8>) -> bool {
    matches!(byte, Some(b'[') | Some(b'{') | Some(b'\'') | Some(b'"'))
}

/// Read an array of service names, e.g. `['tools', 'shell']`.
fn read_array(code: &[u8], literals: &[u8], at: usize) -> Value {
    let Some(end) = skip_balanced(code, at, b'[', b']') else {
        return Value::NotAValue;
    };
    let mut names = Vec::new();
    let mut resolved = true;
    let mut i = at + 1;
    let mut depth = 0i32;
    while i + 1 < end {
        match code[i] {
            b'[' | b'{' | b'(' => depth += 1,
            b']' | b'}' | b')' => depth -= 1,
            b'\'' | b'"' if depth == 0 => {
                if let Some((name, next)) = read_literal(literals, i) {
                    names.push(name);
                    i = next;
                    continue;
                }
                resolved = false;
            }
            _ if depth == 0 && is_ident(code[i]) => resolved = false,
            _ => {}
        }
        i += 1;
    }
    Value::Parsed(names, resolved)
}

/// Read the keys of an object form, e.g. `{ shell: true, 'token-meter': {} }`.
fn read_object_keys(code: &[u8], literals: &[u8], at: usize) -> Value {
    let Some(end) = skip_balanced(code, at, b'{', b'}') else {
        return Value::NotAValue;
    };
    let mut names = Vec::new();
    let mut resolved = true;
    let mut i = at + 1;
    let mut depth = 0i32;
    while i + 1 < end {
        match code[i] {
            b'[' | b'{' | b'(' => depth += 1,
            b']' | b'}' | b')' => depth -= 1,
            b'\'' | b'"' if depth == 0 => {
                if let Some((name, next)) = read_literal(literals, i) {
                    names.push(name);
                    i = next;
                    continue;
                }
                resolved = false;
            }
            _ if depth == 0 && is_ident(code[i]) => {
                let mut j = i;
                while j < code.len() && is_ident(code[j]) {
                    j += 1;
                }
                // Only a `key:` pair names a service; a spread or shorthand
                // value must not become one.
                if code.get(skip_ws(code, j)) == Some(&b':') {
                    names.push(String::from_utf8_lossy(&code[i..j]).into_owned());
                } else {
                    resolved = false;
                }
                i = j;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    Value::Parsed(names, resolved)
}

/// Read a declaration value: an array, an object, or a single literal.
fn read_value(code: &[u8], literals: &[u8], at: usize) -> Value {
    match code.get(at) {
        Some(b'[') => read_array(code, literals, at),
        Some(b'{') => read_object_keys(code, literals, at),
        Some(b'\'') | Some(b'"') => match read_literal(literals, at) {
            Some((name, _)) => Value::Parsed(vec![name], true),
            None => Value::NotAValue,
        },
        _ => Value::NotAValue,
    }
}

/// Advance past the value that starts at `at`, returning the index after it.
fn skip_argument(code: &[u8], at: usize) -> Option<usize> {
    match code.get(at)? {
        b'[' => skip_balanced(code, at, b'[', b']'),
        b'{' => skip_balanced(code, at, b'{', b'}'),
        b'(' => skip_balanced(code, at, b'(', b')'),
        b'\'' | b'"' => read_literal(code, at).map(|(_, next)| next),
        _ => {
            let mut j = at;
            while j < code.len() && !matches!(code[j], b',' | b')' | b'\n') {
                j += 1;
            }
            Some(j)
        }
    }
}

fn line_of(source: &str, offset: usize) -> usize {
    source.as_bytes()[..offset.min(source.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

fn note(scan: &mut Scan, source: &str, offset: usize, what: &str) {
    scan.unresolved
        .insert(format!("line {}: {what}", line_of(source, offset)));
}

/// Record a read value into the requires or provides set, noting anything that
/// could not be fully reduced to literals.
fn record(scan: &mut Scan, source: &str, offset: usize, what: &str, value: Value, provides: bool) {
    let (names, resolved) = match value {
        Value::Parsed(names, resolved) => (names, resolved),
        Value::NotAValue => {
            note(scan, source, offset, &format!("{what} is not a literal"));
            return;
        }
    };
    let names: Vec<String> = names.into_iter().filter(|name| !name.is_empty()).collect();
    if provides {
        scan.provides.extend(names);
    } else {
        scan.requires.extend(names);
    }
    if !resolved {
        note(scan, source, offset, &format!("{what} contains a non-literal name"));
    }
}

/// `inject` in all of its declaration positions: `export const inject = [...]`,
/// `static inject = [...]` / `static override inject = [...]`,
/// `inject: [...]` / `inject: {...}` object properties of a plugin cordis is
/// given, and the scoped `ctx.inject([...], callback)` call. A type annotation
/// between the name and the value (`export const inject: string[] = []`) is
/// stepped over.
///
/// The two `=` forms are declarations wherever they appear: an identifier named
/// `inject` carrying a service list is a cordis plugin field, and `static
/// inject` is the service class's own. Only the object *property* form needs
/// asking whether anything registers it — see
/// [`object_property_is_plugin_declaration`].
fn scan_inject(code: &[u8], literals: &[u8], source: &str, scan: &mut Scan) {
    let mut from = 0usize;
    while let Some(pos) = find_word(code, "inject", from) {
        from = pos + "inject".len();
        let mut i = skip_ws(code, from);
        if code.get(i) == Some(&b'?') {
            i = skip_ws(code, i + 1);
        }
        // Scoped injection `ctx.inject([...], callback)` is always a property
        // call on a context. Requiring both the dot and a context-shaped receiver
        // is what keeps a method *declaration* named `inject` —
        // `inject(message: UserMessage): void`, which real DSH interface types do
        // declare — and the identically shaped slot API out of the dependency set.
        if code.get(i) == Some(&b'(') && pos > 0 && code[pos - 1] == b'.' && receiver_is_context(code, pos - 1)
        {
            let value = read_value(code, literals, skip_ws(code, i + 1));
            record(scan, source, pos, "inject", value, false);
            continue;
        }
        match code.get(i) {
            Some(b'=') => {
                let value = read_value(code, literals, skip_ws(code, i + 1));
                record(scan, source, pos, "inject", value, false);
            }
            Some(b':') => {
                let after = skip_ws(code, i + 1);
                if starts_value(code.get(after)) {
                    // The object-property form, and the only one that needs
                    // asking who registers it. A property nobody does declares
                    // nothing, and stays silent rather than being noted: the
                    // near-misses are routine (`ctx.slots.register({ inject: … })`
                    // fills a UI slot, and the invariant companions' installers
                    // are read by the invariants service).
                    if !object_property_is_plugin_declaration(code, pos) {
                        continue;
                    }
                    let value = read_value(code, literals, after);
                    record(scan, source, pos, "inject", value, false);
                } else if let Some(equals) = find_assignment(code, after) {
                    // `inject: <type> = <value>` — a *binding* named `inject`,
                    // which is the module-level plugin field with its type
                    // spelled out (`export const inject: string[] = []`), not an
                    // object property.
                    let value = read_value(code, literals, skip_ws(code, equals + 1));
                    record(scan, source, pos, "inject", value, false);
                }
                // Otherwise this is a type-only position (`inject?: Inject`),
                // which declares nothing and must not be reported as a miss.
            }
            _ => {}
        }
    }
}

/// The identifier a `super(...)` call passes as its first argument, when it is a
/// bare identifier.
fn first_argument_name(code: &[u8], after_open: usize) -> Option<&[u8]> {
    let at = skip_ws(code, after_open);
    if !code.get(at).is_some_and(|byte| is_ident(*byte)) {
        return None;
    }
    let mut end = at;
    while end < code.len() && is_ident(code[end]) {
        end += 1;
    }
    Some(&code[at..end])
}

/// `super(ctx, 'name')`: the cordis service registration.
///
/// Both arguments are constrained, because each one alone admits a near-miss
/// that really occurs in the harness:
///
/// * The first argument must be the context (`ctx`, or an underscored variant).
///   Error classes also call `super` with a trailing string literal —
///   `class X extends Error { super(message, 'CODE_RUN_FAILED') }` — so a rule
///   keyed on the literal alone turns every error code into a service.
/// * The second argument must be a quoted literal, because concrete service
///   implementations pass their config up instead
///   (`SandboxBashExecutor extends LocalBashExecutor` calls `super(ctx, config)`)
///   so a rule keyed on the first argument alone invents a provider there.
///
/// A plugin that names its context something else is not recognised. That
/// surfaces as a missing provider for its consumers, which is visible, rather
/// than as a wrong edge, which is not.
fn scan_super(code: &[u8], literals: &[u8], source: &str, scan: &mut Scan) {
    let mut from = 0usize;
    while let Some(pos) = find_word(code, "super", from) {
        from = pos + "super".len();
        let open = skip_ws(code, from);
        if code.get(open) != Some(&b'(') {
            continue;
        }
        match first_argument_name(code, open + 1) {
            Some(name) if name == b"ctx" || name == b"_ctx" => {}
            _ => continue,
        }
        let Some(first_end) = skip_argument(code, skip_ws(code, open + 1)) else {
            continue;
        };
        let comma = skip_ws(code, first_end);
        if code.get(comma) != Some(&b',') {
            continue;
        }
        let second = skip_ws(code, comma + 1);
        match read_value(code, literals, second) {
            Value::Parsed(names, true) if names.len() == 1 => scan.provides.extend(names),
            Value::Parsed(_, false) => note(scan, source, pos, "super service name is not a literal"),
            // A non-literal second argument is a config pass-through, not a
            // service name, so it stays quiet rather than being reported.
            _ => {}
        }
    }
}

/// `ctx.provide('name', value)` and `ctx.reflect.provide('name', value)`.
fn scan_provide_calls(code: &[u8], literals: &[u8], source: &str, scan: &mut Scan) {
    let mut from = 0usize;
    while let Some(pos) = find_word(code, "provide", from) {
        from = pos + "provide".len();
        if pos == 0 || code[pos - 1] != b'.' {
            continue;
        }
        let open = skip_ws(code, from);
        if code.get(open) != Some(&b'(') {
            continue;
        }
        let first = skip_ws(code, open + 1);
        match read_value(code, literals, first) {
            Value::Parsed(names, true) => scan.provides.extend(names),
            _ => note(scan, source, pos, "provide() service name is not a literal"),
        }
    }
}

/// The static `provide` field of a plugin descriptor, e.g.
/// `provide: 'shell'` or `provide: ['a', 'b']`. Unused by current DSH packages
/// (they register through `Service`), but it is the declared channel cordis
/// documents for third-party plugins.
fn scan_provide_fields(code: &[u8], literals: &[u8], source: &str, scan: &mut Scan) {
    let mut from = 0usize;
    while let Some(pos) = find_word(code, "provide", from) {
        from = pos + "provide".len();
        if pos > 0 && code[pos - 1] == b'.' {
            continue;
        }
        let mut i = skip_ws(code, from);
        if code.get(i) == Some(&b'?') {
            i = skip_ws(code, i + 1);
        }
        let value_at = match code.get(i) {
            Some(b':') | Some(b'=') => skip_ws(code, i + 1),
            _ => continue,
        };
        // A type position such as `provide?: string | string[]` reads as a bare
        // identifier. That is not a declaration, so it stays quiet.
        match read_value(code, literals, value_at) {
            Value::Parsed(names, true) => scan.provides.extend(names),
            Value::Parsed(names, false) => {
                scan.provides.extend(names);
                note(scan, source, pos, "provide contains a non-literal name");
            }
            Value::NotAValue => {}
        }
    }
}

/// Service names from `declare module '…' { interface Context { name: Type } }`.
/// Only the `Context` block is read: the same `declare module` usually also
/// carries an `interface Events` whose keys are not services.
fn scan_catalogue(code: &[u8], scan: &mut Scan) {
    let mut from = 0usize;
    while let Some(pos) = find_word(code, "interface", from) {
        from = pos + "interface".len();
        let mut i = skip_ws(code, from);
        let name_at = i;
        while i < code.len() && is_ident(code[i]) {
            i += 1;
        }
        if &code[name_at..i] != b"Context" {
            continue;
        }
        let brace = skip_ws(code, i);
        let Some(body) = skip_balanced(code, brace, b'{', b'}') else {
            continue;
        };
        let mut j = brace + 1;
        let mut depth = 0i32;
        while j + 1 < body {
            match code[j] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ if depth == 0 && is_ident(code[j]) => {
                    let start = j;
                    while j < code.len() && is_ident(code[j]) {
                        j += 1;
                    }
                    let after = skip_ws(code, j);
                    // Methods and generics are not service names.
                    if code.get(after) == Some(&b':') {
                        scan.catalogue
                            .insert(String::from_utf8_lossy(&code[start..j]).into_owned());
                    }
                    continue;
                }
                _ => {}
            }
            j += 1;
        }
    }
}

/// Scan one source file.
pub fn scan_source(source: &str) -> Scan {
    let masks = mask(source);
    let code = masks.code.as_slice();
    let literals = masks.literals.as_slice();
    let mut scan = Scan::default();
    scan_inject(code, literals, source, &mut scan);
    scan_super(code, literals, source, &mut scan);
    scan_provide_calls(code, literals, source, &mut scan);
    scan_provide_fields(code, literals, source, &mut scan);
    scan_catalogue(code, &mut scan);
    scan
}

/// Fold several files of one plugin package into a single declaration set.
pub fn scan_sources<'a>(sources: impl IntoIterator<Item = &'a str>) -> Scan {
    let mut total = Scan::default();
    for source in sources {
        total.merge(scan_source(source));
    }
    total
}
