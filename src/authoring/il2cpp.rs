//! **IL2CPP → scry profile**: an offline converter.
//!
//! A Unity IL2CPP game ships its own reflection — `global-metadata.dat` plus the
//! native `GameAssembly` — and tools like *Il2CppDumper* parse those files
//! (100% read-only, no injection) into a human-readable `dump.cs` that annotates
//! every field with its byte offset:
//!
//! ```text
//! // Namespace: Combat
//! public class PartyMember : MonoBehaviour
//! {
//!     public int currentHp; // 0x18
//!     public int maxHp;     // 0x1C
//! }
//! ```
//!
//! Those offsets are exactly what a scry [`Watch`] needs — and exactly what a
//! game *patch* churns. This module turns
//!
//! - an Il2CppDumper `dump.cs` ([`Symbols::parse`]), plus
//! - a small, author-written [name map](ConvertSpec) that pins each watch to a
//!   dotted `Class::field` path,
//!
//! into a ready-to-use [`Profile`] with the offsets filled in ([`convert`]).
//! What the author maintains by hand is a *name* (`PartyMember::currentHp`); the
//! brittle number is *derived*. On a patch you re-run Il2CppDumper and this
//! converter instead of re-counting offsets by hand.
//!
//! # What is and isn't automated
//!
//! The value this tool adds is turning **named field references into numeric
//! offsets** — the error-prone, patch-fragile part. It does *not* reverse the
//! whole pointer chain for you: the **root anchor** of each chain — a static
//! slot for a Tier-1 watch, or an AOB signature (optionally with a RIP-relative
//! [`rip`](crate::profile::Rip) decode) for a Tier-2 watch — is still the
//! author's to supply (found once with `scry scan` / a debugger). A chain entry
//! is therefore either a `Class::field` reference (looked up in the dump) or a
//! literal offset (passed straight through). See the [`ChainEntry`] docs.
//!
//! # Example
//!
//! ```
//! use scry::authoring::il2cpp;
//!
//! let dump = "\
//! // Namespace: Combat
//! public class PartyMember : MonoBehaviour
//! {
//!     public int currentHp; // 0x18
//! }
//! ";
//! let map = r#"{
//!   "label": "Example (IL2CPP)",
//!   "process": "Game.exe",
//!   "module": "GameAssembly.dll",
//!   "probe": { "string": "PartyMember" },
//!   "watches": [
//!     { "name": "hp", "tier": "tier1",
//!       "chain": ["0x2C4E120", "PartyMember::currentHp"], "type": "i32" }
//!   ]
//! }"#;
//!
//! let profile = il2cpp::convert_files(dump, map).expect("convert");
//! assert_eq!(profile.watches.len(), 1);
//! ```

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

use crate::aob;
use crate::profile::{Match, Profile, Rip, ValueType, Watch};

// ---- the dump.cs symbol table ---------------------------------------------

/// A field's location within its declaring type, as read from a dump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// Byte offset of the field: from the instance base for an instance field,
    /// or within the type's static storage for a `static` field.
    pub offset: i64,
    /// Whether the field is declared `static`. Informational — a static field's
    /// offset is relative to the class's static storage, not an instance, so the
    /// chain that reaches it is a different (author-supplied) shape.
    pub is_static: bool,
}

/// Why a `Class::field` reference could not be resolved against a [`Symbols`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupError {
    /// No field by that name exists in the dump.
    Unknown,
    /// A bare `Class::field` whose simple class name is defined in more than one
    /// namespace (and both define the field). Qualify it to disambiguate; the
    /// candidates are the fully-qualified keys that matched.
    Ambiguous(Vec<String>),
}

/// The `Class::field → offset` table parsed out of an Il2CppDumper `dump.cs`.
///
/// Lookups accept either a fully-qualified `Namespace.Class::field` or a bare
/// `Class::field`; the bare form resolves as long as it is unambiguous.
pub struct Symbols {
    /// Keyed by fully-qualified `Namespace.Class::field` (namespace omitted for
    /// the global namespace; nested types joined with `.`).
    by_full: BTreeMap<String, Field>,
    /// `SimpleClass::field` → every fully-qualified key that shortens to it, so
    /// an author can drop the namespace when it is unambiguous.
    by_simple: BTreeMap<String, Vec<String>>,
}

