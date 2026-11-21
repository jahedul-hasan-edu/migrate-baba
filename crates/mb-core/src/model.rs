//! In-memory catalog model of a PostgreSQL database (the subset we migrate).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ident::qualified;

/// Schema-qualified object name.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct QName {
    pub schema: String,
    pub name: String,
}

impl QName {
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self { schema: schema.into(), name: name.into() }
    }

    /// Quoted SQL form, e.g. `public."Order"`.
    pub fn sql(&self) -> String {
        qualified(&self.schema, &self.name)
    }

    /// Parse the output of `quote_ident(schema)||'.'||quote_ident(name)`.
    pub fn parse_quoted(s: &str) -> Option<Self> {
        let parts = split_quoted_ident(s);
        if parts.len() == 2 {
            Some(QName::new(parts[0].clone(), parts[1].clone()))
        } else {
            None
        }
    }
}

impl fmt::Display for QName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.schema, self.name)
    }
}

/// Split `a."b.c".d` into its unquoted parts.
pub fn split_quoted_ident(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut in_quotes = false;
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes => {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            }
            '"' => in_quotes = true,
            '.' if !in_quotes => parts.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    parts.push(cur);
    parts
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaDef {
    pub name: String,
    pub comment: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionDef {
    pub name: String,
    pub schema: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeKind {
    Enum { labels: Vec<String> },
    Domain { base: String, not_null: bool, default: Option<String>, checks: Vec<(String, String)> },
    Composite { attrs: Vec<(String, String)> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeDef {
    pub name: QName,
    pub kind: TypeKind,
    pub comment: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequenceDef {
    pub name: QName,
    pub data_type: String,
    pub start: i64,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
    pub cache: i64,
    pub cycle: bool,
    /// `schema.table.column` (quoted parts) the sequence is OWNED BY.
    pub owned_by: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Identity {
    Always,
    ByDefault,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub data_type: String,
    /// Informational only (used to pick COPY format); not compared.
    pub type_oid: u32,
    pub not_null: bool,
    pub default: Option<String>,
    pub identity: Option<Identity>,
    /// Expression of a `GENERATED ALWAYS AS (…) STORED` column.
    pub generated: Option<String>,
    pub collation: Option<String>,
    pub comment: Option<String>,
}

impl PartialEq for Column {
    fn eq(&self, o: &Self) -> bool {
        self.name == o.name
            && self.data_type == o.data_type
            && self.not_null == o.not_null
            && self.default == o.default
            && self.identity == o.identity
            && self.generated == o.generated
            && self.collation == o.collation
            && self.comment == o.comment
    }
}
impl Eq for Column {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ConstraintKind {
    PrimaryKey,
    Unique,
    Check,
    Exclude,
    ForeignKey,
}

impl ConstraintKind {
    pub fn from_code(c: &str) -> Option<Self> {
        Some(match c {
            "p" => Self::PrimaryKey,
            "u" => Self::Unique,
            "c" => Self::Check,
            "x" => Self::Exclude,
            "f" => Self::ForeignKey,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraint {
    pub name: String,
    pub kind: ConstraintKind,
    pub definition: String,
    /// Referenced table for foreign keys.
    pub ref_table: Option<QName>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub name: String,
    pub definition: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trigger {
    pub name: String,
    pub definition: String,
    /// `O` origin/enabled, `D` disabled, `R` replica, `A` always.
    pub enabled: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub name: String,
    pub permissive: bool,
    pub roles: Vec<String>,
    /// ALL / SELECT / INSERT / UPDATE / DELETE
    pub cmd: String,
    pub using: Option<String>,
    pub with_check: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TableDef {
    pub name: QName,
    pub columns: Vec<Column>,
    pub constraints: BTreeMap<String, Constraint>,
    pub indexes: BTreeMap<String, Index>,
    pub triggers: BTreeMap<String, Trigger>,
    pub policies: BTreeMap<String, Policy>,
    pub rls_enabled: bool,
    pub rls_forced: bool,
    pub unlogged: bool,
    /// `PARTITION BY` clause body for partitioned tables (e.g. `RANGE (created_at)`).
    pub partition_key: Option<String>,
    /// Parent table when this is a partition.
    pub partition_of: Option<QName>,
    /// `FOR VALUES …` / `DEFAULT` for partitions.
    pub partition_bound: Option<String>,
    pub comment: Option<String>,
    // ---- informational, not compared ----
    pub est_rows: i64,
    pub total_bytes: i64,
    pub pages: i64,
}

impl PartialEq for TableDef {
    fn eq(&self, o: &Self) -> bool {
        self.name == o.name
            && self.columns == o.columns
            && self.constraints == o.constraints
            && self.indexes == o.indexes
            && self.triggers == o.triggers
            && self.policies == o.policies
            && self.rls_enabled == o.rls_enabled
            && self.rls_forced == o.rls_forced
            && self.unlogged == o.unlogged
            && self.partition_key == o.partition_key
            && self.partition_of == o.partition_of
            && self.partition_bound == o.partition_bound
            && self.comment == o.comment
    }
}

impl TableDef {
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    pub fn primary_key(&self) -> Option<&Constraint> {
        self.constraints.values().find(|c| c.kind == ConstraintKind::PrimaryKey)
    }

    /// Column names of the primary key, parsed from its definition `PRIMARY KEY (a, b)`.
    pub fn primary_key_columns(&self) -> Vec<String> {
        self.primary_key()
            .map(|pk| parse_column_list(&pk.definition))
            .unwrap_or_default()
    }

    pub fn foreign_key_targets(&self) -> impl Iterator<Item = &QName> {
        self.constraints.values().filter_map(|c| c.ref_table.as_ref())
    }
}

/// Parse the first parenthesised identifier list in a definition, e.g. `PRIMARY KEY (id, "Org")`.
pub fn parse_column_list(def: &str) -> Vec<String> {
    let Some(start) = def.find('(') else { return Vec::new() };
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut chars = def[start + 1..].chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_q => {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_q = false;
                }
            }
            '"' => in_q = true,
            ',' if !in_q => out.push(std::mem::take(&mut cur).trim().to_string()),
            ')' if !in_q => {
                out.push(std::mem::take(&mut cur).trim().to_string());
                break;
            }
            c => cur.push(c),
        }
    }
    out.retain(|s| !s.is_empty());
    out
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ViewDef {
    pub name: QName,
    pub materialized: bool,
    pub definition: String,
    /// `WITH (…)` options, e.g. `security_invoker=true`.
    pub options: Option<String>,
    pub depends_on: Vec<QName>,
    pub comment: Option<String>,
    pub indexes: BTreeMap<String, Index>,
}

impl PartialEq for ViewDef {
    fn eq(&self, o: &Self) -> bool {
        self.name == o.name
            && self.materialized == o.materialized
            && self.options == o.options
            && normalize_body(&self.definition) == normalize_body(&o.definition)
            && self.comment == o.comment
            && self.indexes == o.indexes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FunctionKind {
    Function,
    Procedure,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionDef {
    pub schema: String,
    pub name: String,
    pub identity_args: String,
    pub kind: FunctionKind,
    pub result: Option<String>,
    pub language: String,
    /// Full `CREATE OR REPLACE FUNCTION …` text from `pg_get_functiondef`.
    pub definition: String,
    pub comment: Option<String>,
}

impl PartialEq for FunctionDef {
    fn eq(&self, o: &Self) -> bool {
        self.schema == o.schema
            && self.name == o.name
            && self.identity_args == o.identity_args
            && self.kind == o.kind
            && self.result == o.result
            && normalize_body(&self.definition) == normalize_body(&o.definition)
            && self.comment == o.comment
    }
}

impl FunctionDef {
    pub fn key(&self) -> String {
        format!("{}.{}({})", self.schema, self.name, self.identity_args)
    }

    /// `schema.name(args)` quoted for DROP/COMMENT statements.
    pub fn signature_sql(&self) -> String {
        format!("{}({})", qualified(&self.schema, &self.name), self.identity_args)
    }

    pub fn kind_sql(&self) -> &'static str {
        match self.kind {
            FunctionKind::Function => "FUNCTION",
            FunctionKind::Procedure => "PROCEDURE",
        }
    }
}

/// Normalise whitespace for robust comparisons of SQL bodies.
pub fn normalize_body(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.replace("\r\n", "\n").lines() {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.trim().to_string()
}

/// A full database snapshot.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub server_version: i32,
    pub schemas: BTreeMap<String, SchemaDef>,
    pub extensions: BTreeMap<String, ExtensionDef>,
    pub types: BTreeMap<QName, TypeDef>,
    pub sequences: BTreeMap<QName, SequenceDef>,
    pub tables: BTreeMap<QName, TableDef>,
    pub views: BTreeMap<QName, ViewDef>,
    pub functions: BTreeMap<String, FunctionDef>,
}

impl Catalog {
    /// Keep only objects in the given schema filter.
    pub fn retain_schemas(&mut self, keep: impl Fn(&str) -> bool) {
        self.schemas.retain(|k, _| keep(k));
        self.types.retain(|k, _| keep(&k.schema));
        self.sequences.retain(|k, _| keep(&k.schema));
        self.tables.retain(|k, _| keep(&k.schema));
        self.views.retain(|k, _| keep(&k.schema));
        self.functions.retain(|_, f| keep(&f.schema));
        // Extensions installed into an excluded schema are still relevant (e.g. Supabase `extensions`
        // schema) – they are kept, their schema is created by the extension statement if needed.
    }

    /// Strip all comments (for the "ignore comments" option).
    pub fn strip_comments(&mut self) {
        for s in self.schemas.values_mut() {
            s.comment = None;
        }
        for t in self.types.values_mut() {
            t.comment = None;
        }
        for t in self.tables.values_mut() {
            t.comment = None;
            for c in &mut t.columns {
                c.comment = None;
            }
        }
        for v in self.views.values_mut() {
            v.comment = None;
        }
        for f in self.functions.values_mut() {
            f.comment = None;
        }
    }

    pub fn object_count(&self) -> usize {
        self.schemas.len()
            + self.extensions.len()
            + self.types.len()
            + self.sequences.len()
            + self.tables.len()
            + self.views.len()
            + self.functions.len()
    }

    /// Stable fingerprint of the schema (used to detect drift between compare and deploy).
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        if let Ok(json) = serde_json::to_string(&FingerprintView(self)) {
            json.hash(&mut h);
        }
        h.finish()
    }
}

/// Serialises the catalog without informational fields for fingerprinting.
struct FingerprintView<'a>(&'a Catalog);

impl Serialize for FingerprintView<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTuple;
        let c = self.0;
        let tables: Vec<_> = c
            .tables
            .values()
            .map(|t| (&t.name, t.columns.iter().map(|c| (&c.name, &c.data_type, c.not_null, &c.default)).collect::<Vec<_>>(), &t.constraints, &t.indexes, &t.triggers, &t.policies, t.rls_enabled))
            .collect();
        let mut tup = s.serialize_tuple(7)?;
        tup.serialize_element(&c.schemas)?;
        tup.serialize_element(&c.extensions)?;
        tup.serialize_element(&c.types)?;
        tup.serialize_element(&c.sequences)?;
        tup.serialize_element(&tables)?;
        tup.serialize_element(&c.views.iter().map(|(k, v)| (k, &v.definition)).collect::<Vec<_>>())?;
        tup.serialize_element(&c.functions.iter().map(|(k, v)| (k, &v.definition)).collect::<Vec<_>>())?;
        tup.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_list() {
        assert_eq!(parse_column_list("PRIMARY KEY (id, \"Org Id\")"), vec!["id", "Org Id"]);
        assert_eq!(
            parse_column_list("FOREIGN KEY (a) REFERENCES public.t(b) ON DELETE CASCADE"),
            vec!["a"]
        );
    }

    #[test]
    fn split_idents() {
        assert_eq!(split_quoted_ident("public.\"a.b\""), vec!["public", "a.b"]);
        assert_eq!(split_quoted_ident("s.t.c"), vec!["s", "t", "c"]);
        assert_eq!(QName::parse_quoted("\"My\"\"S\".x"), Some(QName::new("My\"S", "x")));
    }
}
