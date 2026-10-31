//! The MongoDB dialect: the shell's lexical rules for the editor.
//!
//! MongoDB has no SQL, so the syntax only describes how the editor splits,
//! highlights and classifies statements (see [`dbiji_core::Shell`]): strings,
//! comments, `{}`/`[]` nesting and the keywords of the shell API.

use crate::ddl::MongoDdl;
use dbiji_core::DbKind;
use dbiji_core::Shell;
use dbiji_core::ddl::DdlGenerator;
use dbiji_core::dialect::{Dialect, IdentCase, SqlSyntax};

/// Keywords offered by completion and highlighted.
pub const KEYWORDS: &[&str] = &[
    "db",
    "use",
    "show",
    "collections",
    "databases",
    "dbs",
    "tables",
    "find",
    "findOne",
    "aggregate",
    "count",
    "countDocuments",
    "estimatedDocumentCount",
    "distinct",
    "insert",
    "insertOne",
    "insertMany",
    "update",
    "updateOne",
    "updateMany",
    "replaceOne",
    "deleteOne",
    "deleteMany",
    "findOneAndUpdate",
    "findOneAndReplace",
    "findOneAndDelete",
    "createIndex",
    "createIndexes",
    "dropIndex",
    "dropIndexes",
    "getIndexes",
    "getCollection",
    "getSiblingDB",
    "getDB",
    "createCollection",
    "drop",
    "dropDatabase",
    "runCommand",
    "adminCommand",
    "stats",
    "explain",
    "sort",
    "skip",
    "limit",
    "project",
    "renameCollection",
];

/// Aggregation stages and operators.
pub const FUNCTIONS: &[&str] = &[
    "$match",
    "$group",
    "$sort",
    "$limit",
    "$skip",
    "$project",
    "$lookup",
    "$unwind",
    "$addFields",
    "$set",
    "$unset",
    "$count",
    "$facet",
    "$bucket",
    "$replaceRoot",
    "$merge",
    "$out",
    "$sum",
    "$avg",
    "$min",
    "$max",
    "$push",
    "$addToSet",
    "$first",
    "$last",
    "$cond",
    "$ifNull",
    "$concat",
    "$toUpper",
    "$toLower",
    "$dateToString",
    "$toString",
    "$toInt",
    "$toDouble",
    "$in",
    "$nin",
    "$gt",
    "$gte",
    "$lt",
    "$lte",
    "$eq",
    "$ne",
    "$exists",
    "$regex",
    "$size",
    "$elemMatch",
    "$all",
    "$set",
    "$unset",
    "$inc",
    "$mul",
    "$rename",
    "$currentDate",
];

/// BSON types written as shell constructors.
pub const TYPES: &[&str] = &["ObjectId", "ISODate", "NumberLong", "NumberInt", "NumberDecimal", "Timestamp", "BinData", "UUID", "RegExp", "Date", "MinKey", "MaxKey"];

/// Lexical rules of the MongoDB shell.
pub const fn mongo_syntax() -> SqlSyntax {
    SqlSyntax {
        keywords: KEYWORDS,
        functions: FUNCTIONS,
        data_types: TYPES,
        identifier_quotes: &[('"', '"'), ('\'', '\'')],
        double_quoted_strings: true,
        backslash_escapes: true,
        e_strings: false,
        n_strings: false,
        x_strings: false,
        dollar_quoting: false,
        hash_comments: false,
        nested_block_comments: false,
        dash_comment_needs_space: false,
        question_params: false,
        dollar_params: false,
        at_variables: false,
        batch_separator: None,
        delimiter_command: false,
        compound_blocks: false,
        extra_ident_chars: "$_",
        ident_case: IdentCase::Preserve,
        case_sensitive_names: true,
        shell: Some(Shell::Mongo),
    }
}

/// The MongoDB dialect.
pub struct MongoDialect {
    syntax: SqlSyntax,
    ddl: MongoDdl,
}

impl MongoDialect {
    /// Create the dialect.
    pub fn new() -> Self {
        MongoDialect { syntax: mongo_syntax(), ddl: MongoDdl::new(mongo_syntax()) }
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
        &self.syntax
    }

    fn ddl(&self) -> &dyn DdlGenerator {
        &self.ddl
    }

    /// `db.coll.find({})` — the shell has no `SELECT`.
    fn table_data_query(&self, table: &dbiji_core::meta::ObjectRef, limit: Option<u64>) -> String {
        let coll = crate::ddl::quote(&table.name);
        let target = match table.schema.as_deref().filter(|s| !s.is_empty()) {
            Some(db) => format!("db.getSiblingDB({}).getCollection({coll})", crate::ddl::quote(db)),
            None => format!("db.getCollection({coll})"),
        };
        match limit {
            Some(n) => format!("{target}.find({{}}).limit({n})"),
            None => format!("{target}.find({{}})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dialect_speaks_the_shell() {
        let d = MongoDialect::new();
        assert_eq!(d.kind(), DbKind::MongoDb);
        assert_eq!(d.syntax().shell, Some(Shell::Mongo));
        assert!(d.syntax().is_keyword("find"));
        assert!(d.syntax().is_function("$group"));
        let t = dbiji_core::meta::ObjectRef::table(Some("shop".into()), "orders");
        assert_eq!(d.table_data_query(&t, Some(10)), "db.getSiblingDB(\"shop\").getCollection(\"orders\").find({}).limit(10)");
    }
}
