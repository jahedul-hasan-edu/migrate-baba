//! Object-level comparison of two catalogs (drives the compare UI).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::ddl;
use crate::model::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ObjectKind {
    Schema,
    Extension,
    Type,
    Sequence,
    Table,
    View,
    MaterializedView,
    Function,
    Procedure,
}

impl ObjectKind {
    pub const ALL: [ObjectKind; 9] = [
        ObjectKind::Schema,
        ObjectKind::Extension,
        ObjectKind::Type,
        ObjectKind::Sequence,
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
        ObjectKind::Function,
        ObjectKind::Procedure,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ObjectKind::Schema => "Schema",
            ObjectKind::Extension => "Extension",
            ObjectKind::Type => "Type",
            ObjectKind::Sequence => "Sequence",
            ObjectKind::Table => "Table",
            ObjectKind::View => "View",
            ObjectKind::MaterializedView => "Materialized view",
            ObjectKind::Function => "Function",
            ObjectKind::Procedure => "Procedure",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            ObjectKind::Schema => "SCH",
            ObjectKind::Extension => "EXT",
            ObjectKind::Type => "TYP",
            ObjectKind::Sequence => "SEQ",
            ObjectKind::Table => "TBL",
            ObjectKind::View => "VW",
            ObjectKind::MaterializedView => "MV",
            ObjectKind::Function => "FN",
            ObjectKind::Procedure => "PROC",
        }
    }
}

/// Identity of a top-level object. For schemas/extensions `schema` is empty.
/// For functions `name` is `name(identity args)`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ObjectKey {
    pub kind: ObjectKind,
    pub schema: String,
    pub name: String,
}

impl ObjectKey {
    pub fn new(kind: ObjectKind, schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self { kind, schema: schema.into(), name: name.into() }
    }