impl Symbols {
    /// Parse an Il2CppDumper `dump.cs` into a field-offset table.
    ///
    /// Deliberately tolerant: it reads what it recognises (namespaced and nested
    /// type declarations, and their offset-annotated field lines) and silently
    /// ignores everything else — methods, properties, attributes, `const`s, enum
    /// members. A shape it doesn't understand yields no symbol rather than an
    /// error, so a converter run fails only on a reference the map actually asks
    /// for and the dump doesn't provide.
    pub fn parse(dump: &str) -> Self {
        let mut by_full: BTreeMap<String, Field> = BTreeMap::new();
        let mut by_simple: BTreeMap<String, Vec<String>> = BTreeMap::new();

        let mut namespace = String::new();
        // Stack of (brace-depth of the type's body, its fully-qualified name).
        let mut types: Vec<(usize, String)> = Vec::new();
        let mut depth: usize = 0;
        // A type declaration seen whose opening `{` hasn't arrived yet.
        let mut pending_type: Option<String> = None;

        for raw in dump.lines() {
            let line = raw.trim();

            // Namespace marker: `// Namespace: Foo.Bar` (empty for the global one).
            if let Some(ns) = line.strip_prefix("// Namespace:") {
                namespace = ns.trim().to_string();
                continue;
            }
            // Any other comment line — file header, `// Fields`, `// Methods` —
            // is not code. Skip before it can be mistaken for a declaration.
            if line.starts_with("//") {
                continue;
            }

            if let Some(simple) = type_decl_name(line) {
                let parent = types.last().map(|(_, n)| n.as_str());
                pending_type = Some(qualify_type(&namespace, parent, simple));
                // Fall through: the decl line may also carry its opening `{`.
            } else if let Some((name, field)) = field_decl(line) {
                if let Some((_, ty_full)) = types.last() {
                    let full_key = format!("{ty_full}::{name}");
                    let simple_key = format!("{}::{name}", simple_type(ty_full));
                    by_simple
                        .entry(simple_key)
                        .or_default()
                        .push(full_key.clone());
                    by_full.insert(full_key, field);
                }
            }

            // Track braces so we always know which type body we're inside: a
            // pending decl's `{` opens it, and a matching `}` closes the
            // innermost open type.
            for b in line.bytes() {
                match b {
                    b'{' => {
                        depth += 1;
                        if let Some(name) = pending_type.take() {
                            types.push((depth, name));
                        }
                    }
                    b'}' => {
                        if types.last().map(|(d, _)| *d) == Some(depth) {
                            types.pop();
                        }
                        depth = depth.saturating_sub(1);
                    }
                    _ => {}
                }
            }
        }

        Symbols { by_full, by_simple }
    }

    /// Resolve a `Class::field` reference to its [`Field`]. Accepts a
    /// fully-qualified `Namespace.Class::field` or a bare `Class::field`.
    pub fn lookup(&self, reference: &str) -> Result<Field, LookupError> {
        // A fully-qualified reference hits the full table directly.
        if let Some(field) = self.by_full.get(reference) {
            return Ok(*field);
        }
        // Otherwise treat it as a bare `Class::field` via the simple index.
        match self.by_simple.get(reference) {
            None => Err(LookupError::Unknown),
            Some(fulls) if fulls.len() == 1 => Ok(self.by_full[&fulls[0]]),
            Some(fulls) => Err(LookupError::Ambiguous(fulls.clone())),
        }
    }

    /// Number of distinct fields parsed. Handy for a "read N symbols" summary.
    pub fn len(&self) -> usize {
        self.by_full.len()
    }

    /// Whether the dump yielded no fields at all (e.g. an empty or unrecognised
    /// file) — usually a sign the wrong artefact was passed.
    pub fn is_empty(&self) -> bool {
        self.by_full.is_empty()
    }
}

/// The simple class name of a fully-qualified type: everything after the last `.`.
fn simple_type(full: &str) -> &str {
    full.rsplit('.').next().unwrap_or(full)
}

