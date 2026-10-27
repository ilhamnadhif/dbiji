use super::*;
use crate::view::{ClientFilter, SortKey};
use dbiji_core::ddl::DdlGenerator;
use dbiji_core::dialect::{GenericDialect, SqlSyntax};
use dbiji_core::{ColumnSource, DbKind};

/// PostgreSQL-like test dialect: `$n` placeholders.
struct PgLike(GenericDialect);

impl Dialect for PgLike {
    fn kind(&self) -> DbKind {
        DbKind::Postgres
    }
    fn syntax(&self) -> &SqlSyntax {
        self.0.syntax()
    }
    fn ddl(&self) -> &dyn DdlGenerator {
        self.0.ddl()
    }
    fn placeholder(&self, index: usize) -> String {
        format!("${index}")
    }
}

struct MyLike(GenericDialect);

impl Dialect for MyLike {
    fn kind(&self) -> DbKind {
        DbKind::MySql
    }
    fn syntax(&self) -> &SqlSyntax {
        self.0.syntax()
    }
    fn ddl(&self) -> &dyn DdlGenerator {
        self.0.ddl()
    }
}

fn src(col: &str) -> ColumnSource {
    ColumnSource { database: None, schema: Some("app".into()), table: "people".into(), column: col.into() }
}

fn columns() -> Vec<ColumnMeta> {
    let mut id = ColumnMeta::new("id", "int4", DataKind::Int).with_source(src("id"));
    id.auto_increment = Some(true);
    id.nullable = Some(false);
    let mut name = ColumnMeta::new("name", "varchar(10)", DataKind::Text).with_source(src("name"));
    name.max_length = Some(10);
    name.nullable = Some(false);
    vec![
        id,
        name,
        ColumnMeta::new("age", "int4", DataKind::Int).with_source(src("age")),
        ColumnMeta::new("score", "numeric", DataKind::Decimal).with_source(src("score")),
        ColumnMeta::new("label", "text", DataKind::Text),
    ]
}

fn rows(n: usize) -> Vec<Row> {
    (0..n)
        .map(|i| {
            vec![
                Value::Int(i as i64 + 1),
                Value::text(format!("p{i}")),
                if i % 3 == 0 { Value::Null } else { Value::Int(20 + i as i64) },
                Value::decimal(format!("{}.5", i)),
                Value::text("x"),
            ]
        })
        .collect()
}

fn model(n: usize) -> ResultModel {
    let mut m = ResultModel::from_rows(columns(), rows(n));
    let target = determine_editability(m.columns(), |_| Some(vec!["id".into()]), false);
    assert!(target.is_ok());
    m.set_editability(target);
    m
}

#[test]
fn read_only_model_rejects_edits() {
    let mut m = ResultModel::from_rows(columns(), rows(3));
    assert_eq!(m.row_count(), 3);
    assert!(!m.is_editable());
    assert!(m.set_cell(0, 1, "x").is_err());
    assert!(m.add_row(None).is_err());
    assert!(m.delete_rows(&[0]).is_err());
    assert!(m.change_set().is_none());
    assert!(m.change_statements(&GenericDialect::new()).is_empty());
}

#[test]
fn set_cell_overlay_validation_and_undo() {
    let mut m = model(5);
    assert_eq!(m.cell(1, 2), Some(Value::Int(21)));
    m.set_cell(1, 2, "42").unwrap();
    assert_eq!(m.cell(1, 2), Some(Value::Int(42)));
    assert!(m.is_cell_modified(1, 2));
    assert_eq!(m.row_state(1), RowState::Modified);
    // Store still has the original value.
    assert_eq!(m.store().lock().cell(1, 2), Some(&Value::Int(21)));
    // Type validation.
    let err = m.set_cell(1, 2, "abc").unwrap_err();
    assert!(err.contains("age"), "{err}");
    // NOT NULL and length validation.
    assert!(m.set_cell(1, 1, "").is_ok(), "empty text is allowed for text columns");
    assert!(m.set_null(1, 1).unwrap_err().contains("NULL"));
    assert!(m.set_cell(1, 1, "abcdefghijk").unwrap_err().contains("10"));
    // Computed column is read-only.
    assert!(m.set_cell(1, 4, "y").unwrap_err().contains("label"));
    // Setting the original value removes the overlay.
    m.set_cell(1, 1, "p1").unwrap();
    m.set_cell(1, 2, "21").unwrap();
    assert!(!m.is_cell_modified(1, 2));
    assert!(!m.has_changes());
    // Undo restores previous overlay states step by step.
    assert!(m.undo()); // 21 -> 42 again
    assert_eq!(m.cell(1, 2), Some(Value::Int(42)));
    assert!(m.undo()); // name p1 -> ""
    assert_eq!(m.cell(1, 1), Some(Value::text("")));
    assert!(m.undo()); // name "" -> original
    assert!(m.undo()); // age 42 -> original
    assert_eq!(m.cell(1, 2), Some(Value::Int(21)));
    assert!(!m.has_changes());
    assert!(!m.undo());
}

