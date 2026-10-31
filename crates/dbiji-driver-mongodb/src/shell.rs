//! The MongoDB shell statement language: `db.collection.method(args)`,
//! `show collections`, `use db`, `db.runCommand({…})`.
//!
//! Parsing is pure (no I/O) so every shape is unit tested; the session
//! executes the resulting [`ShellCommand`].

use crate::js;
use mongodb::bson::{Bson, Document};

/// What a statement asks the server to do.
#[derive(Debug, Clone, PartialEq)]
pub enum ShellCommand {
    /// `db.coll.find(filter, projection).sort(…).skip(n).limit(n)`
    Find {
        db: Option<String>,
        coll: String,
        filter: Document,
        projection: Option<Document>,
        sort: Option<Document>,
        skip: Option<i64>,
        limit: Option<i64>,
    },
    /// `db.coll.findOne(filter, projection)`
    FindOne {
        db: Option<String>,
        coll: String,
        filter: Document,
        projection: Option<Document>,
    },
    /// `db.coll.aggregate([…])`
    Aggregate {
        db: Option<String>,
        coll: Option<String>,
        pipeline: Vec<Document>,
    },
    Count {
        db: Option<String>,
        coll: String,
        filter: Document,
    },
    EstimatedCount {
        db: Option<String>,
        coll: String,
    },
    Distinct {
        db: Option<String>,
        coll: String,
        field: String,
        filter: Document,
    },
    Insert {
        db: Option<String>,
        coll: String,
        docs: Vec<Document>,
    },
    Update {
        db: Option<String>,
        coll: String,
        filter: Document,
        update: Document,
        many: bool,
        upsert: bool,
    },
    Delete {
        db: Option<String>,
        coll: String,
        filter: Document,
        many: bool,
    },
    /// `findOneAndUpdate` / `findOneAndReplace` / `findOneAndDelete`.
    FindAndModify {
        db: Option<String>,
        coll: String,
        filter: Document,
        update: Option<Document>,
        delete: bool,
    },
    CreateCollection {
        db: Option<String>,
        coll: String,
        options: Document,
    },
    DropCollection {
        db: Option<String>,
        coll: String,
    },
    DropDatabase {
        db: Option<String>,
    },
    CreateIndex {
        db: Option<String>,
        coll: String,
        keys: Document,
        options: Document,
    },
    DropIndex {
        db: Option<String>,
        coll: String,
        name: String,
    },
    /// `db.coll.getIndexes()`
    ListIndexes {
        db: Option<String>,
        coll: String,
    },
    /// `db.coll.stats()` / `db.stats()`
    Stats {
        db: Option<String>,
        coll: Option<String>,
    },
    /// Any other command document, run on `db` (or `admin`).
    RunCommand {
        db: Option<String>,
        admin: bool,
        command: Document,
    },
    /// `show collections` / `show dbs` / `show databases` / `show tables`.
    Show {
        what: ShowWhat,
    },
    /// `use db`
    Use(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowWhat {
    Collections,
    Databases,
}

/// A parsed statement.
#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    pub command: ShellCommand,
    /// The command name as written (`find`, `insertOne`, `show collections`).
    pub method: String,
}

/// Parses one shell statement.
pub fn parse(stmt: &str) -> Result<Statement, String> {
    let text = stmt.trim().trim_end_matches(';').trim();
    if text.is_empty() {
        return Err("The statement is empty".into());
    }
    let lower = text.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("show ") {
        let what = rest.trim();
        return match what {
            "collections" | "tables" => Ok(Statement { command: ShellCommand::Show { what: ShowWhat::Collections }, method: "show collections".into() }),
            "dbs" | "databases" => Ok(Statement { command: ShellCommand::Show { what: ShowWhat::Databases }, method: "show databases".into() }),
            _ => Err(format!("show {what} is not supported; try `show collections` or `show databases`")),
        };
    }
    if lower.starts_with("use ") {
        let name = text[4..].trim().trim_end_matches(';').trim().to_string();
        if name.is_empty() {
            return Err("`use` needs a database name".into());
        }
        return Ok(Statement { command: ShellCommand::Use(name), method: "use".into() });
    }
    let chain = Chain::parse(text)?;
    chain.into_statement()
}

/// `db.<name|method(args)>` followed by `.method(args)` calls.
struct Chain {
    /// Database from a leading `db.getSiblingDB("x")`.
    db: Option<String>,
    collection: Option<String>,
    /// `(method, args)` calls in order.
    calls: Vec<(String, Vec<Bson>)>,
}

impl Chain {
    fn parse(text: &str) -> Result<Chain, String> {
        let mut p = Cursor { src: text, pos: 0 };
        p.skip_ws();
        let head = p.ident().ok_or_else(|| "A statement starts with `db.` (for example `db.users.find({})`)".to_string())?;
        if head != "db" {
            return Err(format!("'{head}' is not supported: statements start with `db.`"));
        }
        let mut chain = Chain { db: None, collection: None, calls: Vec::new() };
        loop {
            p.skip_ws();
            if p.pos >= p.src.len() {
                break;
            }
            if !p.eat('.') {
                return Err(format!("unexpected '{}'", p.src[p.pos..].chars().take(10).collect::<String>()));
            }
            p.skip_ws();
            let name = p.ident().ok_or_else(|| "a name is expected after '.'".to_string())?;
            p.skip_ws();
            if p.peek() == Some(b'(') {
                let args = p.call_args()?;
                match name.as_str() {
                    "getSiblingDB" | "getDB" => {
                        chain.db = Some(text_arg(&args, 0)?);
                    }
                    "getCollection" => {
                        chain.collection = Some(text_arg(&args, 0)?);
                    }
                    "getMongo" | "getSiblingMongo" => {}
                    _ => chain.calls.push((name, args)),
                }
            } else if chain.collection.is_none() {
                chain.collection = Some(name);
            } else {
                return Err(format!("'.{name}' must be a method call: '{name}(…)'"));
            }
        }
        Ok(chain)
    }