/// Build a type's fully-qualified name from the current namespace, its enclosing
/// type (if nested), and its own simple name.
fn qualify_type(namespace: &str, parent: Option<&str>, simple: &str) -> String {
    match parent {
        Some(p) => format!("{p}.{simple}"),
        None if namespace.is_empty() => simple.to_string(),
        None => format!("{namespace}.{simple}"),
    }
}

/// If `line` declares a type (`class`/`struct`/`interface`/`enum`), return its
/// cleaned simple name; otherwise `None`.
fn type_decl_name(line: &str) -> Option<&str> {
    let mut want_name = false;
    for tok in line.split_whitespace() {
        if want_name {
            return Some(clean_type_name(tok));
        }
        if matches!(tok, "class" | "struct" | "interface" | "enum") {
            want_name = true;
        }
    }
    None
}

/// Trim a declared type name down to its identifier: drop any generic arity
/// (`<T>` or the `` ` `` form) and any trailing base-list / brace punctuation.
fn clean_type_name(tok: &str) -> &str {
    let end = tok.find(['<', '`', ':', ',', '{']).unwrap_or(tok.len());
    &tok[..end]
}

/// If `line` is an offset-annotated field declaration, return `(name, field)`.
///
/// A field is a `;`-terminated declaration whose trailing comment is exactly a
/// `0x…` offset — which cleanly excludes methods (comment starts `RVA:`),
/// properties (`{ get; }`), `const`s (an `=` initialiser, no offset) and enum
/// members (comma-terminated).
fn field_decl(line: &str) -> Option<(String, Field)> {
    let (code, comment) = line.split_once("//")?;

    // The comment must be *only* an offset — `0x` followed by hex.
    let hex = comment.trim().strip_prefix("0x")?;
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let offset = i64::from_str_radix(hex, 16).ok()?;

    // The code must be a plain field: a `;`-terminated declaration, no call
    // parens, no property/method braces, no initialiser.
    let decl = code.trim().strip_suffix(';')?;
    if decl.contains('(') || decl.contains('{') || decl.contains('=') {
        return None;
    }
    let name = decl.split_whitespace().last()?;
    let is_static = decl.split_whitespace().any(|t| t == "static");
    Some((name.to_string(), Field { offset, is_static }))
}

// ---- the author name-map ---------------------------------------------------

/// The author-written conversion spec: identity plus the watches to emit, each
/// pinned to `Class::field` names rather than raw offsets.
///
/// This is the JSON an author maintains. It mirrors a [`Profile`] but swaps the
/// fragile numbers for names the converter resolves against a dump.
#[derive(Debug, Deserialize)]
pub struct ConvertSpec {
    /// Optional human-readable label, copied verbatim into the profile.
    #[serde(default)]
    pub label: Option<String>,
    /// Executable name for the profile's `match.process`.
    pub process: String,
    /// Module that anchors the values — for IL2CPP, typically
    /// `"GameAssembly.dll"`. Used as `match.module` and as the default module
    /// for Tier-1 watches that don't name their own.
    pub module: String,
    /// Optional build/version discriminant for `match.version`.
    #[serde(default)]
    pub version: Option<String>,
    /// How to derive the profile's identity `probe`.
    pub probe: ProbeSpec,
    /// The watches to emit, in order.
    pub watches: Vec<WatchSpec>,
}

impl ConvertSpec {
    /// Parse a conversion spec from its JSON document.
    pub fn from_json(s: &str) -> Result<Self, ConvertError> {
        serde_json::from_str(s).map_err(|e| ConvertError::BadMap(e.to_string()))
    }
}

/// How to produce the profile's identity `probe` (an AOB signature).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ProbeSpec {
    /// A raw AOB signature string, passed through verbatim (and validated). Use
    /// this when you already found a code signature with `scry scan`.
    Raw(String),
    /// A distinctive metadata string, encoded to a byte signature. A game's
    /// `global-metadata.dat` is loaded into the process, so a sufficiently unique
    /// name in it is a stable, scannable identity marker — no disassembly
    /// required. It must be a **single contiguous token** (one class or member
    /// identifier, or a user string literal): IL2CPP stores a namespace and type
    /// name separately, so a dotted `Namespace.Type` never appears as those bytes
    /// in a row. The CLI warns when a probe string looks dotted.
    Derived {
        /// The string to encode.
        string: String,
        /// Byte encoding to scan for; defaults to UTF-8.
        #[serde(default)]
        encoding: StringEncoding,
    },
}

