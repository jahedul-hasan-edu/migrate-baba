//! Catalog introspection. All queries use the simple protocol (pooler friendly) and are issued
//! concurrently on a single connection so the driver pipelines them (≈ 1–2 round trips total).

use std::collections::BTreeMap;

use tokio_postgres::{Client, SimpleQueryRow};

use crate::conn::simple_rows;
use crate::error::{Error, Result};
use crate::model::*;

/// Which schemas to include.
#[derive(Clone, Debug, Default)]
pub struct SchemaFilter {
    /// If non-empty, only these schemas are included.
    pub include: Vec<String>,
    /// Always excluded (in addition to system schemas).
    pub exclude: Vec<String>,
}

impl SchemaFilter {
    pub fn allows(&self, schema: &str) -> bool {
        if schema == crate::data::copy::CHECKPOINT_SCHEMA || self.exclude.iter().any(|s| s == schema) {
            return false;
        }
        self.include.is_empty() || self.include.iter().any(|s| s == schema)
    }
}

fn sys_filter(alias: &str) -> String {
    format!(
        "{a}.nspname NOT IN ('pg_catalog','information_schema','pg_toast') \
         AND {a}.nspname NOT LIKE 'pg\\_temp\\_%' AND {a}.nspname NOT LIKE 'pg\\_toast\\_temp\\_%'",
        a = alias
    )
}

fn not_ext(class: &str, oid_expr: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM pg_depend dx WHERE dx.classid = '{class}'::regclass \
         AND dx.objid = {oid_expr} AND dx.deptype = 'e')"
    )
}

// ---- row helpers -----------------------------------------------------------

fn s(r: &SimpleQueryRow, col: &str) -> String {
    r.get(col).unwrap_or_default().to_string()
}
fn o(r: &SimpleQueryRow, col: &str) -> Option<String> {
    r.get(col).map(str::to_string)
}
fn b(r: &SimpleQueryRow, col: &str) -> bool {
    r.get(col) == Some("t")
}
fn i(r: &SimpleQueryRow, col: &str) -> i64 {
    r.get(col).and_then(|v| v.parse::<f64>().ok()).map(|v| v as i64).unwrap_or(0)
}
fn q(r: &SimpleQueryRow, schema: &str, name: &str) -> QName {
    QName::new(s(r, schema), s(r, name))
}
fn json<T: serde::de::DeserializeOwned + Default>(r: &SimpleQueryRow, col: &str) -> T {
    r.get(col).and_then(|v| serde_json::from_str(v).ok()).unwrap_or_default()
}