#[test]
fn insert_positions_and_mapping() {
    let mut m = model(4);
    let r = m.add_row(None).unwrap();
    assert_eq!(r, 4);
    let r = m.add_row(Some(0)).unwrap();
    assert_eq!(r, 1);
    let r2 = m.add_row(Some(1)).unwrap();
    assert_eq!(r2, 2, "after an inserted row");
    let r3 = m.add_row(Some(0)).unwrap();
    assert_eq!(r3, 1, "directly after the stored row, before earlier inserts");
    assert_eq!(m.row_count(), 8);
    let states: Vec<RowState> = (0..8).map(|d| m.row_state(d)).collect();
    use RowState::*;
    assert_eq!(states, vec![Unchanged, Inserted, Inserted, Inserted, Unchanged, Unchanged, Unchanged, Inserted]);
    let stored: Vec<Option<RowSource>> = (0..8).map(|d| m.row_source(d)).collect();
    assert_eq!(stored[0], Some(RowSource::Stored(0)));
    assert_eq!(stored[4], Some(RowSource::Stored(1)));
    assert_eq!(stored[6], Some(RowSource::Stored(3)));
    assert!(m.row_source(8).is_none());
    // display_row_of round trips.
    for d in 0..8 {
        let key = m.row_key(d).unwrap();
        assert_eq!(m.display_row_of(key), Some(d), "row {d}");
    }
    // New rows read as defaults (NULL).
    assert_eq!(m.cell(1, 1), Some(Value::Null));
    m.set_cell(1, 1, "new").unwrap();
    assert_eq!(m.cell(1, 1), Some(Value::text("new")));
    assert!(m.is_cell_modified(1, 1));
    // Undo the cell edit, then the four inserts.
    for _ in 0..5 {
        assert!(m.undo());
    }
    assert_eq!(m.row_count(), 4);
}

#[test]
fn duplicate_and_delete() {
    let mut m = model(4);
    let dup = m.duplicate_rows(&[2, 1]).unwrap();
    assert_eq!(dup, vec![3, 4]);
    assert_eq!(m.cell(3, 1), Some(Value::text("p1")));
    assert_eq!(m.cell(4, 1), Some(Value::text("p2")));
    // Auto increment key is left at its default.
    assert_eq!(m.cell(3, 0), Some(Value::Null));
    assert!(!m.is_cell_modified(3, 0));
    assert!(m.is_cell_modified(3, 1));
    // Delete one stored and one inserted row.
    assert_eq!(m.delete_rows(&[0, 3, 0]).unwrap(), 2);
    assert_eq!(m.row_count(), 5);
    assert_eq!(m.row_state(0), RowState::Deleted);
    assert!(m.set_cell(0, 1, "z").unwrap_err().contains("deletion"));
    let counts = m.pending_counts();
    assert_eq!(counts, PendingCounts { updated: 0, inserted: 1, deleted: 1 });
    assert!(m.undo());
    assert_eq!(m.row_count(), 6);
    assert_eq!(m.row_state(0), RowState::Unchanged);
    assert_eq!(m.cell(3, 1), Some(Value::text("p1")), "inserted row restored at its position");
    m.discard_changes();
    assert!(!m.has_changes());
    assert_eq!(m.row_count(), 4);
}

