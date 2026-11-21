use std::collections::{BTreeMap, BTreeSet};

use mb_core::diff::{compare, DiffStatus, ObjectKey, ObjectKind};
use mb_core::model::*;
use mb_core::plan::{plan, plan_selection, Hazard, Phase, PlanOptions, RunMode};

fn col(name: &str, ty: &str) -> Column {
    Column { name: name.into(), data_type: ty.into(), type_oid: 23, ..Default::default() }
}

fn table(schema: &str, name: &str, cols: Vec<Column>) -> TableDef {
    let mut t = TableDef { name: QName::new(schema, name), columns: cols, ..Default::default() };
    t.constraints.insert(
        format!("{name}_pkey"),
        Constraint {
            name: format!("{name}_pkey"),
            kind: ConstraintKind::PrimaryKey,
            definition: "PRIMARY KEY (id)".into(),
            ref_table: None,
        },
    );
    t
}

fn base() -> Catalog {
    let mut c = Catalog { server_version: 150000, ..Default::default() };
    c.schemas.insert("public".into(), SchemaDef { name: "public".into(), comment: None });
    let mut id = col("id", "bigint");
    id.not_null = true;
    let users = table("public", "users", vec![id, col("email", "text")]);
    c.tables.insert(users.name.clone(), users);
    c
}

fn sqls(p: &mb_core::plan::Plan) -> Vec<String> {
    p.steps.iter().map(|s| s.sql.clone()).collect()
}

fn pos(p: &mb_core::plan::Plan, needle: &str) -> usize {
    p.steps.iter().position(|s| s.sql.contains(needle)).unwrap_or_else(|| panic!("missing step: {needle}\n{:#?}", sqls(p)))
}

#[test]
fn identical_catalogs_produce_empty_plan() {
    let c = base();
    assert!(plan(&c, &c, &PlanOptions::default()).is_empty());
    assert!(compare(&c, &c).iter().all(|d| d.status == DiffStatus::Identical));
}

#[test]
fn new_table_with_fk_index_policy() {
    let cur = base();
    let mut des = base();
    let mut id = col("id", "bigint");
    id.not_null = true;
    id.identity = Some(Identity::Always);
    let mut posts = table("public", "posts", vec![id, col("user_id", "bigint"), col("title", "text")]);
    posts.constraints.insert(
        "posts_user_fk".into(),
        Constraint {
            name: "posts_user_fk".into(),
            kind: ConstraintKind::ForeignKey,
            definition: "FOREIGN KEY (user_id) REFERENCES public.users(id)".into(),
            ref_table: Some(QName::new("public", "users")),
        },
    );
    posts.indexes.insert(
        "posts_user_idx".into(),
        Index { name: "posts_user_idx".into(), definition: "CREATE INDEX posts_user_idx ON public.posts USING btree (user_id)".into() },
    );
    posts.rls_enabled = true;
    posts.policies.insert(
        "read own".into(),
        Policy {
            name: "read own".into(),
            permissive: true,
            roles: vec!["authenticated".into()],
            cmd: "SELECT".into(),
            using: Some("(auth.uid() = user_id)".into()),
            with_check: None,
        },
    );
    des.tables.insert(posts.name.clone(), posts);

    let p = plan(&cur, &des, &PlanOptions::default());
    let create = pos(&p, "CREATE TABLE public.posts");
    let fk = pos(&p, "ADD CONSTRAINT posts_user_fk");
    let idx = pos(&p, "CREATE INDEX posts_user_idx");
    let rls = pos(&p, "ENABLE ROW LEVEL SECURITY");
    let pol = pos(&p, "CREATE POLICY \"read own\" ON public.posts AS PERMISSIVE FOR SELECT TO authenticated USING ((auth.uid() = user_id))");
    assert!(create < idx && idx < fk && fk < rls && rls < pol);
    assert!(p.steps[create].sql.contains("id bigint GENERATED ALWAYS AS IDENTITY NOT NULL"));
    assert!(p.steps[create].sql.contains("CONSTRAINT posts_pkey PRIMARY KEY (id)"));
    assert!(!p.has_data_loss());
}