/// Byte encoding for a string-derived probe.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StringEncoding {
    /// UTF-8 (the default; how IL2CPP metadata stores type and member names).
    #[default]
    Utf8,
    /// UTF-16 little-endian (for values the engine happens to hold as wide
    /// strings).
    Utf16le,
}

/// One watch, pinned by name. Mirrors [`Watch`] but its `chain` carries
/// `Class::field` references in place of raw offsets.
#[derive(Debug, Deserialize)]
#[serde(tag = "tier", rename_all = "lowercase")]
pub enum WatchSpec {
    /// Tier-1: anchored at a module's load base. `chain` starts from that base.
    Tier1 {
        /// Label for the value.
        name: String,
        /// Module whose base anchors the chain; defaults to the spec's `module`.
        #[serde(default)]
        module: Option<String>,
        /// The pointer chain, as a mix of `Class::field` references and literal
        /// offsets. See [`ChainEntry`].
        chain: Vec<ChainEntry>,
        /// How to interpret the bytes at the resolved address.
        #[serde(rename = "type")]
        ty: ValueType,
        /// Optional per-watch sample rate in hertz.
        #[serde(default)]
        rate_hz: Option<f64>,
    },
    /// Tier-2: anchored at the first hit of an AOB signature.
    Tier2 {
        /// Label for the value.
        name: String,
        /// AOB signature whose match address anchors the chain.
        anchor: String,
        /// Optional RIP-relative decode applied to the anchor before the chain
        /// is walked — the x64 static-base shape (a `mov reg, [rip+disp32]`
        /// accessor). Copied verbatim into the emitted [`Watch::Tier2`]; the
        /// converter doesn't derive it (the accessor is found by hand, see
        /// `docs/authoring-il2cpp.md`), it just carries it through. Absent means
        /// the AOB hit *is* the chain start. See [`Rip`].
        #[serde(default)]
        rip: Option<Rip>,
        /// The pointer chain from the anchor; see [`ChainEntry`].
        chain: Vec<ChainEntry>,
        /// How to interpret the bytes at the resolved address.
        #[serde(rename = "type")]
        ty: ValueType,
        /// Optional per-watch sample rate in hertz.
        #[serde(default)]
        rate_hz: Option<f64>,
    },
}

/// One step of a pointer chain in the author map.
///
/// Either a **field reference** to resolve against the dump, or a **literal
/// offset** to pass straight through:
///
/// - a JSON string containing `::` — a `Class::field` reference (bare or
///   `Namespace.Class::field`), replaced by that field's offset;
/// - a JSON number — a literal byte offset;
/// - a JSON string with no `::` — a literal offset in text form (`"0x18"`,
///   `"-4"`), for the parts of a chain a dump can't name (a static-storage base,
///   a hand-found constant).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ChainEntry {
    /// A literal byte offset.
    Num(i64),
    /// A `Class::field` reference, or a numeric literal in string form.
    Sym(String),
}

// ---- conversion ------------------------------------------------------------

/// Anything that can go wrong turning a [`ConvertSpec`] into a [`Profile`].
#[derive(Debug)]
pub enum ConvertError {
    /// The author map JSON could not be parsed.
    BadMap(String),
    /// A chain entry that is neither a `Class::field` reference nor a numeric
    /// offset literal.
    BadChainEntry {
        /// Watch whose chain contained it.
        watch: String,
        /// The offending entry.
        entry: String,
    },
    /// A `Class::field` reference the dump does not contain.
    UnknownField {
        /// Watch that referenced it.
        watch: String,
        /// The unresolved reference.
        field: String,
    },
    /// A bare `Class::field` reference that is ambiguous across namespaces.
    AmbiguousField {
        /// Watch that referenced it.
        watch: String,
        /// The ambiguous reference.
        field: String,
        /// The fully-qualified keys it could mean; qualify the map with one.
        candidates: Vec<String>,
    },
    /// A Tier-2 watch's `anchor` is not a valid AOB signature.
    BadAnchor {
        /// Watch that carried it.
        watch: String,
        /// Why it failed to parse.
        reason: String,
    },
    /// The derived (or raw) probe is not a valid AOB signature.
    BadProbe(String),
}

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConvertError::BadMap(e) => write!(f, "bad author map: {e}"),
            ConvertError::BadChainEntry { watch, entry } => write!(
                f,
                "watch {watch:?}: chain entry {entry:?} is neither a `Class::field` \
                 reference nor a numeric offset"
            ),
            ConvertError::UnknownField { watch, field } => {
                write!(f, "watch {watch:?}: field {field:?} not found in the dump")
            }
            ConvertError::AmbiguousField {
                watch,
                field,
                candidates,
            } => write!(
                f,
                "watch {watch:?}: field {field:?} is ambiguous — qualify it as one of: {}",
                candidates.join(", ")
            ),
            ConvertError::BadAnchor { watch, reason } => {
                write!(f, "watch {watch:?}: bad anchor signature: {reason}")
            }
            ConvertError::BadProbe(reason) => write!(f, "bad probe signature: {reason}"),
        }
    }
}