#[test]
fn change_statements_parameterised() {
    let mut m = model(4);
    m.set_cell(1, 1, "Bob").unwrap();
    m.set_cell(1, 2, "33").unwrap();
    m.set_default(2, 3).unwrap();
    let r = m.add_row(None).unwrap();
    m.set_cell(r, 1, "New").unwrap();
    m.add_row(None).unwrap(); // all defaults
    m.delete_rows(&[3]).unwrap();
    let cs = m.change_set().unwrap();
    assert_eq!(cs.len(), 5);
    assert_eq!(cs.summary(), "2 updated, 2 inserted, 1 deleted");
    let stmts = cs.statements(&PgLike(GenericDialect::new()));
    assert_eq!(stmts[0], ("DELETE FROM app.people WHERE id = $1".to_string(), vec![Value::Int(4)]));
    assert_eq!(stmts[1], ("UPDATE app.people SET name = $1, age = $2 WHERE id = $3".to_string(), vec![Value::text("Bob"), Value::Int(33), Value::Int(2)]));
    assert_eq!(stmts[2], ("UPDATE app.people SET score = DEFAULT WHERE id = $1".to_string(), vec![Value::Int(3)]));
    assert_eq!(stmts[3], ("INSERT INTO app.people (name) VALUES ($1)".to_string(), vec![Value::text("New")]));
    assert_eq!(stmts[4], ("INSERT INTO app.people DEFAULT VALUES".to_string(), vec![]));
    let generic = m.change_statements(&GenericDialect::new());
    assert_eq!(generic[1].0, "UPDATE app.people SET name = ?, age = ? WHERE id = ?");
    let mysql = m.change_statements(&MyLike(GenericDialect::new()));
    assert_eq!(mysql[4].0, "INSERT INTO app.people () VALUES ()");
    let preview = m.change_preview_sql(&GenericDialect::new());
    assert_eq!(
        preview,
        "DELETE FROM app.people WHERE id = 4;\n\
         UPDATE app.people SET name = 'Bob', age = 33 WHERE id = 2;\n\
         UPDATE app.people SET score = DEFAULT WHERE id = 3;\n\
         INSERT INTO app.people (name) VALUES ('New');\n\
         INSERT INTO app.people DEFAULT VALUES;"
    );
}

#[test]
fn null_safe_keys_and_key_updates() {
    let cols = vec![ColumnMeta::new("code", "text", DataKind::Text).with_source(src("code")), ColumnMeta::new("v", "int", DataKind::Int).with_source(src("v"))];
    let mut m = ResultModel::from_rows(cols, vec![vec![Value::Null, Value::Int(1)], vec![Value::text("a"), Value::Int(2)]]);
    m.set_editability(determine_editability(m.columns(), |_| Some(vec!["code".into()]), false));
    m.set_cell(0, 1, "10").unwrap();
    m.set_cell(1, 0, "b").unwrap();
    let stmts = m.change_statements(&PgLike(GenericDialect::new()));
    assert_eq!(stmts[0], ("UPDATE app.people SET v = $1 WHERE code IS NULL".to_string(), vec![Value::Int(10)]));
    assert_eq!(stmts[1], ("UPDATE app.people SET code = $1 WHERE code = $2".to_string(), vec![Value::text("b"), Value::text("a")]));
}

#[test]
fn apply_saved_updates_store_and_view() {
    let mut m = model(5);
    m.set_cell(0, 2, "99").unwrap();
    let r = m.add_row(Some(1)).unwrap();
    assert_eq!(r, 2);
    m.set_cell(r, 1, "ins").unwrap();
    m.delete_rows(&[4]).unwrap(); // stored row 3
    m.apply_saved();
    assert!(!m.has_changes());
    assert!(!m.can_undo());
    assert_eq!(m.row_count(), 5);
    assert_eq!(m.stored_row_count(), 6);
    let names: Vec<Value> = (0..5).map(|d| m.cell(d, 1).unwrap()).collect();
    assert_eq!(names, vec![Value::text("p0"), Value::text("p1"), Value::text("ins"), Value::text("p2"), Value::text("p4")]);
    assert_eq!(m.store().lock().cell(0, 2), Some(&Value::Int(99)));
    assert_eq!(m.row_state(0), RowState::Unchanged);
    // Rows fetched later are appended to the (materialized) view.
    m.store().lock().push(rows(7).pop().unwrap());
    assert!(m.sync());
    assert_eq!(m.row_count(), 6);
    assert_eq!(m.cell(5, 1), Some(Value::text("p6")));
    assert!(!m.is_view_stale());
    // Clearing the view keeps saved deletions hidden.
    m.clear_view();
    assert_eq!(m.row_count(), 6);
    assert!((0..6).all(|d| m.cell(d, 1) != Some(Value::text("p3"))));
}