    fn into_statement(self) -> Result<Statement, String> {
        let Chain { db, collection, mut calls } = self;
        let coll = collection.clone();
        if calls.is_empty() {
            return Err("The statement does not call a method: try `db.<collection>.find({})`".into());
        }
        // `find(...)` / `findOne(...)` followed by sort/skip/limit/project.
        let (method, args) = calls.remove(0);
        let stmt = match method.as_str() {
            "find" | "findOne" => {
                let filter = doc_arg(&args, 0)?;
                let projection = match args.get(1) {
                    Some(Bson::Document(d)) if !d.is_empty() => Some(d.clone()),
                    _ => None,
                };
                let mut sort = None;
                let mut skip = None;
                let mut limit = None;
                let mut extra_projection = projection;
                for (name, a) in &calls {
                    match name.as_str() {
                        "sort" => sort = Some(doc_arg(a, 0)?),
                        "skip" => skip = Some(int_arg(a, 0)?),
                        "limit" => limit = Some(int_arg(a, 0)?),
                        "project" | "projection" => extra_projection = Some(doc_arg(a, 0)?),
                        other => return Err(format!("'.{other}' cannot follow {method}()")),
                    }
                }
                calls.clear();
                let coll = coll.clone().ok_or_else(|| format!("`{method}` needs a collection: db.<collection>.{method}(…)"))?;
                if method == "findOne" {
                    ShellCommand::FindOne { db, coll, filter, projection: extra_projection }
                } else {
                    ShellCommand::Find { db, coll, filter, projection: extra_projection, sort, skip, limit }
                }
            }
            "aggregate" => {
                let pipeline = array_of_docs(args.first())?;
                if calls.iter().any(|(n, _)| n != "explain") {
                    return Err("Only find() accepts sort / skip / limit".into());
                }
                ShellCommand::Aggregate { db, coll: coll.clone(), pipeline }
            }
            "count" | "countDocuments" => ShellCommand::Count { db, coll: coll.clone().ok_or("count needs a collection")?, filter: doc_arg(&args, 0)? },
            "estimatedDocumentCount" => ShellCommand::EstimatedCount { db, coll: coll.clone().ok_or("estimatedDocumentCount needs a collection")? },
            "distinct" => ShellCommand::Distinct { db, coll: coll.clone().ok_or("distinct needs a collection")?, field: text_arg(&args, 0)?, filter: doc_arg(&args, 1)? },
            "insert" | "insertOne" | "insertMany" => {
                let docs = match args.first() {
                    Some(Bson::Array(a)) => array_of_docs(Some(&Bson::Array(a.clone())))?,
                    Some(Bson::Document(d)) => vec![d.clone()],
                    _ => return Err("insert() needs a document or an array of documents".into()),
                };
                ShellCommand::Insert { db, coll: coll.clone().ok_or("insert needs a collection")?, docs }
            }
            "update" | "updateOne" | "updateMany" => {
                let many = method == "updateMany" || bool_named(&args, "multi").unwrap_or(false);
                ShellCommand::Update {
                    db,
                    coll: coll.clone().ok_or("update needs a collection")?,
                    filter: doc_arg(&args, 0)?,
                    update: doc_arg(&args, 1)?,
                    many,
                    upsert: bool_named(&args, "upsert").unwrap_or(false),
                }
            }
            "replaceOne" => ShellCommand::Update {
                db,
                coll: coll.clone().ok_or("replaceOne needs a collection")?,
                filter: doc_arg(&args, 0)?,
                update: doc_arg(&args, 1)?,
                many: false,
                upsert: bool_named(&args, "upsert").unwrap_or(false),
            },
            "deleteOne" | "deleteMany" | "remove" => {
                ShellCommand::Delete { db, coll: coll.clone().ok_or("delete needs a collection")?, filter: doc_arg(&args, 0)?, many: method != "deleteOne" }
            }
            "findOneAndUpdate" | "findOneAndReplace" => ShellCommand::FindAndModify {
                db,
                coll: coll.clone().ok_or("findOneAndUpdate needs a collection")?,
                filter: doc_arg(&args, 0)?,
                update: Some(doc_arg(&args, 1)?),
                delete: false,
            },
            "findOneAndDelete" => {
                ShellCommand::FindAndModify { db, coll: coll.clone().ok_or("findOneAndDelete needs a collection")?, filter: doc_arg(&args, 0)?, update: None, delete: true }
            }
            "createIndex" | "createIndexes" => ShellCommand::CreateIndex {
                db,
                coll: coll.clone().ok_or("createIndex needs a collection")?,
                keys: doc_arg(&args, 0)?,
                options: doc_arg(&args, 1).unwrap_or_default(),
            },
            "dropIndex" | "dropIndexes" => ShellCommand::DropIndex {
                db,
                coll: coll.clone().ok_or("dropIndex needs a collection")?,
                name: text_arg(&args, 0).or_else(|_| doc_arg(&args, 0).map(|d| d.get_str("name").unwrap_or("*").to_string()))?,
            },
            "getIndexes" | "listIndexes" => ShellCommand::ListIndexes { db, coll: coll.clone().ok_or("getIndexes needs a collection")? },
            "drop" => ShellCommand::DropCollection { db, coll: coll.clone().ok_or("drop needs a collection: db.<collection>.drop()")? },
            "createCollection" | "create" => ShellCommand::CreateCollection {
                db,
                coll: text_arg(&args, 0).or_else(|_| coll.clone().ok_or_else(|| "createCollection needs a name".to_string()))?,
                options: doc_arg(&args, 1).unwrap_or_default(),
            },
            "stats" => ShellCommand::Stats { db, coll: coll.clone() },
            "runCommand" | "adminCommand" => ShellCommand::RunCommand { db, admin: method == "adminCommand", command: doc_arg(&args, 0)? },
            "dropDatabase" => ShellCommand::DropDatabase { db },
            "explain" => ShellCommand::RunCommand {
                db,
                admin: false,
                command: {
                    let mut c = mongodb::bson::doc! {"explain": args.first().cloned().unwrap_or(Bson::Document(Document::new()))};
                    c.insert("verbosity", "executionStats");
                    c
                },
            },
            other => {
                return Err(format!("`{other}()` is not supported by the MongoDB shell of DBiji"));
            }
        };
        // A statement on `db.<method>` has no collection: its database is the session's.
        if let Some(extra) = calls.first() {
            return Err(format!("'.{}' cannot follow {method}()", extra.0));
        }
        Ok(Statement { method, command: stmt })
    }
}

/// A tiny cursor over the statement text.
struct Cursor<'a> {
    src: &'a str,
    pos: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c as u8) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn ident(&mut self) -> Option<String> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
                self.pos += 1;
            } else {
                break;
            }
        }
        (start < self.pos).then(|| self.src[start..self.pos].to_string())
    }

    /// `( arg, arg, … )` with nested brackets, strings and comments.
    fn call_args(&mut self) -> Result<Vec<Bson>, String> {
        let open = self.pos;
        if !self.eat('(') {
            return Err("'(' is expected".into());
        }
        let start = self.pos;
        let mut depth = 1i32;
        let mut quote: Option<u8> = None;
        let mut escaped = false;
        while let Some(c) = self.peek() {
            if let Some(q) = quote {
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
                self.pos += 1;
                continue;
            }
            match c {
                b'\'' | b'"' | b'`' => quote = Some(c),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        let inner = &self.src[start..self.pos];
                        self.pos += 1;
                        return split_args(inner);
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
        let _ = quote;
        Err(format!("')' is missing for the call at byte {open}"))
    }
}

/// Split the argument text at top-level commas and parse each part.
fn split_args(inner: &str) -> Result<Vec<Bson>, String> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    for (i, c) in inner.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == char::from(q) {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => quote = Some(c as u8),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&inner[start..]);
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        if p.trim().is_empty() {
            continue;
        }
        out.push(js::parse_value(p).map_err(|e| format!("{e} in `{}`", p.trim()))?);
    }
    Ok(out)
}