    pub fn display(&self) -> String {
        if self.schema.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.schema, self.name)
        }
    }

    pub fn qname(&self) -> QName {
        QName::new(self.schema.clone(), self.name.clone())
    }

    /// Key into `Catalog::functions`.
    pub fn function_key(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DiffStatus {
    /// Only in source → will be created on target.
    Added,
    /// In both but different → will be altered.
    Changed,
    /// Only in target → will be dropped.
    Removed,
    Identical,
}

impl DiffStatus {
    pub fn label(self) -> &'static str {
        match self {
            DiffStatus::Added => "To create",
            DiffStatus::Changed => "To alter",
            DiffStatus::Removed => "To drop",
            DiffStatus::Identical => "Identical",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjectDiff {
    pub key: ObjectKey,
    pub status: DiffStatus,
    /// DDL of the object in the source (empty if absent).
    pub source_ddl: String,
    /// DDL of the object in the target (empty if absent).
    pub target_ddl: String,
    /// Human readable list of changes.
    pub details: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DiffSummary {
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    pub identical: usize,
}

pub fn summarize(diffs: &[ObjectDiff]) -> DiffSummary {
    let mut s = DiffSummary::default();
    for d in diffs {
        match d.status {
            DiffStatus::Added => s.added += 1,
            DiffStatus::Changed => s.changed += 1,
            DiffStatus::Removed => s.removed += 1,
            DiffStatus::Identical => s.identical += 1,
        }
    }
    s
}

fn push_diff<T>(
    out: &mut Vec<ObjectDiff>,
    key: ObjectKey,
    src: Option<&T>,
    tgt: Option<&T>,
    render: impl Fn(&T) -> String,
    details: impl Fn(&T, &T) -> Vec<String>,
) {
    let (status, det) = match (src, tgt) {
        (Some(_), None) => (DiffStatus::Added, vec!["only in source".to_string()]),
        (None, Some(_)) => (DiffStatus::Removed, vec!["only in target".to_string()]),
        (Some(s), Some(t)) => {
            let d = details(s, t);
            if d.is_empty() {
                (DiffStatus::Identical, d)
            } else {
                (DiffStatus::Changed, d)
            }
        }
        (None, None) => return,
    };
    out.push(ObjectDiff {
        key,
        status,
        source_ddl: src.map(&render).unwrap_or_default(),
        target_ddl: tgt.map(&render).unwrap_or_default(),
        details: det,
    });
}

fn union_keys<'a, K: Ord + Clone + 'a, V>(a: &'a BTreeMap<K, V>, b: &'a BTreeMap<K, V>) -> BTreeSet<K> {
    a.keys().chain(b.keys()).cloned().collect()
}

fn opt_change(out: &mut Vec<String>, what: &str, src: &Option<String>, tgt: &Option<String>) {
    if src != tgt {
        out.push(format!(
            "{what}: {} → {}",
            tgt.as_deref().unwrap_or("(none)"),
            src.as_deref().unwrap_or("(none)")
        ));
    }
}

/// Compare `source` (desired) against `target` (current).
pub fn compare(source: &Catalog, target: &Catalog) -> Vec<ObjectDiff> {
    let mut out = Vec::new();

    for k in union_keys(&source.schemas, &target.schemas) {
        push_diff(
            &mut out,
            ObjectKey::new(ObjectKind::Schema, "", k.clone()),
            source.schemas.get(&k),
            target.schemas.get(&k),
            ddl::create_schema,
            |s, t| {
                let mut d = Vec::new();
                opt_change(&mut d, "comment", &s.comment, &t.comment);
                d
            },
        );
    }
    for k in union_keys(&source.extensions, &target.extensions) {
        push_diff(
            &mut out,
            ObjectKey::new(ObjectKind::Extension, "", k.clone()),
            source.extensions.get(&k),
            target.extensions.get(&k),
            ddl::create_extension,
            |s, t| {
                let mut d = Vec::new();
                if s.version != t.version {
                    d.push(format!("version: {} → {}", t.version, s.version));
                }
                if s.schema != t.schema {
                    d.push(format!("schema: {} → {}", t.schema, s.schema));
                }
                d
            },
        );
    }
    for k in union_keys(&source.types, &target.types) {
        push_diff(
            &mut out,
            ObjectKey::new(ObjectKind::Type, k.schema.clone(), k.name.clone()),
            source.types.get(&k),
            target.types.get(&k),
            ddl::create_type,
            type_details,
        );
    }
    for k in union_keys(&source.sequences, &target.sequences) {
        push_diff(
            &mut out,
            ObjectKey::new(ObjectKind::Sequence, k.schema.clone(), k.name.clone()),
            source.sequences.get(&k),
            target.sequences.get(&k),
            ddl::create_sequence,
            |s, t| {
                let mut d = Vec::new();
                if ddl::sequence_options(s) != ddl::sequence_options(t) {
                    d.push(format!("options: {} → {}", ddl::sequence_options(t), ddl::sequence_options(s)));
                }
                opt_change(&mut d, "owned by", &s.owned_by, &t.owned_by);
                d
            },
        );
    }
    for k in union_keys(&source.tables, &target.tables) {
        push_diff(
            &mut out,
            ObjectKey::new(ObjectKind::Table, k.schema.clone(), k.name.clone()),
            source.tables.get(&k),
            target.tables.get(&k),
            ddl::table_full,
            table_details,
        );
    }
    for k in union_keys(&source.views, &target.views) {
        let s = source.views.get(&k);
        let t = target.views.get(&k);
        let mat = s.or(t).map(|v| v.materialized).unwrap_or(false);
        push_diff(
            &mut out,
            ObjectKey::new(
                if mat { ObjectKind::MaterializedView } else { ObjectKind::View },
                k.schema.clone(),
                k.name.clone(),
            ),
            s,
            t,
            ddl::create_view,
            |s, t| {
                let mut d = Vec::new();
                if s.materialized != t.materialized {
                    d.push("materialized flag differs".into());
                }
                if normalize_body(&s.definition) != normalize_body(&t.definition) {
                    d.push("definition differs".into());
                }
                opt_change(&mut d, "options", &s.options, &t.options);
                if s.indexes != t.indexes {
                    d.push("indexes differ".into());
                }
                opt_change(&mut d, "comment", &s.comment, &t.comment);
                d
            },
        );
    }
    for k in union_keys(&source.functions, &target.functions) {
        let s = source.functions.get(&k);
        let t = target.functions.get(&k);
        let f = s.or(t).expect("key from union");
        let kind = if f.kind == FunctionKind::Procedure { ObjectKind::Procedure } else { ObjectKind::Function };
        push_diff(
            &mut out,
            ObjectKey::new(kind, f.schema.clone(), format!("{}({})", f.name, f.identity_args)),
            s,
            t,
            ddl::create_function,
            |s, t| {
                let mut d = Vec::new();
                if s.kind != t.kind {
                    d.push("function/procedure kind differs".into());
                }
                opt_change(&mut d, "returns", &s.result, &t.result);
                if s.language != t.language {
                    d.push(format!("language: {} → {}", t.language, s.language));
                }
                if normalize_body(&s.definition) != normalize_body(&t.definition) {
                    d.push("body/attributes differ".into());
                }
                opt_change(&mut d, "comment", &s.comment, &t.comment);
                d
            },
        );
    }
    out
}

pub fn type_details(s: &TypeDef, t: &TypeDef) -> Vec<String> {
    let mut d = Vec::new();
    match (&s.kind, &t.kind) {
        (TypeKind::Enum { labels: a }, TypeKind::Enum { labels: b }) => {
            if a != b {
                let added: Vec<_> = a.iter().filter(|l| !b.contains(l)).cloned().collect();
                let removed: Vec<_> = b.iter().filter(|l| !a.contains(l)).cloned().collect();
                if !added.is_empty() {
                    d.push(format!("enum values added: {}", added.join(", ")));
                }
                if !removed.is_empty() {
                    d.push(format!("enum values removed: {}", removed.join(", ")));
                }
                if added.is_empty() && removed.is_empty() {
                    d.push("enum value order differs".into());
                }
            }
        }
        (
            TypeKind::Domain { base: b1, not_null: n1, default: d1, checks: c1 },
            TypeKind::Domain { base: b2, not_null: n2, default: d2, checks: c2 },
        ) => {
            if b1 != b2 {
                d.push(format!("base type: {b2} → {b1}"));
            }
            if n1 != n2 {
                d.push(format!("not null: {n2} → {n1}"));
            }
            opt_change(&mut d, "default", d1, d2);
            if c1 != c2 {
                d.push("check constraints differ".into());
            }
        }
        (TypeKind::Composite { attrs: a }, TypeKind::Composite { attrs: b }) => {
            if a != b {
                d.push("attributes differ".into());
            }
        }
        _ => d.push("type kind differs (enum/domain/composite)".into()),
    }
    opt_change(&mut d, "comment", &s.comment, &t.comment);
    d
}

pub fn table_details(s: &TableDef, t: &TableDef) -> Vec<String> {
    let mut d = Vec::new();
    if s.partition_key != t.partition_key {
        d.push("partition key differs (manual change required)".into());
    }
    if s.partition_of != t.partition_of || s.partition_bound != t.partition_bound {
        d.push("partition parent/bound differs".into());
    }
    if s.unlogged != t.unlogged {
        d.push(format!("{} → {}", if t.unlogged { "UNLOGGED" } else { "LOGGED" }, if s.unlogged { "UNLOGGED" } else { "LOGGED" }));
    }
    let is_partition = s.partition_of.is_some();
    if !is_partition {
        for c in &s.columns {
            match t.column(&c.name) {
                None => d.push(format!("column added: {} {}", c.name, c.data_type)),
                Some(tc) => {
                    if c.data_type != tc.data_type {
                        d.push(format!("column {}: type {} → {}", c.name, tc.data_type, c.data_type));
                    }
                    if c.not_null != tc.not_null {
                        d.push(format!(
                            "column {}: {}",
                            c.name,
                            if c.not_null { "set NOT NULL" } else { "drop NOT NULL" }
                        ));
                    }
                    opt_change(&mut d, &format!("column {} default", c.name), &c.default, &tc.default);
                    if c.identity != tc.identity {
                        d.push(format!("column {}: identity {:?} → {:?}", c.name, tc.identity, c.identity));
                    }
                    opt_change(&mut d, &format!("column {} generated", c.name), &c.generated, &tc.generated);
                    opt_change(&mut d, &format!("column {} collation", c.name), &c.collation, &tc.collation);
                    opt_change(&mut d, &format!("column {} comment", c.name), &c.comment, &tc.comment);
                }
            }
        }
        for tc in &t.columns {
            if s.column(&tc.name).is_none() {
                d.push(format!("column dropped: {} {}", tc.name, tc.data_type));
            }
        }
    }
    keyed_details(&mut d, "constraint", &s.constraints, &t.constraints, |a, b| a.definition != b.definition);
    keyed_details(&mut d, "index", &s.indexes, &t.indexes, |a, b| a.definition != b.definition);
    keyed_details(&mut d, "trigger", &s.triggers, &t.triggers, |a, b| a != b);
    keyed_details(&mut d, "policy", &s.policies, &t.policies, |a, b| a != b);
    if s.rls_enabled != t.rls_enabled {
        d.push(format!("row level security {}", if s.rls_enabled { "enabled" } else { "disabled" }));
    }
    if s.rls_forced != t.rls_forced {
        d.push(format!("force row level security {}", if s.rls_forced { "on" } else { "off" }));
    }
    opt_change(&mut d, "comment", &s.comment, &t.comment);
    d
}

fn keyed_details<V>(
    d: &mut Vec<String>,
    what: &str,
    s: &BTreeMap<String, V>,
    t: &BTreeMap<String, V>,
    differs: impl Fn(&V, &V) -> bool,
) {
    for (k, v) in s {
        match t.get(k) {
            None => d.push(format!("{what} added: {k}")),
            Some(tv) if differs(v, tv) => d.push(format!("{what} changed: {k}")),
            _ => {}
        }
    }
    for k in t.keys() {
        if !s.contains_key(k) {
            d.push(format!("{what} dropped: {k}"));
        }
    }
}
