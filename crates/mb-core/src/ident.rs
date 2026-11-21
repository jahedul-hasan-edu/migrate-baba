//! Identifier and literal quoting following PostgreSQL rules.

/// Reserved / type-function-name keywords that must be quoted when used as identifiers.
const RESERVED: &[&str] = &[
    "all", "analyse", "analyze", "and", "any", "array", "as", "asc", "asymmetric", "authorization",
    "between", "bigint", "binary", "bit", "boolean", "both", "case", "cast", "char", "character",
    "check", "coalesce", "collate", "collation", "column", "concurrently", "constraint", "create",
    "cross", "current_catalog", "current_date", "current_role", "current_schema", "current_time",
    "current_timestamp", "current_user", "dec", "decimal", "default", "deferrable", "desc",
    "distinct", "do", "else", "end", "except", "exists", "extract", "false", "fetch", "float", "for",
    "foreign", "freeze", "from", "full", "grant", "greatest", "group", "grouping", "having", "ilike",
    "in", "initially", "inner", "inout", "int", "integer", "intersect", "interval", "into", "is",
    "isnull", "join", "lateral", "leading", "least", "left", "like", "limit", "localtime",
    "localtimestamp", "national", "natural", "nchar", "none", "normalize", "not", "notnull", "null",
    "nullif", "numeric", "offset", "on", "only", "or", "order", "out", "outer", "overlaps",
    "overlay", "placing", "position", "precision", "primary", "real", "references", "returning",
    "right", "row", "select", "session_user", "setof", "similar", "smallint", "some", "substring",
    "symmetric", "system_user", "table", "tablesample", "then", "time", "timestamp", "to",
    "trailing", "treat", "trim", "true", "union", "unique", "user", "using", "values", "varchar",
    "variadic", "verbose", "when", "where", "window", "with", "xmlattributes", "xmlconcat",
    "xmlelement", "xmlexists", "xmlforest", "xmlnamespaces", "xmlparse", "xmlpi", "xmlroot",
    "xmlserialize", "xmltable",
];

/// Quote an identifier only when necessary (like PostgreSQL's `quote_ident`).
pub fn quote_ident(name: &str) -> String {
    let safe = !name.is_empty()
        && name
            .chars()
            .next()
            .map(|c| c.is_ascii_lowercase() || c == '_')
            .unwrap_or(false)
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$')
        && !RESERVED.contains(&name);
    if safe {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// `schema.name`, each part quoted as needed.
pub fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

/// Quote a string literal (standard_conforming_strings = on).
pub fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// `NULL` or a quoted literal.
pub fn quote_nullable(value: Option<&str>) -> String {
    match value {
        Some(v) => quote_literal(v),
        None => "NULL".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idents() {
        assert_eq!(quote_ident("users"), "users");
        assert_eq!(quote_ident("user"), "\"user\"");
        assert_eq!(quote_ident("Users"), "\"Users\"");
        assert_eq!(quote_ident("my col"), "\"my col\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_ident("_x1$"), "_x1$");
        assert_eq!(quote_ident("1abc"), "\"1abc\"");
        assert_eq!(qualified("public", "order"), "public.\"order\"");
    }

    #[test]
    fn literals() {
        assert_eq!(quote_literal("it's"), "'it''s'");
        assert_eq!(quote_nullable(None), "NULL");
    }
}