#[test]
fn column_type_change_recreates_dependent_views() {
    let mut cur = base();
    let view = ViewDef {
        name: QName::new("public", "user_emails"),
        definition: " SELECT users.id, users.email FROM public.users;".into(),
        depends_on: vec![QName::new("public", "users")],
        ..Default::default()
    };
    let view2 = ViewDef {
        name: QName::new("public", "user_emails2"),
        definition: " SELECT id FROM public.user_emails;".into(),
        depends_on: vec![QName::new("public", "user_emails")],
        ..Default::default()
    };
    cur.views.insert(view.name.clone(), view.clone());
    cur.views.insert(view2.name.clone(), view2.clone());
    let mut des = cur.clone();
    des.tables.get_mut(&QName::new("public", "users")).unwrap().columns[1].data_type = "character varying(320)".into();

    let p = plan(&cur, &des, &PlanOptions::default());
    let drop2 = pos(&p, "DROP VIEW IF EXISTS public.user_emails2");
    let drop1 = pos(&p, "DROP VIEW IF EXISTS public.user_emails;");
    let alter = pos(&p, "ALTER COLUMN email TYPE character varying(320) USING email::character varying(320)");
    let create1 = pos(&p, "CREATE VIEW public.user_emails AS");
    let create2 = pos(&p, "CREATE VIEW public.user_emails2 AS");
    assert!(drop2 < drop1 && drop1 < alter && alter < create1 && create1 < create2);
    assert!(p.steps[alter].hazards.contains(&Hazard::TableRewrite));
}

#[test]
fn enum_value_added_in_order() {
    let mut cur = base();
    let t = TypeDef {
        name: QName::new("public", "status"),
        kind: TypeKind::Enum { labels: vec!["new".into(), "done".into()] },
        comment: None,
    };
    cur.types.insert(t.name.clone(), t.clone());
    let mut des = cur.clone();
    des.types.get_mut(&t.name).unwrap().kind =
        TypeKind::Enum { labels: vec!["new".into(), "active".into(), "done".into()] };
    let p = plan(&cur, &des, &PlanOptions::default());
    assert_eq!(p.steps.len(), 1);
    assert_eq!(p.steps[0].sql, "ALTER TYPE public.status ADD VALUE IF NOT EXISTS 'active' AFTER 'new';");
    assert_eq!(p.steps[0].run, RunMode::BeforeTransaction);

    // Removing a value is manual.
    des.types.get_mut(&t.name).unwrap().kind = TypeKind::Enum { labels: vec!["new".into()] };
    let p = plan(&cur, &des, &PlanOptions::default());
    assert!(p.steps[0].is_manual());
}

#[test]
fn dropped_table_and_column_flag_data_loss() {
    let mut cur = base();
    let old = table("public", "old_logs", vec![col("id", "integer")]);
    cur.tables.insert(old.name.clone(), old);
    let mut des = base();
    des.tables.get_mut(&QName::new("public", "users")).unwrap().columns.pop();
    let p = plan(&cur, &des, &PlanOptions::default());
    let d = pos(&p, "DROP TABLE IF EXISTS public.old_logs;");
    assert_eq!(p.steps[d].phase, Phase::Drops);
    assert!(p.steps[d].hazards.contains(&Hazard::DataLoss));
    let c = pos(&p, "DROP COLUMN email");
    assert!(p.steps[c].hazards.contains(&Hazard::DataLoss));
}

#[test]
fn serial_to_identity_order() {
    let mut cur = base();
    let mut c = col("n", "integer");
    c.default = Some("nextval('public.users_n_seq'::regclass)".into());
    cur.tables.get_mut(&QName::new("public", "users")).unwrap().columns.push(c);
    let mut des = cur.clone();
    {
        let n = des.tables.get_mut(&QName::new("public", "users")).unwrap().columns.last_mut().unwrap();
        n.default = None;
        n.identity = Some(Identity::ByDefault);
        n.not_null = true;
    }
    let p = plan(&cur, &des, &PlanOptions::default());
    let a = pos(&p, "ALTER COLUMN n DROP DEFAULT");
    let b = pos(&p, "ALTER COLUMN n SET NOT NULL");
    let c = pos(&p, "ALTER COLUMN n ADD GENERATED BY DEFAULT AS IDENTITY");
    assert!(a < b && b < c);
}