/// Read the whole catalog from a connection created with `SessionOptions::introspection`.
pub async fn introspect(client: &Client, filter: &SchemaFilter) -> Result<Catalog> {
    let ver_rows = simple_rows(client, "SELECT current_setting('server_version_num') AS v").await?;
    let version: i32 = ver_rows
        .first()
        .and_then(|r| r.get("v"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if version < 120000 {
        return Err(Error::Unsupported(format!(
            "PostgreSQL {} is not supported (12 or newer required)",
            version / 10000
        )));
    }

    let q_schemas = format!(
        "SELECT n.nspname AS name, obj_description(n.oid, 'pg_namespace') AS comment \
         FROM pg_namespace n WHERE {} AND {}",
        sys_filter("n"),
        not_ext("pg_namespace", "n.oid")
    );
    let q_ext = "SELECT e.extname AS name, n.nspname AS schema, e.extversion AS version \
                 FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace \
                 WHERE e.extname <> 'plpgsql'"
        .to_string();
    let q_enums = format!(
        "SELECT n.nspname AS schema, t.typname AS name, \
           json_agg(e.enumlabel ORDER BY e.enumsortorder)::text AS labels, \
           obj_description(t.oid, 'pg_type') AS comment \
         FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
         JOIN pg_enum e ON e.enumtypid = t.oid \
         WHERE {} AND {} GROUP BY n.nspname, t.typname, t.oid",
        sys_filter("n"),
        not_ext("pg_type", "t.oid")
    );
    let q_domains = format!(
        "SELECT n.nspname AS schema, t.typname AS name, format_type(t.typbasetype, t.typtypmod) AS base, \
           t.typnotnull AS not_null, t.typdefault AS def, obj_description(t.oid, 'pg_type') AS comment, \
           (SELECT coalesce(json_agg(json_build_array(c.conname, pg_get_constraintdef(c.oid)) ORDER BY c.conname), '[]') \
              FROM pg_constraint c WHERE c.contypid = t.oid)::text AS checks \
         FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
         WHERE t.typtype = 'd' AND {} AND {}",
        sys_filter("n"),
        not_ext("pg_type", "t.oid")
    );
    let q_composites = format!(
        "SELECT n.nspname AS schema, t.typname AS name, obj_description(t.oid, 'pg_type') AS comment, \
           (SELECT coalesce(json_agg(json_build_array(a.attname, format_type(a.atttypid, a.atttypmod)) ORDER BY a.attnum), '[]') \
              FROM pg_attribute a WHERE a.attrelid = t.typrelid AND a.attnum > 0 AND NOT a.attisdropped)::text AS attrs \
         FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace JOIN pg_class c ON c.oid = t.typrelid \
         WHERE t.typtype = 'c' AND c.relkind = 'c' AND {} AND {}",
        sys_filter("n"),
        not_ext("pg_type", "t.oid")
    );
    let q_sequences = format!(
        "SELECT n.nspname AS schema, c.relname AS name, format_type(s.seqtypid, NULL) AS data_type, \
           s.seqstart AS start, s.seqincrement AS inc, s.seqmin AS min, s.seqmax AS max, s.seqcache AS cache, \
           s.seqcycle AS cycle, \
           (SELECT quote_ident(tn.nspname) || '.' || quote_ident(tc.relname) || '.' || quote_ident(a.attname) \
              FROM pg_depend d JOIN pg_class tc ON tc.oid = d.refobjid \
              JOIN pg_namespace tn ON tn.oid = tc.relnamespace \
              JOIN pg_attribute a ON a.attrelid = tc.oid AND a.attnum = d.refobjsubid \
              WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND d.deptype = 'a' LIMIT 1) AS owned_by \
         FROM pg_sequence s JOIN pg_class c ON c.oid = s.seqrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE {} AND {} AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass \
           AND d.objid = c.oid AND d.deptype = 'i')",
        sys_filter("n"),
        not_ext("pg_class", "c.oid")
    );
    let q_tables = format!(
        "SELECT n.nspname AS schema, c.relname AS name, c.relkind AS kind, c.relpersistence AS persistence, \
           c.relrowsecurity AS rls, c.relforcerowsecurity AS rls_forced, obj_description(c.oid, 'pg_class') AS comment, \
           CASE WHEN c.relispartition THEN (SELECT quote_ident(pn.nspname) || '.' || quote_ident(pc.relname) \
              FROM pg_inherits ih JOIN pg_class pc ON pc.oid = ih.inhparent JOIN pg_namespace pn ON pn.oid = pc.relnamespace \
              WHERE ih.inhrelid = c.oid LIMIT 1) END AS partition_of, \
           CASE WHEN c.relispartition THEN pg_get_expr(c.relpartbound, c.oid) END AS partition_bound, \
           CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) END AS partition_key, \
           greatest(c.reltuples, 0)::bigint AS est_rows, c.relpages::bigint AS pages, \
           pg_total_relation_size(c.oid) AS total_bytes \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'p') AND {} AND {}",
        sys_filter("n"),
        not_ext("pg_class", "c.oid")
    );
    let q_columns = format!(
        "SELECT n.nspname AS schema, c.relname AS rel, a.attname AS name, \
           format_type(a.atttypid, a.atttypmod) AS data_type, a.atttypid AS type_oid, a.attnotnull AS not_null, \
           CASE WHEN a.attgenerated = '' THEN pg_get_expr(ad.adbin, ad.adrelid) END AS def, \
           CASE WHEN a.attgenerated <> '' THEN pg_get_expr(ad.adbin, ad.adrelid) END AS generated, \
           a.attidentity AS identity, \
           CASE WHEN a.attcollation <> 0 AND a.attcollation <> t.typcollation \
                THEN quote_ident(cn.nspname) || '.' || quote_ident(co.collname) END AS collation, \
           col_description(c.oid, a.attnum) AS comment \
         FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_type t ON t.oid = a.atttypid \
         LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
         LEFT JOIN pg_collation co ON co.oid = a.attcollation LEFT JOIN pg_namespace cn ON cn.oid = co.collnamespace \
         WHERE c.relkind IN ('r', 'p') AND a.attnum > 0 AND NOT a.attisdropped AND {} \
         ORDER BY n.nspname, c.relname, a.attnum",
        sys_filter("n")
    );
    let q_constraints = format!(
        "SELECT n.nspname AS schema, c.relname AS rel, con.conname AS name, con.contype AS kind, \
           pg_get_constraintdef(con.oid) AS def, \
           CASE WHEN con.contype = 'f' THEN (SELECT quote_ident(fn.nspname) || '.' || quote_ident(fc.relname) \
              FROM pg_class fc JOIN pg_namespace fn ON fn.oid = fc.relnamespace WHERE fc.oid = con.confrelid) END AS ref \
         FROM pg_constraint con JOIN pg_class c ON c.oid = con.conrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE con.contype IN ('p', 'u', 'c', 'x', 'f') AND c.relkind IN ('r', 'p') \
           AND con.conparentid = 0 AND con.conislocal AND {}",
        sys_filter("n")
    );
    let q_indexes = format!(
        "SELECT n.nspname AS schema, c.relname AS rel, ic.relname AS name, pg_get_indexdef(i.indexrelid) AS def \
         FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid JOIN pg_class c ON c.oid = i.indrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'p', 'm') AND {} \
           AND NOT EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conindid = i.indexrelid AND k.contype IN ('p', 'u', 'x')) \
           AND NOT EXISTS (SELECT 1 FROM pg_inherits ih WHERE ih.inhrelid = i.indexrelid)",
        sys_filter("n")
    );
    let q_triggers = format!(
        "SELECT n.nspname AS schema, c.relname AS rel, t.tgname AS name, pg_get_triggerdef(t.oid) AS def, \
           t.tgenabled AS enabled \
         FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE NOT t.tgisinternal AND c.relkind IN ('r', 'p') AND {} {}",
        sys_filter("n"),
        if version >= 130000 { "AND t.tgparentid = 0" } else { "" }
    );
    let q_policies = format!(
        "SELECT n.nspname AS schema, c.relname AS rel, p.polname AS name, p.polpermissive AS permissive, \
           CASE p.polcmd WHEN 'r' THEN 'SELECT' WHEN 'a' THEN 'INSERT' WHEN 'w' THEN 'UPDATE' \
                WHEN 'd' THEN 'DELETE' ELSE 'ALL' END AS cmd, \
           (SELECT coalesce(json_agg(rn ORDER BY rn), '[]') FROM (SELECT CASE WHEN r = 0 THEN 'public' \
              ELSE quote_ident(pg_get_userbyid(r)) END AS rn FROM unnest(p.polroles) r) x)::text AS roles, \
           pg_get_expr(p.polqual, p.polrelid) AS using_expr, pg_get_expr(p.polwithcheck, p.polrelid) AS check_expr \
         FROM pg_policy p JOIN pg_class c ON c.oid = p.polrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE {}",
        sys_filter("n")
    );
    let q_views = format!(
        "SELECT n.nspname AS schema, c.relname AS name, c.relkind AS kind, pg_get_viewdef(c.oid, true) AS def, \
           array_to_string(c.reloptions, ', ') AS options, obj_description(c.oid, 'pg_class') AS comment \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('v', 'm') AND {} AND {}",
        sys_filter("n"),
        not_ext("pg_class", "c.oid")
    );
    let q_view_deps = "SELECT DISTINCT vn.nspname AS vs, v.relname AS vname, rn.nspname AS rs, r.relname AS rname \
         FROM pg_depend d JOIN pg_rewrite rw ON rw.oid = d.objid \
         JOIN pg_class v ON v.oid = rw.ev_class JOIN pg_namespace vn ON vn.oid = v.relnamespace \
         JOIN pg_class r ON r.oid = d.refobjid JOIN pg_namespace rn ON rn.oid = r.relnamespace \
         WHERE d.classid = 'pg_rewrite'::regclass AND d.refclassid = 'pg_class'::regclass \
           AND v.oid <> r.oid AND v.relkind IN ('v', 'm')"
        .to_string();
    let q_functions = format!(
        "SELECT n.nspname AS schema, p.proname AS name, pg_get_function_identity_arguments(p.oid) AS args, \
           p.prokind AS kind, pg_get_function_result(p.oid) AS result, l.lanname AS lang, \
           pg_get_functiondef(p.oid) AS def, obj_description(p.oid, 'pg_proc') AS comment \
         FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace JOIN pg_language l ON l.oid = p.prolang \
         WHERE p.prokind IN ('f', 'p') AND l.lanname NOT IN ('c', 'internal') AND {} AND {}",
        sys_filter("n"),
        not_ext("pg_proc", "p.oid")
    );

    let (schemas, exts, enums, domains, composites, seqs, tables, columns, cons, idx, trg, pol, views, vdeps, funcs) =
        tokio::try_join!(
            simple_rows(client, &q_schemas),
            simple_rows(client, &q_ext),
            simple_rows(client, &q_enums),
            simple_rows(client, &q_domains),
            simple_rows(client, &q_composites),
            simple_rows(client, &q_sequences),
            simple_rows(client, &q_tables),
            simple_rows(client, &q_columns),
            simple_rows(client, &q_constraints),
            simple_rows(client, &q_indexes),
            simple_rows(client, &q_triggers),
            simple_rows(client, &q_policies),
            simple_rows(client, &q_views),
            simple_rows(client, &q_view_deps),
            simple_rows(client, &q_functions),
        )?;

    let mut cat = Catalog { server_version: version, ..Default::default() };

    for r in &schemas {
        let name = s(r, "name");
        cat.schemas.insert(name.clone(), SchemaDef { name, comment: o(r, "comment") });
    }
    for r in &exts {
        let name = s(r, "name");
        cat.extensions
            .insert(name.clone(), ExtensionDef { name, schema: s(r, "schema"), version: s(r, "version") });
    }
    for r in &enums {
        let name = q(r, "schema", "name");
        cat.types.insert(
            name.clone(),
            TypeDef { name, kind: TypeKind::Enum { labels: json(r, "labels") }, comment: o(r, "comment") },
        );
    }
    for r in &domains {
        let name = q(r, "schema", "name");
        let checks: Vec<(String, String)> = json(r, "checks");
        cat.types.insert(
            name.clone(),
            TypeDef {
                name,
                kind: TypeKind::Domain {
                    base: s(r, "base"),
                    not_null: b(r, "not_null"),
                    default: o(r, "def"),
                    checks,
                },
                comment: o(r, "comment"),
            },
        );
    }
    for r in &composites {
        let name = q(r, "schema", "name");
        cat.types.insert(
            name.clone(),
            TypeDef { name, kind: TypeKind::Composite { attrs: json(r, "attrs") }, comment: o(r, "comment") },
        );
    }
    for r in &seqs {
        let name = q(r, "schema", "name");
        cat.sequences.insert(
            name.clone(),
            SequenceDef {
                name,
                data_type: s(r, "data_type"),
                start: i(r, "start"),
                increment: i(r, "inc"),
                min: i(r, "min"),
                max: i(r, "max"),
                cache: i(r, "cache"),
                cycle: b(r, "cycle"),
                owned_by: o(r, "owned_by"),
            },
        );
    }
    for r in &tables {
        let name = q(r, "schema", "name");
        cat.tables.insert(
            name.clone(),
            TableDef {
                name,
                rls_enabled: b(r, "rls"),
                rls_forced: b(r, "rls_forced"),
                unlogged: s(r, "persistence") == "u",
                partition_key: o(r, "partition_key"),
                partition_of: o(r, "partition_of").and_then(|p| QName::parse_quoted(&p)),
                partition_bound: o(r, "partition_bound"),
                comment: o(r, "comment"),
                est_rows: i(r, "est_rows"),
                total_bytes: i(r, "total_bytes"),
                pages: i(r, "pages"),
                ..Default::default()
            },
        );
    }
    for r in &columns {
        let Some(t) = cat.tables.get_mut(&q(r, "schema", "rel")) else { continue };
        t.columns.push(Column {
            name: s(r, "name"),
            data_type: s(r, "data_type"),
            type_oid: i(r, "type_oid") as u32,
            not_null: b(r, "not_null"),
            default: o(r, "def"),
            identity: match r.get("identity") {
                Some("a") => Some(Identity::Always),
                Some("d") => Some(Identity::ByDefault),
                _ => None,
            },
            generated: o(r, "generated"),
            collation: o(r, "collation"),
            comment: o(r, "comment"),
        });
    }
    for r in &cons {
        let Some(t) = cat.tables.get_mut(&q(r, "schema", "rel")) else { continue };
        let Some(kind) = ConstraintKind::from_code(&s(r, "kind")) else { continue };
        let name = s(r, "name");
        t.constraints.insert(
            name.clone(),
            Constraint {
                name,
                kind,
                definition: s(r, "def"),
                ref_table: o(r, "ref").and_then(|x| QName::parse_quoted(&x)),
            },
        );
    }
    // Views first so matview indexes can be attached.
    let mut deps: BTreeMap<QName, Vec<QName>> = BTreeMap::new();
    for r in &vdeps {
        deps.entry(q(r, "vs", "vname")).or_default().push(q(r, "rs", "rname"));
    }
    for r in &views {
        let name = q(r, "schema", "name");
        let depends_on = deps.remove(&name).unwrap_or_default();
        cat.views.insert(
            name.clone(),
            ViewDef {
                name,
                materialized: s(r, "kind") == "m",
                definition: s(r, "def"),
                options: o(r, "options").filter(|s| !s.is_empty()),
                depends_on,
                comment: o(r, "comment"),
                indexes: BTreeMap::new(),
            },
        );
    }
    for r in &idx {
        let key = q(r, "schema", "rel");
        let index = Index { name: s(r, "name"), definition: s(r, "def") };
        if let Some(t) = cat.tables.get_mut(&key) {
            t.indexes.insert(index.name.clone(), index);
        } else if let Some(v) = cat.views.get_mut(&key) {
            v.indexes.insert(index.name.clone(), index);
        }
    }
    for r in &trg {
        let Some(t) = cat.tables.get_mut(&q(r, "schema", "rel")) else { continue };
        let name = s(r, "name");
        t.triggers.insert(name.clone(), Trigger { name, definition: s(r, "def"), enabled: s(r, "enabled") });
    }
    for r in &pol {
        let Some(t) = cat.tables.get_mut(&q(r, "schema", "rel")) else { continue };
        let name = s(r, "name");
        t.policies.insert(
            name.clone(),
            Policy {
                name,
                permissive: b(r, "permissive"),
                roles: json(r, "roles"),
                cmd: s(r, "cmd"),
                using: o(r, "using_expr"),
                with_check: o(r, "check_expr"),
            },
        );
    }
    for r in &funcs {
        let f = FunctionDef {
            schema: s(r, "schema"),
            name: s(r, "name"),
            identity_args: s(r, "args"),
            kind: if s(r, "kind") == "p" { FunctionKind::Procedure } else { FunctionKind::Function },
            result: o(r, "result"),
            language: s(r, "lang"),
            definition: s(r, "def"),
            comment: o(r, "comment"),
        };
        cat.functions.insert(f.key(), f);
    }

    cat.retain_schemas(|sch| filter.allows(sch));
    Ok(cat)
}