fn doc_arg(args: &[Bson], i: usize) -> Result<Document, String> {
    match args.get(i) {
        None => Ok(Document::new()),
        Some(Bson::Document(d)) => Ok(d.clone()),
        Some(Bson::Null) => Ok(Document::new()),
        Some(other) => Err(format!("argument {} must be a document, found {}", i + 1, other)),
    }
}

fn text_arg(args: &[Bson], i: usize) -> Result<String, String> {
    match args.get(i) {
        Some(Bson::String(s)) => Ok(s.clone()),
        Some(Bson::Document(d)) => d.get_str("name").map(str::to_string).map_err(|_| format!("argument {} must be a name", i + 1)),
        _ => Err(format!("argument {} must be a name", i + 1)),
    }
}

fn int_arg(args: &[Bson], i: usize) -> Result<i64, String> {
    match args.get(i) {
        Some(Bson::Int32(n)) => Ok(*n as i64),
        Some(Bson::Int64(n)) => Ok(*n),
        Some(Bson::Double(n)) => Ok(*n as i64),
        _ => Err(format!("argument {} must be a number", i + 1)),
    }
}

fn bool_named(args: &[Bson], key: &str) -> Option<bool> {
    args.iter().find_map(|a| match a {
        Bson::Document(d) => d.get_bool(key).ok(),
        _ => None,
    })
}

