//! MongoDB "dialect": shell statements instead of SQL. Collections are the
//! navigator's tables; the data tab reads them with `find`.

use crate::shell::key;
use dbiji_core::DbKind;
use dbiji_core::ddl::*;
use dbiji_core::dialect::{Dialect, IdentCase, SqlSyntax};
use dbiji_core::meta::ObjectRef;
use dbiji_core::value::{DataKind, Value};

const METHODS: &[&str] = &[
    "db",
    "find",
    "findOne",
    "aggregate",
    "countDocuments",
    "estimatedDocumentCount",
    "count",
    "distinct",
    "insertOne",
    "insertMany",
    "updateOne",
    "updateMany",
    "replaceOne",
    "deleteOne",
    "deleteMany",
    "drop",
    "createIndex",
    "dropIndex",
    "getIndexes",
    "sort",
    "limit",
    "skip",
    "projection",
    "explain",
    "getCollection",
    "getSiblingDB",
    "runCommand",
    "adminCommand",
    "getCollectionNames",
    "createCollection",
    "dropDatabase",
    "stats",
    "show",
    "use",
    "dbs",
    "collections",
];

const FUNCTIONS: &[&str] = &[
    "ObjectId",
    "ISODate",
    "Date",
    "NumberLong",
    "NumberInt",
    "NumberDecimal",
    "UUID",
    "BinData",
    "Timestamp",
    "RegExp",
    "MinKey",
    "MaxKey",
];

const OPERATORS: &[&str] = &[
    "$eq",
    "$ne",
    "$gt",
    "$gte",
    "$lt",
    "$lte",
    "$in",
    "$nin",
    "$and",
    "$or",
    "$not",
    "$nor",
    "$exists",
    "$type",
    "$regex",
    "$expr",
    "$elemMatch",
    "$size",
    "$all",
    "$set",
    "$unset",
    "$inc",
    "$push",
    "$pull",
    "$addToSet",
    "$match",
    "$group",
    "$project",
    "$sort",
    "$limit",
    "$skip",
    "$lookup",
    "$unwind",
    "$count",
    "$sum",
    "$avg",
    "$min",
    "$max",
    "$first",
    "$last",
    "$facet",
    "$addFields",
    "$sample",
    "$text",
    "$search",
];

pub const SYNTAX: SqlSyntax = SqlSyntax {
    keywords: METHODS,
    functions: FUNCTIONS,
    data_types: OPERATORS,
    identifier_quotes: &[],
    double_quoted_strings: true,
    backslash_escapes: true,
    e_strings: false,
    n_strings: false,
    x_strings: false,
    dollar_quoting: false,
    hash_comments: false,
    nested_block_comments: false,
    dash_comment_needs_space: true,
    question_params: false,
    dollar_params: false,
    at_variables: false,
    batch_separator: None,
    delimiter_command: false,
    compound_blocks: false,
    plsql_blocks: false,
    newline_ends_statement: true,
    extra_ident_chars: "$",
    ident_case: IdentCase::Preserve,
    case_sensitive_names: true,
};

fn json(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// `db.getSiblingDB("shop").getCollection("users")`.
pub(crate) fn collection(db: Option<&str>, name: &str) -> String {
    match db {
        Some(d) if !d.is_empty() => {
            format!("db.getSiblingDB({}).getCollection({})", json(d), json(name))
        }
        _ => format!("db.getCollection({})", json(name)),
    }
}

/// `name DESC, age` (the data tab's ORDER BY) as a sort document.
fn sort_doc(order_by: &str) -> String {
    let parts: Vec<String> = order_by
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut w = p.split_whitespace();
            let field = w.next().unwrap_or("").trim_matches('"');
            let desc = w.next().is_some_and(|d| d.eq_ignore_ascii_case("desc"));
            format!("{}: {}", key(field), if desc { -1 } else { 1 })
        })
        .collect();
    format!("{{{}}}", parts.join(", "))
}

pub struct MongoDialect {
    ddl: MongoDdl,
}

impl MongoDialect {
    pub fn new() -> Self {
        MongoDialect { ddl: MongoDdl }
    }
}

impl Default for MongoDialect {
    fn default() -> Self {
        Self::new()
    }
}

