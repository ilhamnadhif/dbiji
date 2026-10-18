//! Metadata model used by the navigator, object editors, autocomplete and
//! search. Definitions used for DDL generation live in [`crate::ddl`].

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    Database,
    Schema,
    Table,
    View,
    MaterializedView,
    ForeignTable,
    Column,
    Index,
    PrimaryKey,
    UniqueKey,
    CheckConstraint,
    ForeignKey,
    Trigger,
    Sequence,
    Function,
    Procedure,
    Aggregate,
    Type,
    Domain,
    Extension,
    Event,
    Partition,
    Synonym,
    Role,
    Tablespace,
}

impl ObjectKind {
    /// Singular English label, routed through i18n by the UI.
    pub fn label(self) -> &'static str {
        match self {
            ObjectKind::Database => "Database",
            ObjectKind::Schema => "Schema",
            ObjectKind::Table => "Table",
            ObjectKind::View => "View",
            ObjectKind::MaterializedView => "Materialized View",
            ObjectKind::ForeignTable => "Foreign Table",
            ObjectKind::Column => "Column",
            ObjectKind::Index => "Index",
            ObjectKind::PrimaryKey => "Primary Key",
            ObjectKind::UniqueKey => "Unique Key",
            ObjectKind::CheckConstraint => "Check Constraint",
            ObjectKind::ForeignKey => "Foreign Key",
            ObjectKind::Trigger => "Trigger",
            ObjectKind::Sequence => "Sequence",
            ObjectKind::Function => "Function",
            ObjectKind::Procedure => "Procedure",
            ObjectKind::Aggregate => "Aggregate",
            ObjectKind::Type => "Type",
            ObjectKind::Domain => "Domain",
            ObjectKind::Extension => "Extension",
            ObjectKind::Event => "Event",
            ObjectKind::Partition => "Partition",
            ObjectKind::Synonym => "Synonym",
            ObjectKind::Role => "Role",
            ObjectKind::Tablespace => "Tablespace",
        }
    }

    /// Plural label used for navigator folders.
    pub fn plural_label(self) -> &'static str {
        match self {
            ObjectKind::Database => "Databases",
            ObjectKind::Schema => "Schemas",
            ObjectKind::Table => "Tables",
            ObjectKind::View => "Views",
            ObjectKind::MaterializedView => "Materialized Views",
            ObjectKind::ForeignTable => "Foreign Tables",
            ObjectKind::Column => "Columns",
            ObjectKind::Index => "Indexes",
            ObjectKind::PrimaryKey => "Primary Keys",
            ObjectKind::UniqueKey => "Unique Keys",
            ObjectKind::CheckConstraint => "Check Constraints",
            ObjectKind::ForeignKey => "Foreign Keys",
            ObjectKind::Trigger => "Triggers",
            ObjectKind::Sequence => "Sequences",
            ObjectKind::Function => "Functions",
            ObjectKind::Procedure => "Procedures",
            ObjectKind::Aggregate => "Aggregates",
            ObjectKind::Type => "Types",
            ObjectKind::Domain => "Domains",
            ObjectKind::Extension => "Extensions",
            ObjectKind::Event => "Events",
            ObjectKind::Partition => "Partitions",
            ObjectKind::Synonym => "Synonyms",
            ObjectKind::Role => "Roles",
            ObjectKind::Tablespace => "Tablespaces",
        }
    }

    /// Objects whose rows can be browsed in a data editor.
    pub fn has_data(self) -> bool {
        matches!(
            self,
            ObjectKind::Table
                | ObjectKind::View
                | ObjectKind::MaterializedView
                | ObjectKind::ForeignTable
                | ObjectKind::Partition
        )
    }

    pub fn is_table_like(self) -> bool {
        matches!(self, ObjectKind::Table | ObjectKind::ForeignTable | ObjectKind::Partition)
    }

    pub fn is_routine(self) -> bool {
        matches!(self, ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Aggregate)
    }

    /// Objects that belong to a table (and need `ObjectRef::table`).
    pub fn is_table_child(self) -> bool {
        matches!(
            self,
            ObjectKind::Column
                | ObjectKind::Index
                | ObjectKind::PrimaryKey
                | ObjectKind::UniqueKey
                | ObjectKind::CheckConstraint
                | ObjectKind::ForeignKey
                | ObjectKind::Trigger
                | ObjectKind::Partition
        )
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Database + schema pair identifying a namespace.
///
/// `schema` is the qualifier used in SQL text: the PostgreSQL schema, the MySQL
/// database, the SQLite attached database name ("main"). `database` is only
/// used where a server hosts several catalogs that cannot be referenced in one
/// session (PostgreSQL, SQL Server); it selects which session to use.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub struct SchemaRef {
    pub database: Option<String>,
    pub schema: Option<String>,
}

impl SchemaRef {
    pub fn new(database: Option<String>, schema: Option<String>) -> Self {
        SchemaRef { database, schema }
    }
    pub fn schema(schema: impl Into<String>) -> Self {
        SchemaRef { database: None, schema: Some(schema.into()) }
    }
}