fn array_of_docs(v: Option<&Bson>) -> Result<Vec<Document>, String> {
    match v {
        Some(Bson::Array(a)) => a
            .iter()
            .map(|item| match item {
                Bson::Document(d) => Ok(d.clone()),
                other => Err(format!("a pipeline stage must be a document, found {other}")),
            })
            .collect(),
        None | Some(Bson::Null) => Ok(Vec::new()),
        Some(other) => Err(format!("a pipeline must be an array, found {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_with_filter_sort_limit() {
        let s = parse(r#"db.users.find({age: {$gt: 30}}, {name: 1}).sort({age: -1}).limit(10)"#).unwrap();
        assert_eq!(s.method, "find");
        match s.command {
            ShellCommand::Find { coll, filter, projection, sort, limit, .. } => {
                assert_eq!(coll, "users");
                assert!(filter.contains_key("age"));
                assert!(projection.is_some());
                assert!(sort.is_some());
                assert_eq!(limit, Some(10));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn inserts_updates_and_deletes() {
        assert!(matches!(parse("db.t.insertOne({a: 1})").unwrap().command, ShellCommand::Insert { ref docs, .. } if docs.len() == 1));
        assert!(matches!(parse("db.t.insert([{a: 1}, {a: 2}])").unwrap().command, ShellCommand::Insert { ref docs, .. } if docs.len() == 2));
        assert!(matches!(parse("db.t.updateMany({a: 1}, {$set: {b: 2}})").unwrap().command, ShellCommand::Update { many: true, .. }));
        assert!(matches!(parse("db.t.deleteOne({a: 1})").unwrap().command, ShellCommand::Delete { many: false, .. }));
        assert!(matches!(parse("db.t.deleteMany({})").unwrap().command, ShellCommand::Delete { many: true, .. }));
        assert!(matches!(parse("db.t.findOneAndUpdate({a: 1}, {$inc: {n: 1}})").unwrap().command, ShellCommand::FindAndModify { delete: false, .. }));
    }

    #[test]
    fn aggregation_and_commands() {
        assert!(matches!(
            parse("db.orders.aggregate([{$match: {a: 1}}, {$group: {_id: '$a', n: {$sum: 1}}}])").unwrap().command,
            ShellCommand::Aggregate { ref pipeline, .. } if pipeline.len() == 2
        ));
        assert!(matches!(parse(r#"db.runCommand({ping: 1})"#).unwrap().command, ShellCommand::RunCommand { admin: false, .. }));
        assert!(matches!(parse(r#"db.adminCommand({listDatabases: 1})"#).unwrap().command, ShellCommand::RunCommand { admin: true, .. }));
    }

    #[test]
    fn show_use_and_names() {
        assert!(matches!(parse("show collections").unwrap().command, ShellCommand::Show { what: ShowWhat::Collections }));
        assert!(matches!(parse("show dbs;").unwrap().command, ShellCommand::Show { what: ShowWhat::Databases }));
        assert_eq!(parse("use shop").unwrap().command, ShellCommand::Use("shop".into()));
        assert!(matches!(parse(r#"db.getCollection("my-coll").find({})"#).unwrap().command, ShellCommand::Find { ref coll, .. } if coll == "my-coll"));
        assert!(matches!(parse(r#"db.getSiblingDB("other").t.find({})"#).unwrap().command, ShellCommand::Find { ref db, .. } if db.as_deref() == Some("other")));
    }

    #[test]
    fn indexes_and_ddl() {
        assert!(matches!(parse("db.t.createIndex({a: 1}, {unique: true})").unwrap().command, ShellCommand::CreateIndex { .. }));
        assert!(matches!(parse("db.t.getIndexes()").unwrap().command, ShellCommand::ListIndexes { .. }));
        assert!(matches!(parse("db.t.drop()").unwrap().command, ShellCommand::DropCollection { .. }));
        assert!(matches!(parse("db.dropDatabase()").unwrap().command, ShellCommand::DropDatabase { .. }));
        assert!(matches!(parse(r#"db.createCollection("c", {capped: true})"#).unwrap().command, ShellCommand::CreateCollection { .. }));
    }

    #[test]
    fn unsupported_statements_say_so() {
        let e = parse("db.t.mapReduce()").unwrap_err();
        assert!(e.contains("not supported"), "{e}");
        assert!(parse("SELECT * FROM t").unwrap_err().contains("db."));
        assert!(parse("db.t.find({").is_err());
    }
}