impl std::error::Error for ConvertError {}

/// Convert a parsed [`ConvertSpec`] against a [`Symbols`] table into a
/// [`Profile`] with every offset resolved.
///
/// Fails on the first reference the dump can't satisfy (unknown or ambiguous),
/// on a malformed chain entry, or on a signature (probe/anchor) that isn't valid
/// AOB — the honest, early failure an author wants at authoring time rather than
/// a silently wrong profile discovered against a live game.
pub fn convert(spec: &ConvertSpec, symbols: &Symbols) -> Result<Profile, ConvertError> {
    let probe = resolve_probe(&spec.probe);
    // A profile whose probe can't parse could never claim a process — reject now.
    aob::parse_pattern(&probe).map_err(|e| ConvertError::BadProbe(e.to_string()))?;

    let mut watches = Vec::with_capacity(spec.watches.len());
    for w in &spec.watches {
        watches.push(build_watch(w, symbols, &spec.module)?);
    }

    Ok(Profile {
        label: spec.label.clone(),
        match_: Match {
            process: spec.process.clone(),
            module: spec.module.clone(),
            version: spec.version.clone(),
            probe,
        },
        watches,
    })
}

/// Parse a `dump.cs` and an author-map JSON in one call.
pub fn convert_files(dump: &str, map_json: &str) -> Result<Profile, ConvertError> {
    let symbols = Symbols::parse(dump);
    let spec = ConvertSpec::from_json(map_json)?;
    convert(&spec, &symbols)
}

fn build_watch(
    spec: &WatchSpec,
    symbols: &Symbols,
    default_module: &str,
) -> Result<Watch, ConvertError> {
    match spec {
        WatchSpec::Tier1 {
            name,
            module,
            chain,
            ty,
            rate_hz,
        } => Ok(Watch::Tier1 {
            name: name.clone(),
            module: module.clone().unwrap_or_else(|| default_module.to_string()),
            offsets: resolve_chain(name, chain, symbols)?,
            ty: *ty,
            rate_hz: *rate_hz,
        }),
        WatchSpec::Tier2 {
            name,
            anchor,
            rip,
            chain,
            ty,
            rate_hz,
        } => {
            aob::parse_pattern(anchor).map_err(|e| ConvertError::BadAnchor {
                watch: name.clone(),
                reason: e.to_string(),
            })?;
            Ok(Watch::Tier2 {
                name: name.clone(),
                anchor: anchor.clone(),
                rip: *rip,
                offsets: resolve_chain(name, chain, symbols)?,
                ty: *ty,
                rate_hz: *rate_hz,
            })
        }
    }
}

fn resolve_chain(
    watch: &str,
    chain: &[ChainEntry],
    symbols: &Symbols,
) -> Result<Vec<i64>, ConvertError> {
    chain
        .iter()
        .map(|entry| resolve_entry(watch, entry, symbols))
        .collect()
}

