//! The Apinata kind: Bruno-shaped collections whose root `environments/*.yml` must not carry a
//! value for a `secret: true` variable.

use std::cell::Cell;
use std::fmt;
use std::sync::LazyLock;

use globset::{GlobBuilder, GlobMatcher};
use serde::de::{
    DeserializeSeed, Deserializer, EnumAccess, Error as _, MapAccess, SeqAccess, VariantAccess,
    Visitor,
};
use yaml_serde::mapping::Entry;
use yaml_serde::value::{Tag, TaggedValue};
use yaml_serde::{Mapping, Value};

use super::{Blob, Change, Code, Finding, RULES_ABI, RuleSet, SeedFile};

/// The rule name of a `secret: true` variable carrying a value.
pub const SECRET_VALUE: &str = "secret_value";
/// The rule name of an `environments/*.yml` file Apinata's reader would refuse.
pub const ENV_UNPARSABLE: &str = "env_unparsable";

/// The one kind of v1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Apinata;

impl RuleSet for Apinata {
    fn kind(&self) -> &'static str {
        "apinata"
    }

    fn abi(&self) -> u32 {
        RULES_ABI
    }

    fn formats(&self) -> &'static [&'static str] {
        &["1.0.0"]
    }

    fn format_file(&self) -> &'static str {
        "opencollection.yml"
    }

    fn detect_format(&self, format_file: &[u8]) -> Option<String> {
        detect_format(format_file)
    }

    /// No path rule of its own: `.env` belongs to the generic set.
    fn path_rule(&self, _path: &str) -> Option<Finding> {
        None
    }

    fn reads(&self, path: &str) -> bool {
        is_environment_file(path)
    }

    /// The environment rule on the new side of an environment file; a deleted file passes.
    /// Baseline and role do not matter, and a blob the hook did not read is `env_unparsable`.
    fn check(&self, change: &Change<'_>) -> Vec<Finding> {
        let path = change.path();
        if !is_environment_file(path) {
            return Vec::new();
        }
        let found = match change.new_blob() {
            None => None,
            Some(Blob::Bytes(blob)) => environment_rule(path, blob, NODE_CAP),
            // An unread blob, or any side this version does not know, cannot be vouched for.
            Some(_) => Some(Finding::new(Code::SecretValue, ENV_UNPARSABLE, path)),
        };
        found.into_iter().collect()
    }

    fn seed(&self) -> Vec<SeedFile> {
        seed()
    }
}

/// The format version `opencollection.yml` declares: the first top-level `opencollection:`
/// line's value, with a trailing ` #` comment and one pair of matching quotes removed.
/// `None` for bytes that are not UTF-8, no such line, or an empty value.
pub fn detect_format(bytes: &[u8]) -> Option<String> {
    // A line scan, not a YAML parse: the version must be readable even when the rest of the
    // file is not, and only a key at column 0 is top level.
    let text = std::str::from_utf8(bytes).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let line = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .find_map(|l| l.strip_prefix("opencollection:"))?;
    let value = line.split_once(" #").map_or(line, |(v, _)| v).trim();
    let value = ['"', '\'']
        .iter()
        .find_map(|q| value.strip_prefix(*q)?.strip_suffix(*q))
        .unwrap_or(value);
    (!value.is_empty()).then(|| value.to_owned())
}

/// Nodes one document may hold after alias expansion: every scalar, sequence and mapping
/// built, keys included, each alias expansion counted again. Above it the file is
/// `env_unparsable`, as Apinata's loader refuses it.
pub const NODE_CAP: usize = 2_000_000;

/// The environment-file rule on `blob` with an explicit `node_cap`, reported at `path`:
/// `env_unparsable` when the bytes are not one YAML document, hold more than `node_cap` nodes
/// or carry a merge key yaml_serde cannot apply (any `<<` inside a mapping key), `secret_value`
/// for a valued secret once merges are folded in, else `None`. `check` applies it with
/// `NODE_CAP` to every path `is_environment_file` accepts.
pub fn environment_rule(path: &str, blob: &[u8], node_cap: usize) -> Option<Finding> {
    // Fail closed: Apinata reads with the same parser, so a file it refuses is one no
    // honest user relies on, and passing it would hide whatever it holds from the walk.
    // Count first so a merge cannot bypass the cap, merge before the walk so a pulled-in
    // value is seen.
    let Ok(mut doc) = parse_environment(blob, node_cap) else {
        return Some(Finding::new(Code::SecretValue, ENV_UNPARSABLE, path));
    };
    if !fold_merges(&mut doc) {
        return Some(Finding::new(Code::SecretValue, ENV_UNPARSABLE, path));
    }
    has_valued_secret(&doc).then(|| Finding::new(Code::SecretValue, SECRET_VALUE, path))
}

// Rounds of `apply_merge` allowed before a leftover `<<` counts as unparsable; the
// parser's nesting limit, since each round folds at least one level.
const MERGE_ROUNDS: usize = 128;

// One `apply_merge` folds a single level, so a `<<` carried in by a merge survives it;
// repeat until none is left. Merges move entries and never clone, so no round grows the
// tree past what the counted parse paid for. `false` on a merge error or the bound, and at
// once for a `<<` inside a key, which `apply_merge` never folds since it walks values only.
fn fold_merges(doc: &mut Value) -> bool {
    if merge_in_a_key(doc) {
        return false;
    }
    for _ in 0..MERGE_ROUNDS {
        if !has_merge_key(doc) {
            return true;
        }
        if doc.apply_merge().is_err() {
            return false;
        }
    }
    !has_merge_key(doc)
}