#[test]
fn function_return_change_drops_and_recreates() {
    let mut cur = base();
    let f = FunctionDef {
        schema: "public".into(),
        name: "get_count".into(),
        identity_args: "".into(),
        kind: FunctionKind::Function,
        result: Some("integer".into()),
        language: "sql".into(),
        definition: "CREATE OR REPLACE FUNCTION public.get_count()\n RETURNS integer\n LANGUAGE sql\nAS $function$ select 1 $function$\n".into(),
        comment: None,
    };
    cur.functions.insert(f.key(), f.clone());
    let mut des = cur.clone();
    let g = des.functions.get_mut(&f.key()).unwrap();
    g.result = Some("bigint".into());
    g.definition = g.definition.replace("RETURNS integer", "RETURNS bigint");
    let p = plan(&cur, &des, &PlanOptions::default());
    let d = pos(&p, "DROP FUNCTION IF EXISTS public.get_count();");
    let c = pos(&p, "RETURNS bigint");
    assert!(d < c);

    // Body-only change → CREATE OR REPLACE only.
    let mut des2 = cur.clone();
    des2.functions.get_mut(&f.key()).unwrap().definition = f.definition.replace("select 1", "select 2");
    let p = plan(&cur, &des2, &PlanOptions::default());
    assert_eq!(p.steps.len(), 1);
    assert!(p.steps[0].sql.starts_with("CREATE OR REPLACE FUNCTION"));
}

#[test]
fn selection_limits_plan() {
    let target = base();
    let mut source = base();
    let a = table("public", "a", vec![col("id", "integer")]);
    let b = table("public", "b", vec![col("id", "integer")]);
    source.tables.insert(a.name.clone(), a);
    source.tables.insert(b.name.clone(), b);
    source.tables.get_mut(&QName::new("public", "users")).unwrap().comment = Some("people".into());

    let diffs = compare(&source, &target);
    assert_eq!(diffs.iter().filter(|d| d.status == DiffStatus::Added).count(), 2);
    assert_eq!(diffs.iter().filter(|d| d.status == DiffStatus::Changed).count(), 1);

    let sel: BTreeSet<ObjectKey> = [ObjectKey::new(ObjectKind::Table, "public", "a")].into_iter().collect();
    let p = plan_selection(&source, &target, &sel, &PlanOptions::default());
    assert_eq!(p.steps.len(), 1);
    assert!(p.steps[0].sql.starts_with("CREATE TABLE public.a"));
}

#[test]
fn pk_change_recreates_referencing_fks() {
    let mut cur = base();
    let mut orders = table("public", "orders", vec![col("id", "integer"), col("user_id", "bigint")]);
    orders.constraints.insert(
        "orders_user_fk".into(),
        Constraint {
            name: "orders_user_fk".into(),
            kind: ConstraintKind::ForeignKey,
            definition: "FOREIGN KEY (user_id) REFERENCES public.users(id)".into(),
            ref_table: Some(QName::new("public", "users")),
        },
    );
    cur.tables.insert(orders.name.clone(), orders);
    let mut des = cur.clone();
    des.tables.get_mut(&QName::new("public", "users")).unwrap().constraints.get_mut("users_pkey").unwrap().definition =
        "PRIMARY KEY (id) INCLUDE (email)".into();
    let p = plan(&cur, &des, &PlanOptions::default());
    let drop_fk = pos(&p, "DROP CONSTRAINT orders_user_fk");
    let drop_pk = pos(&p, "DROP CONSTRAINT users_pkey");
    let add_pk = pos(&p, "ADD CONSTRAINT users_pkey");
    let add_fk = pos(&p, "ADD CONSTRAINT orders_user_fk");
    assert!(drop_fk < drop_pk && drop_pk < add_pk && add_pk < add_fk);
}

#[test]
fn script_rendering_has_header_and_hazards() {
    let mut cur = base();
    let old = table("public", "gone", vec![col("id", "integer")]);
    cur.tables.insert(old.name.clone(), old);
    let p = plan(&cur, &base(), &PlanOptions { cascade_drops: true, ..Default::default() });
    let sql = p.to_sql("test");
    assert!(sql.contains("SET check_function_bodies = off;"));
    assert!(sql.contains("-- [DATA LOSS] table public.gone"));
    assert!(sql.contains("DROP TABLE IF EXISTS public.gone CASCADE;"));
    let _unused: BTreeMap<u8, u8> = BTreeMap::new();
}