fn resolve_entry(watch: &str, entry: &ChainEntry, symbols: &Symbols) -> Result<i64, ConvertError> {
    match entry {
        ChainEntry::Num(n) => Ok(*n),
        // A `::` marks a field reference; anything else is a numeric literal.
        ChainEntry::Sym(s) if s.contains("::") => {
            symbols.lookup(s).map(|f| f.offset).map_err(|e| match e {
                LookupError::Unknown => ConvertError::UnknownField {
                    watch: watch.to_string(),
                    field: s.clone(),
                },
                LookupError::Ambiguous(candidates) => ConvertError::AmbiguousField {
                    watch: watch.to_string(),
                    field: s.clone(),
                    candidates,
                },
            })
        }
        ChainEntry::Sym(s) => parse_int(s).ok_or_else(|| ConvertError::BadChainEntry {
            watch: watch.to_string(),
            entry: s.clone(),
        }),
    }
}

/// Parse a signed integer literal, decimal or `0x`-prefixed hex.
fn parse_int(s: &str) -> Option<i64> {
    let s = s.trim();
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r.trim_start()),
        None => (false, s),
    };
    let magnitude = match rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        Some(hex) => i64::from_str_radix(hex, 16).ok()?,
        None => rest.parse::<i64>().ok()?,
    };
    Some(if neg { -magnitude } else { magnitude })
}

fn resolve_probe(spec: &ProbeSpec) -> String {
    match spec {
        ProbeSpec::Raw(sig) => sig.clone(),
        ProbeSpec::Derived { string, encoding } => encode_signature(string, *encoding),
    }
}