fn has_merge_key(node: &Value) -> bool {
    match node {
        Value::Mapping(map) => {
            map.contains_key("<<")
                || map
                    .iter()
                    .any(|(k, v)| has_merge_key(k) || has_merge_key(v))
        }
        Value::Sequence(items) => items.iter().any(has_merge_key),
        Value::Tagged(tagged) => has_merge_key(&tagged.value),
        _ => false,
    }
}

// Hands each mapping key to `has_merge_key` once and recurses only through values, items and
// tags, so every node is visited a bounded number of times.
fn merge_in_a_key(node: &Value) -> bool {
    match node {
        Value::Mapping(map) => map
            .iter()
            .any(|(k, v)| has_merge_key(k) || merge_in_a_key(v)),
        Value::Sequence(items) => items.iter().any(merge_in_a_key),
        Value::Tagged(tagged) => merge_in_a_key(&tagged.value),
        _ => false,
    }
}

// Builds the same `Value` yaml_serde would, spending one unit of budget per node, so an alias
// fan-out stops at the cap instead of filling memory first.
struct Counted<'a> {
    budget: &'a Cell<usize>,
}

impl Counted<'_> {
    fn spend<E: serde::de::Error>(&self) -> Result<(), E> {
        match self.budget.get().checked_sub(1) {
            Some(left) => {
                self.budget.set(left);
                Ok(())
            }
            None => Err(E::custom("node cap exceeded")),
        }
    }

    fn child(&self) -> Counted<'_> {
        Counted {
            budget: self.budget,
        }
    }
}

impl<'de> DeserializeSeed<'de> for Counted<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Counted<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any YAML value")
    }

    fn visit_bool<E: serde::de::Error>(self, b: bool) -> Result<Value, E> {
        self.spend()?;
        Ok(Value::Bool(b))
    }

    fn visit_i64<E: serde::de::Error>(self, i: i64) -> Result<Value, E> {
        self.spend()?;
        Ok(Value::Number(i.into()))
    }

    fn visit_u64<E: serde::de::Error>(self, u: u64) -> Result<Value, E> {
        self.spend()?;
        Ok(Value::Number(u.into()))
    }

    fn visit_f64<E: serde::de::Error>(self, f: f64) -> Result<Value, E> {
        self.spend()?;
        Ok(Value::Number(f.into()))
    }

    fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Value, E> {
        self.spend()?;
        Ok(Value::String(s.to_owned()))
    }

    fn visit_string<E: serde::de::Error>(self, s: String) -> Result<Value, E> {
        self.spend()?;
        Ok(Value::String(s))
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        self.spend()?;
        Ok(Value::Null)
    }

    fn visit_none<E: serde::de::Error>(self) -> Result<Value, E> {
        self.spend()?;
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        self.spend()?;
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(self.child())? {
            items.push(item);
        }
        Ok(Value::Sequence(items))
    }

    // Duplicate keys are refused here, as yaml_serde's own Mapping visitor refuses them.
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        self.spend()?;
        let mut mapping = Mapping::new();
        while let Some(key) = map.next_key_seed(self.child())? {
            match mapping.entry(key) {
                Entry::Occupied(_) => return Err(A::Error::custom("duplicate key")),
                Entry::Vacant(slot) => {
                    slot.insert(map.next_value_seed(self.child())?);
                }
            }
        }
        Ok(Value::Mapping(mapping))
    }

    // The tag wrapper is not a node; only what it wraps is counted.
    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Value, A::Error> {
        let (tag, contents) = data.variant::<String>()?;
        if tag.is_empty() {
            return Err(A::Error::custom("empty tag"));
        }
        let value = contents.newtype_variant_seed(self.child())?;
        Ok(Value::Tagged(Box::new(TaggedValue {
            tag: Tag::new(tag),
            value,
        })))
    }
}

fn parse_environment(blob: &[u8], node_cap: usize) -> Result<Value, yaml_serde::Error> {
    let budget = Cell::new(node_cap);
    Counted { budget: &budget }.deserialize(yaml_serde::Deserializer::from_slice(blob))
}

// Depth-first through mapping keys and values, sequences and tags, so a complex key cannot
// carry a secret past the walk. Only `null` means "no value", so an empty string still counts.
fn has_valued_secret(node: &Value) -> bool {
    match node {
        Value::Mapping(map) => {
            let secret = map.get("secret") == Some(&Value::Bool(true));
            let valued = map.get("value").is_some_and(|v| !v.is_null());
            (secret && valued)
                || map
                    .iter()
                    .any(|(k, v)| has_valued_secret(k) || has_valued_secret(v))
        }
        Value::Sequence(items) => items.iter().any(has_valued_secret),
        Value::Tagged(tagged) => has_valued_secret(&tagged.value),
        _ => false,
    }
}

/// `environments/*.yml` at the repository root, as Apinata reads them: the literal directory,
/// case-sensitive, the file directly inside it, `.yml` only.
pub fn is_environment_file(path: &str) -> bool {
    ENVIRONMENT_FILE.is_match(path)
}

/// The seed: `.gitignore` then `opencollection.yml`, both at the workspace root, bytes
/// vendored from Apinata's `Collection::create`; they are a contract, copied, never edited.
pub fn seed() -> Vec<SeedFile> {
    vec![
        SeedFile::new(".gitignore", include_bytes!("seed/.gitignore")),
        SeedFile::new(
            "opencollection.yml",
            include_bytes!("seed/opencollection.yml"),
        ),
    ]
}

// Literal separators keep `*` inside one path segment, so a nested directory never matches.
#[allow(
    clippy::expect_used,
    reason = "the glob is a constant; a bad pattern is a build defect the tests catch on first use"
)]
static ENVIRONMENT_FILE: LazyLock<GlobMatcher> = LazyLock::new(|| {
    GlobBuilder::new("environments/*.yml")
        .literal_separator(true)
        .build()
        .expect("the environment file glob compiles")
        .compile_matcher()
});