/// Fully qualified reference to a database object.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjectRef {
    pub kind: ObjectKind,
    pub database: Option<String>,
    pub schema: Option<String>,
    /// Owning table for table children (columns, indexes, constraints, triggers, partitions).
    pub table: Option<String>,
    pub name: String,
    /// Disambiguator for overloaded routines: argument list `(integer, text)` or an OID.
    pub signature: Option<String>,
}

impl ObjectRef {
    pub fn new(kind: ObjectKind, schema: Option<String>, name: impl Into<String>) -> Self {
        ObjectRef { kind, database: None, schema, table: None, name: name.into(), signature: None }
    }

    pub fn table(schema: Option<String>, name: impl Into<String>) -> Self {
        Self::new(ObjectKind::Table, schema, name)
    }

    pub fn child(kind: ObjectKind, parent: &ObjectRef, name: impl Into<String>) -> Self {
        ObjectRef {
            kind,
            database: parent.database.clone(),
            schema: parent.schema.clone(),
            table: Some(parent.name.clone()),
            name: name.into(),
            signature: None,
        }
    }

    pub fn with_database(mut self, database: Option<String>) -> Self {
        self.database = database;
        self
    }

    pub fn with_signature(mut self, signature: impl Into<String>) -> Self {
        self.signature = Some(signature.into());
        self
    }

    pub fn schema_ref(&self) -> SchemaRef {
        SchemaRef { database: self.database.clone(), schema: self.schema.clone() }
    }

    /// For table children: reference to the owning table.
    pub fn parent_table(&self) -> Option<ObjectRef> {
        self.table.as_ref().map(|t| ObjectRef {
            kind: ObjectKind::Table,
            database: self.database.clone(),
            schema: self.schema.clone(),
            table: None,
            name: t.clone(),
            signature: None,
        })
    }

    /// Unquoted dotted path for display: `schema.table.name`.
    pub fn display_path(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if let Some(s) = &self.schema {
            parts.push(s);
        }
        if let Some(t) = &self.table {
            parts.push(t);
        }
        parts.push(&self.name);
        let mut s = parts.join(".");
        if let Some(sig) = &self.signature
            && sig.starts_with('(') {
                s.push_str(sig);
            }
        s
    }
}

impl fmt::Display for ObjectRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display_path())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DatabaseInfo {
    pub name: String,
    /// The database the session is connected to.
    pub is_current: bool,
    pub is_system: bool,
    pub owner: Option<String>,
    pub comment: Option<String>,
    pub encoding: Option<String>,
    pub size_bytes: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SchemaInfo {
    pub name: String,
    pub is_current: bool,
    pub is_system: bool,
    pub owner: Option<String>,
    pub comment: Option<String>,
}

/// Navigator entry for one object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectInfo {
    pub obj: ObjectRef,
    pub comment: Option<String>,
    /// Short extra text shown greyed next to the name, e.g. a column type
    /// (`integer NOT NULL`), a routine signature, an index definition.
    pub detail: Option<String>,
    pub is_system: bool,
    /// Estimated row count for tables.
    pub row_estimate: Option<i64>,
}

impl ObjectInfo {
    pub fn new(obj: ObjectRef) -> Self {
        ObjectInfo { obj, comment: None, detail: None, is_system: false, row_estimate: None }
    }
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
    pub fn with_comment(mut self, comment: Option<String>) -> Self {
        self.comment = comment.filter(|c| !c.is_empty());
        self
    }
}

/// Name/value pair shown in property sheets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Property {
    pub group: Option<String>,
    pub name: String,
    pub value: String,
}

impl Property {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Property { group: None, name: name.into(), value: value.into() }
    }
    pub fn grouped(group: impl Into<String>, name: impl Into<String>, value: impl Into<String>) -> Self {
        Property { group: Some(group.into()), name: name.into(), value: value.into() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DependencyDirection {
    /// The object depends on `target`.
    DependsOn,
    /// `target` depends on the object.
    UsedBy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    pub direction: DependencyDirection,
    pub target: ObjectRef,
    /// e.g. "normal", "auto", "foreign key", "view"
    pub kind: Option<String>,
}

/// Metadata search request ("search metadata" / navigator search).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataSearch {
    /// Pattern with `*`/`?` wildcards; plain text means "contains".
    pub pattern: String,
    pub kinds: Vec<ObjectKind>,
    /// Restrict to these schemas; empty = all visible schemas.
    pub schemas: Vec<String>,
    pub case_sensitive: bool,
    /// Also match object comments.
    pub search_comments: bool,
    pub limit: usize,
}

impl Default for MetadataSearch {
    fn default() -> Self {
        MetadataSearch {
            pattern: String::new(),
            kinds: vec![ObjectKind::Table, ObjectKind::View, ObjectKind::Column, ObjectKind::Function],
            schemas: Vec::new(),
            case_sensitive: false,
            search_comments: false,
            limit: 500,
        }
    }
}

/// Lightweight column description used for autocomplete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnBrief {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub is_primary_key: bool,
    pub comment: Option<String>,
}