#[test]
fn apply_saved_simple_append_keeps_identity() {
    let mut m = model(3);
    let r = m.add_row(None).unwrap();
    m.set_cell(r, 1, "z").unwrap();
    m.apply_saved();
    assert!(m.view().is_none());
    assert_eq!(m.row_count(), 4);
    assert_eq!(m.cell(3, 1), Some(Value::text("z")));
}

#[test]
fn client_view_and_inserted_anchor() {
    let mut m = model(6);
    let idx =
        crate::view::compute_view(m.store(), &ViewSpec { sort: vec![SortKey::desc(0)], filter: None }, &m.view_context(), &std::sync::atomic::AtomicBool::new(false)).unwrap();
    m.set_view_index(idx, ViewSpec { sort: vec![SortKey::desc(0)], filter: None });
    assert_eq!(m.cell(0, 0), Some(Value::Int(6)));
    // Insert after the row with id 6, then re-sort ascending: the new row follows id 6.
    let r = m.add_row(Some(0)).unwrap();
    assert_eq!(r, 1);
    let spec = ViewSpec { sort: vec![SortKey::asc(0)], filter: None };
    let idx = crate::view::compute_view(m.store(), &spec, &m.view_context(), &std::sync::atomic::AtomicBool::new(false)).unwrap();
    m.set_view_index(idx, spec);
    assert_eq!(m.row_state(6), RowState::Inserted);
    assert_eq!(m.cell(5, 0), Some(Value::Int(6)));
    // Appended rows make a client view stale.
    m.store().lock().push(rows(8).pop().unwrap());
    m.sync();
    assert!(m.is_view_stale());
    assert_eq!(m.rows_outside_view(), 1);
    assert_eq!(m.row_count(), 7);
    // Filter hides rows; inserted rows stay visible.
    let f = ClientFilter::parse("age > 21", &m.column_names()).unwrap();
    let spec = ViewSpec { sort: vec![], filter: Some(f) };
    let idx = crate::view::compute_view(m.store(), &spec, &m.view_context(), &std::sync::atomic::AtomicBool::new(false)).unwrap();
    m.set_view_index(idx, spec);
    assert_eq!(m.stored_row_count(), 7);
    let visible: Vec<Option<Value>> = (0..m.row_count()).map(|d| m.cell(d, 2)).collect();
    assert!(visible.contains(&Some(Value::Null)), "inserted row (age NULL) stays visible");
    assert!(m.has_client_view());
    m.clear_view();
    assert!(!m.has_client_view());
    assert_eq!(m.row_count(), 8);
}