/// Encode a string to a space-separated hex AOB signature.
fn encode_signature(s: &str, enc: StringEncoding) -> String {
    let bytes: Vec<u8> = match enc {
        StringEncoding::Utf8 => s.bytes().collect(),
        StringEncoding::Utf16le => s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect(),
    };
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    // A compact but representative dump: the global namespace, a namespaced
    // type, a nested enum, a compiler-generated backing field, a static field,
    // and a same-simple-name class in a second namespace (with one shared field
    // name, to force an ambiguity).
    const DUMP: &str = "\
// Image 0: mscorlib.dll - 0

// Namespace:
public class GameManager : MonoBehaviour
{
	// Fields
	private static GameManager _instance; // 0x0
	public int gold; // 0x20
	public PartyMember leader; // 0x28

	// Properties
	public static GameManager instance { get; }

	// Methods
	public void Awake() { } // RVA: 0x1000 Offset: 0x1000 VA: 0x181000
}

// Namespace: Combat
public class PartyMember : MonoBehaviour
{
	// Fields
	public int currentHp; // 0x18
	public int maxHp; // 0x1C
	[CompilerGenerated]
	private bool <isAlive>k__BackingField; // 0x24
	public GameManager manager; // 0x30
	public const int MAX_LEVEL = 99;

	// Nested type
	public enum Status
	{
		Dead = 0,
		Alive = 1
	}
}

// Namespace: Ui
public class PartyMember
{
	// Fields
	public int slot; // 0x40
	public int currentHp; // 0x44
}
";

    #[test]
    fn parses_instance_static_and_backing_fields() {
        let s = Symbols::parse(DUMP);

        assert_eq!(
            s.lookup("GameManager::gold").unwrap(),
            Field {
                offset: 0x20,
                is_static: false
            }
        );
        assert_eq!(
            s.lookup("GameManager::_instance").unwrap(),
            Field {
                offset: 0x0,
                is_static: true
            }
        );
        // Compiler-generated backing fields keep their `<...>` names.
        assert_eq!(
            s.lookup("Combat.PartyMember::<isAlive>k__BackingField")
                .unwrap()
                .offset,
            0x24
        );
    }

    #[test]
    fn const_members_and_methods_are_not_fields() {
        let s = Symbols::parse(DUMP);
        // A `const` (initialiser, no offset) and a method (RVA comment) must not
        // be mistaken for readable fields.
        assert_eq!(
            s.lookup("PartyMember::MAX_LEVEL"),
            Err(LookupError::Unknown)
        );
        assert_eq!(s.lookup("GameManager::Awake"), Err(LookupError::Unknown));
        // The nested enum's members are not fields either.
        assert_eq!(s.lookup("Status::Alive"), Err(LookupError::Unknown));
    }

    #[test]
    fn bare_reference_resolves_when_unambiguous() {
        let s = Symbols::parse(DUMP);
        // `maxHp` lives only on Combat.PartyMember, so the bare form is fine.
        assert_eq!(s.lookup("PartyMember::maxHp").unwrap().offset, 0x1C);
        // `slot` lives only on Ui.PartyMember.
        assert_eq!(s.lookup("PartyMember::slot").unwrap().offset, 0x40);
    }

    #[test]
    fn ambiguous_bare_reference_reports_candidates() {
        let s = Symbols::parse(DUMP);
        // `currentHp` exists on both PartyMember classes.
        match s.lookup("PartyMember::currentHp") {
            Err(LookupError::Ambiguous(mut c)) => {
                c.sort();
                assert_eq!(
                    c,
                    vec![
                        "Combat.PartyMember::currentHp".to_string(),
                        "Ui.PartyMember::currentHp".to_string()
                    ]
                );
            }
            other => panic!("expected ambiguity, got {other:?}"),
        }
        // Qualifying it resolves.
        assert_eq!(
            s.lookup("Combat.PartyMember::currentHp").unwrap().offset,
            0x18
        );
        assert_eq!(s.lookup("Ui.PartyMember::currentHp").unwrap().offset, 0x44);
    }

    #[test]
    fn nested_type_does_not_leak_into_parent_stack() {
        // After the nested enum closes, the second class's fields must still be
        // attributed to it — proof the brace stack popped correctly.
        let s = Symbols::parse(DUMP);
        assert_eq!(s.lookup("Ui.PartyMember::slot").unwrap().offset, 0x40);
        // 6 fields total: gold, _instance, leader, currentHp, maxHp, backing,
        // manager, slot, Ui.currentHp = 9.
        assert_eq!(s.len(), 9);
    }

    #[test]
    fn rip_block_is_carried_through_verbatim() {
        // The x64 static-base shape: a Tier-2 accessor with a RIP-relative decode
        // the author found by hand. The converter copies `rip` through unchanged
        // (hex or decimal accepted, since it reuses the profile's `Rip` parsing).
        let map = r#"{
          "process": "g.exe", "module": "GameAssembly.dll",
          "probe": "90 90",
          "watches": [
            { "name": "hp", "tier": "tier2",
              "anchor": "48 8B 05 ?? ?? ?? ?? 48 8B 88",
              "rip": { "disp": 3, "len": 7 },
              "chain": ["GameManager::gold"], "type": "i32" }
          ]
        }"#;
        let profile = convert_files(DUMP, map).expect("convert");
        match &profile.watches[0] {
            Watch::Tier2 { rip, offsets, .. } => {
                assert_eq!(*rip, Some(Rip { disp: 3, len: 7 }));
                assert_eq!(offsets, &vec![0x20]);
            }
            other => panic!("expected Tier2, got {other:?}"),
        }
    }

    fn map_json() -> &'static str {
        r#"{
          "label": "Sea of Stars (test)",
          "process": "SeaOfStars.exe",
          "module": "GameAssembly.dll",
          "version": "1.0.0",
          "probe": { "string": "Combat.PartyMember" },
          "watches": [
            { "name": "hp", "tier": "tier1",
              "chain": ["0x2C4E120", 16, "GameManager::leader", "Combat.PartyMember::currentHp"],
              "type": "i32", "rate_hz": 10.0 },
            { "name": "gold", "tier": "tier2",
              "anchor": "48 8B 05 ?? ?? ?? ?? 48 8B 88",
              "chain": ["GameManager::gold"], "type": "u32" }
          ]
        }"#
    }

    #[test]
    fn converts_a_full_spec_to_a_valid_profile() {
        let profile = convert_files(DUMP, map_json()).expect("convert");

        assert_eq!(profile.label.as_deref(), Some("Sea of Stars (test)"));
        assert_eq!(profile.match_.process, "SeaOfStars.exe");
        assert_eq!(profile.match_.module, "GameAssembly.dll");
        assert_eq!(profile.match_.version.as_deref(), Some("1.0.0"));
        // "Combat.PartyMember" encoded as UTF-8 bytes.
        assert_eq!(
            profile.match_.probe,
            "43 6F 6D 62 61 74 2E 50 61 72 74 79 4D 65 6D 62 65 72"
        );

        match &profile.watches[0] {
            Watch::Tier1 {
                name,
                module,
                offsets,
                ty,
                rate_hz,
            } => {
                assert_eq!(name, "hp");
                assert_eq!(module, "GameAssembly.dll"); // defaulted from the spec
                assert_eq!(offsets, &vec![0x2C4E120, 16, 0x28, 0x18]);
                assert_eq!(*ty, ValueType::I32);
                assert_eq!(*rate_hz, Some(10.0));
            }
            other => panic!("expected Tier1, got {other:?}"),
        }
        match &profile.watches[1] {
            Watch::Tier2 {
                name,
                anchor,
                rip,
                offsets,
                ty,
                ..
            } => {
                assert_eq!(name, "gold");
                assert_eq!(anchor, "48 8B 05 ?? ?? ?? ?? 48 8B 88");
                // No `rip` in the map -> none emitted (the AOB hit is the start).
                assert_eq!(*rip, None);
                assert_eq!(offsets, &vec![0x20]);
                assert_eq!(*ty, ValueType::U32);
            }
            other => panic!("expected Tier2, got {other:?}"),
        }
    }

    #[test]
    fn emitted_profile_round_trips_through_the_runtime_parser() {
        // The converter's whole point: what it emits must be a profile the
        // engine can load unchanged.
        let profile = convert_files(DUMP, map_json()).expect("convert");
        let json = profile.to_json().expect("serialize");
        let reparsed = Profile::from_json(&json).expect("the runtime must parse it");
        assert_eq!(reparsed, profile);
    }

    #[test]
    fn unknown_field_is_a_clear_error() {
        let map = r#"{
          "process": "g.exe", "module": "GameAssembly.dll",
          "probe": "90 90",
          "watches": [
            { "name": "hp", "tier": "tier1", "chain": ["GameManager::nope"], "type": "i32" }
          ]
        }"#;
        match convert_files(DUMP, map) {
            Err(ConvertError::UnknownField { watch, field }) => {
                assert_eq!(watch, "hp");
                assert_eq!(field, "GameManager::nope");
            }
            other => panic!("expected UnknownField, got {other:?}"),
        }
    }

    #[test]
    fn ambiguous_field_in_a_watch_is_rejected() {
        let map = r#"{
          "process": "g.exe", "module": "GameAssembly.dll",
          "probe": "90 90",
          "watches": [
            { "name": "hp", "tier": "tier1", "chain": ["PartyMember::currentHp"], "type": "i32" }
          ]
        }"#;
        assert!(matches!(
            convert_files(DUMP, map),
            Err(ConvertError::AmbiguousField { .. })
        ));
    }

    #[test]
    fn a_bad_probe_is_caught_before_a_bad_profile_ships() {
        let map = r#"{
          "process": "g.exe", "module": "GameAssembly.dll",
          "probe": "not hex at all",
          "watches": []
        }"#;
        assert!(matches!(
            convert_files(DUMP, map),
            Err(ConvertError::BadProbe(_))
        ));
    }

    #[test]
    fn a_bad_anchor_is_caught() {
        let map = r#"{
          "process": "g.exe", "module": "GameAssembly.dll",
          "probe": "90 90",
          "watches": [
            { "name": "gold", "tier": "tier2", "anchor": "zz", "chain": [0], "type": "u32" }
          ]
        }"#;
        assert!(matches!(
            convert_files(DUMP, map),
            Err(ConvertError::BadAnchor { .. })
        ));
    }

    #[test]
    fn numeric_literals_pass_through_in_both_forms() {
        // A chain of pure literals (number, hex string, negative) is resolved
        // without touching the dump — the "author supplies the raw parts" path.
        let map = r#"{
          "process": "g.exe", "module": "GameAssembly.dll",
          "probe": "90",
          "watches": [
            { "name": "v", "tier": "tier1", "chain": [16, "0x18", "-4"], "type": "u64" }
          ]
        }"#;
        let profile = convert_files(DUMP, map).expect("convert");
        match &profile.watches[0] {
            Watch::Tier1 { offsets, .. } => assert_eq!(offsets, &vec![16, 0x18, -4]),
            other => panic!("expected Tier1, got {other:?}"),
        }
    }

    #[test]
    fn utf16le_probe_encoding() {
        // Two bytes per code unit, low byte first.
        assert_eq!(
            encode_signature("Hi", StringEncoding::Utf16le),
            "48 00 69 00"
        );
    }
}