impl Dialect for MongoDialect {
    fn kind(&self) -> DbKind {
        DbKind::MongoDb
    }
    fn syntax(&self) -> &SqlSyntax {
        &SYNTAX
    }
    fn ddl(&self) -> &dyn DdlGenerator {
        &self.ddl
    }
    fn quote_ident(&self, ident: &str) -> String {
        key(ident)
    }
    fn needs_quoting(&self, ident: &str) -> bool {
        key(ident) != ident
    }
    fn qualified(&self, schema: Option<&str>, name: &str) -> String {
        collection(schema, name)
    }
    fn qualified_ref(&self, obj: &ObjectRef) -> String {
        collection(obj.schema.as_deref(), &obj.name)
    }
    fn string_literal(&self, s: &str) -> String {
        json(s)
    }
    fn literal(&self, value: &Value, _type_name: Option<&str>) -> String {
        match value {
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) if f.is_finite() => f.to_string(),
            Value::Json(j) => j.to_string(),
            other => json(&other.to_text()),
        }
    }
    fn limit_query(&self, sql: &str, limit: u64, offset: u64) -> String {
        let s = sql.trim().trim_end_matches(';');
        if !s.contains(".find(") {
            return s.to_string();
        }
        if offset > 0 {
            format!("{s}.skip({offset}).limit({limit})")
        } else {
            format!("{s}.limit({limit})")
        }
    }
    /// `find(…)` counted: `.count()` runs countDocuments with its filter.
    fn count_query(&self, sql: &str) -> String {
        format!("{}.count()", sql.trim().trim_end_matches(';'))
    }
    fn aggregate_query(&self, sql: &str, _select_list: &str) -> String {
        self.count_query(sql)
    }
    /// `find(<filter>)`: the data tab's filter box takes a query document.
    fn select_table(
        &self,
        table: &ObjectRef,
        columns: Option<&[String]>,
        where_clause: Option<&str>,
        order_by: Option<&str>,
        limit: Option<u64>,
    ) -> String {
        let filter = where_clause
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .unwrap_or("{}");
        let mut s = format!(
            "{}.find({filter}",
            collection(table.schema.as_deref(), &table.name)
        );
        if let Some(cols) = columns.filter(|c| !c.is_empty()) {
            s.push_str(&format!(
                ", {{{}}}",
                cols.iter()
                    .map(|c| format!("{}: 1", key(c)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        s.push(')');
        if let Some(o) = order_by.filter(|o| !o.trim().is_empty()) {
            s.push_str(&format!(".sort({})", sort_doc(o)));
        }
        if let Some(l) = limit {
            s.push_str(&format!(".limit({l})"));
        }
        s
    }
    fn truncate_table(&self, table: &ObjectRef) -> String {
        format!(
            "{}.deleteMany({{}})",
            collection(table.schema.as_deref(), &table.name)
        )
    }
}

pub struct MongoDdl;

const NOT_MONGO: &str = "// Not available for MongoDB collections";

impl DdlGenerator for MongoDdl {
    fn create_table(&self, t: &TableDef, _include_foreign_keys: bool) -> Vec<String> {
        let mut out = vec![match &t.schema {
            Some(d) => format!(
                "db.getSiblingDB({}).createCollection({})",
                json(d),
                json(&t.name)
            ),
            None => format!("db.createCollection({})", json(&t.name)),
        }];
        for i in t.indexes.iter().filter(|i| i.name != "_id_") {
            out.push(self.create_index(t, i));
        }
        out
    }
    fn drop_table(&self, schema: Option<&str>, name: &str, _cascade: bool) -> String {
        format!("{}.drop()", collection(schema, name))
    }
    fn rename_table(&self, schema: Option<&str>, old_name: &str, new_name: &str) -> String {
        format!(
            "{}.renameCollection({})",
            collection(schema, old_name),
            json(new_name)
        )
    }
    fn alter_table(
        &self,
        from: &TableDef,
        to: &TableDef,
        _renamed: &[(String, String)],
    ) -> Vec<String> {
        let mut out = Vec::new();
        for i in &from.indexes {
            if !to.indexes.iter().any(|x| x.name == i.name) {
                out.push(self.drop_index(to, i));
            }
        }
        for i in &to.indexes {
            if !from.indexes.iter().any(|x| x.name == i.name) {
                out.push(self.create_index(to, i));
            }
        }
        out
    }
    fn add_column(&self, _table: &TableDef, _column: &ColumnDef) -> Vec<String> {
        Vec::new()
    }
    fn drop_column(&self, table: &TableDef, column: &str) -> String {
        format!(
            "{}.updateMany({{}}, {{$unset: {{{}: \"\"}}}})",
            collection(table.schema.as_deref(), &table.name),
            key(column)
        )
    }
    fn create_index(&self, table: &TableDef, index: &IndexDef) -> String {
        let keys: Vec<String> = index
            .columns
            .iter()
            .map(|c| format!("{}: {}", key(&c.name), if c.descending { -1 } else { 1 }))
            .collect();
        let mut opts = vec![format!("name: {}", json(&index.name))];
        if index.unique {
            opts.push("unique: true".into());
        }
        format!(
            "{}.createIndex({{{}}}, {{{}}})",
            collection(table.schema.as_deref(), &table.name),
            keys.join(", "),
            opts.join(", ")
        )
    }
    fn drop_index(&self, table: &TableDef, index: &IndexDef) -> String {
        format!(
            "{}.dropIndex({})",
            collection(table.schema.as_deref(), &table.name),
            json(&index.name)
        )
    }
    fn add_primary_key(&self, _t: &TableDef, _k: &KeyDef) -> String {
        NOT_MONGO.into()
    }
    fn drop_primary_key(&self, _t: &TableDef, _k: &KeyDef) -> String {
        NOT_MONGO.into()
    }
    fn add_unique_key(&self, _t: &TableDef, _k: &KeyDef) -> String {
        NOT_MONGO.into()
    }
    fn drop_unique_key(&self, _t: &TableDef, _k: &KeyDef) -> String {
        NOT_MONGO.into()
    }
    fn add_check(&self, _t: &TableDef, _c: &CheckDef) -> String {
        NOT_MONGO.into()
    }
    fn drop_check(&self, _t: &TableDef, _c: &CheckDef) -> String {
        NOT_MONGO.into()
    }
    fn add_foreign_key(&self, _t: &TableDef, _f: &ForeignKeyDef) -> String {
        NOT_MONGO.into()
    }
    fn drop_foreign_key(&self, _t: &TableDef, _f: &ForeignKeyDef) -> String {
        NOT_MONGO.into()
    }
    fn create_trigger(&self, _t: &TableDef, _tr: &TriggerDef) -> Vec<String> {
        Vec::new()
    }
    fn drop_trigger(&self, _t: &TableDef, _tr: &TriggerDef) -> String {
        NOT_MONGO.into()
    }
    fn create_view(&self, view: &ViewDef, _or_replace: bool) -> Vec<String> {
        vec![view.definition.clone()]
    }
    fn drop_view(&self, view: &ViewDef) -> String {
        format!("{}.drop()", collection(view.schema.as_deref(), &view.name))
    }
    fn create_sequence(&self, _s: &SequenceDef) -> String {
        NOT_MONGO.into()
    }
    fn alter_sequence(&self, _f: &SequenceDef, _t: &SequenceDef) -> Vec<String> {
        Vec::new()
    }
    fn drop_sequence(&self, _s: &SequenceDef) -> String {
        NOT_MONGO.into()
    }
    fn create_routine(&self, _r: &RoutineDef) -> Vec<String> {
        Vec::new()
    }
    fn drop_routine(&self, _r: &RoutineDef) -> String {
        NOT_MONGO.into()
    }
    fn create_schema(&self, name: &str) -> String {
        format!("db.getSiblingDB({}).createCollection(\"init\")", json(name))
    }
    fn drop_schema(&self, name: &str, _cascade: bool) -> String {
        format!("db.getSiblingDB({}).dropDatabase()", json(name))
    }
    fn map_type(&self, _source: DbKind, _data_type: &str, kind: DataKind) -> String {
        self.default_type_for(kind)
    }
    fn default_type_for(&self, kind: DataKind) -> String {
        match kind {
            DataKind::Bool => "bool",
            DataKind::Int => "long",
            DataKind::Float => "double",
            DataKind::Decimal => "decimal",
            DataKind::Binary => "binData",
            DataKind::Date | DataKind::DateTime | DataKind::DateTimeTz => "date",
            DataKind::Json => "object",
            DataKind::Array => "array",
            _ => "string",
        }
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::meta::ObjectKind;

    #[test]
    fn the_data_tab_reads_with_find() {
        let d = MongoDialect::new();
        let t = ObjectRef::new(ObjectKind::Table, Some("shop".into()), "users");
        let s = d.select_table(
            &t,
            None,
            Some("{age: {$gt: 3}}"),
            Some("name DESC, age"),
            Some(50),
        );
        assert_eq!(
            s,
            "db.getSiblingDB(\"shop\").getCollection(\"users\").find({age: {$gt: 3}}).sort({name: -1, age: 1}).limit(50)"
        );
        assert_eq!(
            d.count_query("db.getCollection(\"users\").find({})"),
            "db.getCollection(\"users\").find({}).count()"
        );
        assert!(crate::shell::parse(&s).is_ok());
        assert!(crate::shell::parse(&d.count_query(&s)).is_ok());
        assert_eq!(
            d.ddl().drop_table(Some("shop"), "users", false),
            "db.getSiblingDB(\"shop\").getCollection(\"users\").drop()"
        );
    }
}