#[test]
fn background_client_view() {
    let mut m = model(50);
    m.start_client_view(ViewSpec { sort: vec![SortKey::desc(1)], filter: None }, Box::new(|| {}));
    assert!(m.is_view_job_running());
    let start = std::time::Instant::now();
    loop {
        if let Some(r) = m.poll_view_job() {
            r.unwrap();
            break;
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(!m.is_view_job_running());
    assert_eq!(m.cell(0, 1), Some(Value::text("p9")));
    assert_eq!(m.client_view_spec().sort, vec![SortKey::desc(1)]);
}

#[test]
fn virtual_columns() {
    let mut m = model(3);
    let c = m.add_virtual_column("double age", "coalesce(age, 0) * 2").unwrap();
    assert_eq!(c, 5);
    assert_eq!(m.column_count(), 6);
    assert!(m.is_virtual_column(5));
    assert_eq!(m.column_name(5), "double age");
    assert_eq!(m.cell(1, 5), Some(Value::Int(42)));
    assert_eq!(m.cell(0, 5), Some(Value::Int(0)));
    // Pending edits are reflected.
    m.set_cell(1, 2, "50").unwrap();
    assert_eq!(m.cell(1, 5), Some(Value::Int(100)));
    assert!(m.set_cell(1, 5, "1").is_err());
    assert!(m.add_virtual_column("bad", "nope + 1").is_err());
    m.update_virtual_column(0, "", "upper(name)").unwrap();
    assert_eq!(m.column_name(5), "upper(name)");
    assert_eq!(m.row_values(2).unwrap()[5], Value::text("P2"));
    m.remove_virtual_column(0);
    assert_eq!(m.column_count(), 5);
}

#[test]
fn paste_block_adds_rows_and_validates() {
    let mut m = model(2);
    let block = vec![vec!["Ann".to_string(), "30".into()], vec!["Ben".into(), "31".into()], vec!["Cy".into(), "32".into()]];
    let n = m.paste_block(1, &block, &[1, 2], None).unwrap();
    assert_eq!(n, 6);
    assert_eq!(m.row_count(), 4);
    assert_eq!(m.cell(1, 1), Some(Value::text("Ann")));
    assert_eq!(m.cell(3, 2), Some(Value::Int(32)));
    assert_eq!(m.row_state(2), RowState::Inserted);
    assert!(m.undo());
    assert_eq!(m.row_count(), 2);
    assert!(!m.has_changes());
    // Invalid number: nothing changes.
    let bad = vec![vec!["Ann".to_string(), "x".into()]];
    let err = m.paste_block(0, &bad, &[1, 2], None).unwrap_err();
    assert!(err.contains("Row 1"), "{err}");
    assert!(!m.has_changes());
    // Virtual / computed targets are skipped or rejected.
    let err = m.paste_block(0, &[vec!["q".to_string()]], &[4], None).unwrap_err();
    assert!(err.contains("label"), "{err}");
}

#[test]
fn dictionary_enum_fk() {
    let mut m = model(2);
    let mut d = HashMap::new();
    d.insert("21".to_string(), "twenty-one".to_string());
    d.insert("3".to_string(), "three".to_string());
    m.set_dictionary(2, d);
    assert_eq!(m.describe_value(2, &Value::Int(21)), Some("twenty-one"));
    assert_eq!(m.describe_value(2, &Value::Null), None);
    let entries: Vec<&str> = m.dictionary(2).unwrap().entries().iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(entries, vec!["3", "21"]);
    m.set_enum_values(1, vec!["a".into(), "b".into()]);
    assert_eq!(m.enum_values(1).unwrap().len(), 2);
    m.set_foreign_keys(vec![ForeignKeyLink {
        name: Some("fk".into()),
        columns: vec![2],
        ref_table: ObjectRef::table(Some("app".into()), "ages"),
        ref_columns: vec!["age".into()],
    }]);
    assert_eq!(m.foreign_key_for_column(2), Some(0));
    assert_eq!(m.foreign_key_for_column(1), None);
}

#[test]
fn sync_handles_cleared_store_and_reset() {
    let mut m = model(3);
    m.set_cell(0, 1, "q").unwrap();
    m.store().lock().clear();
    assert!(m.sync());
    assert_eq!(m.row_count(), 0);
    assert!(!m.has_changes());
    let _ = m.add_virtual_column("v", "id + 1");
    let store = RowStore::shared(1 << 20);
    store.lock().extend(rows(2));
    m.reset(columns(), store);
    assert_eq!(m.row_count(), 2);
    assert_eq!(m.column_count(), 6, "virtual column kept for identical columns");
    assert!(m.is_editable());
}

#[test]
fn values_bulk_access() {
    let m = model(10);
    let v = m.values(&[5, 1, 2, 9], &[1, 0]);
    assert_eq!(v[0], vec![Value::text("p5"), Value::Int(6)]);
    assert_eq!(v[1], vec![Value::text("p1"), Value::Int(2)]);
    assert_eq!(v[3], vec![Value::text("p9"), Value::Int(10)]);
}

#[test]
fn generate_rows_sql() {
    let mut m = model(3);
    m.set_cell(1, 2, "77").unwrap();
    let d = GenericDialect::new();
    let sql = m.generate_rows_sql(SqlKind::Update, &[1], &d, "app.people");
    assert_eq!(sql, "UPDATE app.people SET name = 'p1', age = 77, score = 1.5 WHERE id = 2;");
    let sql = m.generate_rows_sql(SqlKind::Insert, &[0], &d, "t");
    assert_eq!(sql, "INSERT INTO t (id, name, age, score) VALUES (1, 'p0', NULL, 0.5);");
    let sql = m.generate_rows_sql(SqlKind::Delete, &[2, 0], &d, "t");
    assert_eq!(sql, "DELETE FROM t WHERE id = 1;\nDELETE FROM t WHERE id = 3;");
    let sql = m.generate_rows_sql(SqlKind::Select, &[0], &d, "t");
    assert_eq!(sql, "SELECT * FROM t WHERE id = 1;");
    // Without a key every column identifies the row (NULL-safe).
    let ro = ResultModel::from_rows(vec![ColumnMeta::new("a", "int", DataKind::Int)], vec![vec![Value::Null]]);
    assert_eq!(ro.generate_rows_sql(SqlKind::Delete, &[0], &d, "t"), "DELETE FROM t WHERE a IS NULL;");
}

/// `id`, a `timestamptz` and a text column of app.people.
fn timestamp_model() -> ResultModel {
    let mut id = ColumnMeta::new("id", "int4", DataKind::Int).with_source(src("id"));
    id.nullable = Some(false);
    let at = ColumnMeta::new("at", "timestamptz", DataKind::DateTimeTz).with_source(src("at"));
    let note = ColumnMeta::new("note", "text", DataKind::Text).with_source(src("note"));
    let stamp = Value::parse_as("2025-11-24 13:48:03.461+07", DataKind::DateTimeTz).unwrap();
    let mut m = ResultModel::from_rows(vec![id, at, note], vec![vec![Value::Int(1), stamp.clone(), Value::text("a")], vec![Value::Int(2), stamp, Value::text("b")]]);
    m.set_editability(determine_editability(m.columns(), |_| Some(vec!["id".into()]), false));
    m
}

#[test]
fn sql_functions_in_typed_columns_are_saved_as_typed() {
    let mut m = timestamp_model();
    let d = PgLike(GenericDialect::new());
    // now() is no timestamp: the database evaluates it.
    m.set_cell(0, 1, "now()").unwrap();
    assert!(m.is_cell_modified(0, 1));
    assert_eq!(m.row_state(0), RowState::Modified);
    assert_eq!(m.cell(0, 1), Some(Value::text("now()")), "shown and copied as typed");
    // In a text column it is text.
    m.set_cell(1, 2, "now()").unwrap();
    assert_eq!(m.cell(1, 2), Some(Value::text("now()")));
    // Not every word: a mistyped value is still refused.
    assert!(m.set_cell(1, 1, "yesterday").is_err());
    assert!(m.set_cell(1, 1, "2025-13-01").is_err());
    let stmts = m.change_statements(&d);
    assert_eq!(stmts[0].0, "UPDATE app.people SET at = now() WHERE id = $1");
    assert_eq!(stmts[1], ("UPDATE app.people SET note = $1 WHERE id = $2".to_string(), vec![Value::text("now()"), Value::Int(2)]));
    assert!(m.change_preview_sql(&d).contains("SET at = now() WHERE id = 1"));
    // A new row with an expression; a pasted one; generated SQL keeps it.
    let r = m.add_row(None).unwrap();
    m.set_cell(r, 0, "3").unwrap();
    m.set_cell(r, 1, "CURRENT_TIMESTAMP").unwrap();
    let stmts = m.change_statements(&d);
    assert_eq!(stmts.last().unwrap(), &("INSERT INTO app.people (id, at) VALUES ($1, CURRENT_TIMESTAMP)".to_string(), vec![Value::Int(3)]));
    assert!(m.generate_rows_sql(crate::grid::SqlKind::Update, &[0], &d, "app.people").contains("at = now()"));
    m.paste_block(1, &[vec!["now() - interval '1 day'".into()]], &[1], None).unwrap();
    assert!(m.change_statements(&d)[1].0.contains("at = now() - interval '1 day'"));
    // Saved: the values the database gave are shown (read back), not NULL.
    let stamp = Value::parse_as("2026-10-27 10:00:00+07", DataKind::DateTimeTz).unwrap();
    m.apply_saved_with(&HashMap::new(), &HashMap::from([(0u32, vec![(1usize, stamp.clone())])]));
    assert!(!m.has_changes());
    assert_eq!(m.cell(0, 1), Some(stamp));
    assert_eq!(m.cell(0, 2), Some(Value::text("a")));
}
