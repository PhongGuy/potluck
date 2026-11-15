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

use super::{Code, Finding, Kind, SeedFile};

/// The rule name of a `secret: true` variable carrying a value.
pub const SECRET_VALUE: &str = "secret_value";
/// The rule name of an `environments/*.yml` file Apinata's reader would refuse.
pub const ENV_UNPARSABLE: &str = "env_unparsable";

/// The one kind of v1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Apinata;

impl Kind for Apinata {
    fn name(&self) -> &'static str {
        "apinata"
    }

    /// No path rule of its own: `.env` belongs to the generic set.
    fn path_rule(&self, _path: &str) -> Option<Finding> {
        None
    }

    /// For an environment file only: `env_unparsable` when the blob is not exactly one YAML
    /// document yaml_serde accepts within `NODE_CAP` nodes, `secret_value` when that document
    /// holds, at any depth, a mapping with `secret: true` (the boolean) and a non-null `value`.
    fn content_rule(&self, path: &str, blob: &[u8]) -> Option<Finding> {
        is_environment_file(path)
            .then(|| environment_rule(blob, NODE_CAP))
            .flatten()
    }

    /// The environment files, the only paths `content_rule` reads.
    fn content_path(&self, path: &str) -> bool {
        is_environment_file(path)
    }

    fn seed(&self) -> Vec<SeedFile> {
        seed()
    }
}

/// Nodes one document may hold after alias expansion: every scalar, sequence and mapping
/// built, keys included, each alias expansion counted again. Above it the file is
/// `env_unparsable`, as Apinata's loader refuses it.
pub const NODE_CAP: usize = 2_000_000;

/// The environment-file rule on `blob` alone with an explicit `node_cap`: `env_unparsable`
/// when the bytes are not one YAML document, hold more than `node_cap` nodes or carry a
/// merge key yaml_serde cannot apply, `secret_value` for a valued secret once merges are
/// folded in, else `None`. `content_rule` applies it with `NODE_CAP` to every path
/// `is_environment_file` accepts.
pub fn environment_rule(blob: &[u8], node_cap: usize) -> Option<Finding> {
    // Fail closed: Apinata reads with the same parser, so a file it refuses is one no
    // honest user relies on, and passing it would hide whatever it holds from the walk.
    // Count first so a merge cannot bypass the cap, merge before the walk so a pulled-in
    // value is seen.
    let Ok(mut doc) = parse_environment(blob, node_cap) else {
        return Some(Finding::new(Code::SecretValue, ENV_UNPARSABLE));
    };
    if !fold_merges(&mut doc) {
        return Some(Finding::new(Code::SecretValue, ENV_UNPARSABLE));
    }
    has_valued_secret(&doc).then(|| Finding::new(Code::SecretValue, SECRET_VALUE))
}

// Rounds of `apply_merge` allowed before a leftover `<<` counts as unparsable; the
// parser's nesting limit, since each round folds at least one level.
const MERGE_ROUNDS: usize = 128;

// One `apply_merge` folds a single level, so a `<<` carried in by a merge survives it;
// repeat until none is left. Merges move entries and never clone, so no round grows the
// tree past what the counted parse paid for. `false` on a merge error or the bound.
fn fold_merges(doc: &mut Value) -> bool {
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
        Value::Mapping(map) => map.contains_key("<<") || map.values().any(has_merge_key),
        Value::Sequence(items) => items.iter().any(has_merge_key),
        Value::Tagged(tagged) => has_merge_key(&tagged.value),
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

// Depth-first through mappings, sequences and tags. Only `null` means "no value", so an
// empty string still counts as one.
fn has_valued_secret(node: &Value) -> bool {
    match node {
        Value::Mapping(map) => {
            let secret = map.get("secret") == Some(&Value::Bool(true));
            let valued = map.get("value").is_some_and(|v| !v.is_null());
            (secret && valued) || map.values().any(has_valued_secret)
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
