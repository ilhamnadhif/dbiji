//! Prompts of the SQL assistant: the system prompt, task specific user
//! messages, the schema context and extraction of SQL from answers.
//!
//! The system prompt (instructions plus schema) is stable for a connection
//! and is sent as a cached block; the question goes into the messages.

use crate::types::ChatMessage;
use dbiji_core::DbKind;
use dbiji_core::ddl::{ForeignKeyDef, SchemaSnapshot, TableDef};
use dbiji_core::i18n::Locale;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Default character budget of the schema context.
pub const DEFAULT_CONTEXT_BUDGET: usize = 60_000;

fn dialect_notes(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Postgres => {
            "PostgreSQL: quote identifiers with double quotes only when needed, use LIMIT/OFFSET, ILIKE for case-insensitive matching, `::` casts, date_trunc / interval arithmetic, and RETURNING where useful."
        }
        DbKind::CockroachDb => {
            "CockroachDB (PostgreSQL wire compatible): prefer standard PostgreSQL syntax, avoid PostgreSQL extensions CockroachDB lacks, use LIMIT, and note that sequences and SERIAL behave differently (unique_rowid)."
        }
        DbKind::MySql => {
            "MySQL: quote identifiers with backticks only when needed, use LIMIT, DATE_FORMAT / DATE_ADD for dates, and remember that GROUP BY must follow ONLY_FULL_GROUP_BY rules."
        }
        DbKind::MariaDb => {
            "MariaDB: quote identifiers with backticks only when needed, use LIMIT, and prefer syntax common to MySQL and MariaDB unless a MariaDB feature is clearly better."
        }
        DbKind::Sqlite => "SQLite: dynamic typing, LIMIT/OFFSET, strftime / date() for dates, no RIGHT/FULL JOIN on old versions, and very limited ALTER TABLE.",
        DbKind::SqlServer => {
            "SQL Server (T-SQL): quote identifiers with [brackets] only when needed, use TOP or OFFSET ... FETCH, DATEADD / DATEDIFF / FORMAT for dates, and GETDATE() for the current time."
        }
        DbKind::ClickHouse => {
            "ClickHouse: analytical column store; use LIMIT, toDate / toStartOfInterval for time buckets, avoid row-by-row UPDATE/DELETE (they are mutations), and prefer aggregate functions such as uniq / quantile."
        }
        DbKind::Oracle => {
            "Oracle: quote identifiers with double quotes only when needed (unquoted names are upper case), use FETCH FIRST n ROWS ONLY (12c+) instead of LIMIT, NVL / DECODE / TO_CHAR / TO_DATE / ADD_MONTHS / TRUNC for dates, SYSDATE / SYSTIMESTAMP, DUAL for expressions, and no semicolon inside a single statement sent to the server; PL/SQL blocks end with END;."
        }
        DbKind::Cassandra => {
            "Cassandra (CQL): no joins, subqueries or GROUP BY on non-key columns; WHERE must restrict the partition key (or use ALLOW FILTERING knowingly), ORDER BY only on clustering columns, use LIMIT, and model queries per table. Answer with CQL in ```cql fences."
        }
        DbKind::Redis => {
            "Redis: answer with Redis commands, one per line, in ```redis fences (for example HGETALL user:1, ZRANGE board 0 9 WITHSCORES, SCAN 0 MATCH user:* COUNT 100). Never suggest KEYS * or FLUSHALL / FLUSHDB unless explicitly asked."
        }
        DbKind::MongoDb => {
            "MongoDB: answer with mongosh syntax in ```javascript fences, using db.<collection>.find({...}, {projection}).sort({...}).limit(n), aggregate([...]) pipelines, countDocuments, distinct, insertOne/insertMany, updateOne/updateMany with $set, deleteOne/deleteMany. Use one statement per query and relaxed JSON (ObjectId(\"...\"), ISODate(\"...\"))."
        }
        DbKind::Elasticsearch => {
            "Elasticsearch / OpenSearch: prefer the SQL interface (SELECT ... FROM \"index-name\" WHERE ... LIMIT n; quote index names with hyphens or wildcards); for full-text relevance use MATCH() / QUERY() or, when SQL is not enough, a request like GET /index/_search followed by the JSON body."
        }
        DbKind::Neo4j => {
            "Neo4j: answer with Cypher in ```cypher fences: MATCH (n:Label {prop: $v})-[r:TYPE]->(m) WHERE ... RETURN n.prop, m LIMIT 25; use parameters only when given, OPTIONAL MATCH for optional patterns, MERGE for upserts and avoid unbounded variable-length paths."
        }
        DbKind::Generic => "Standard SQL: avoid vendor specific syntax.",
    }
}

fn language_name(l: Locale) -> &'static str {
    match l {
        Locale::En => "English",
        Locale::Id => "Indonesian (Bahasa Indonesia)",
    }
}

/// System prompt for the SQL assistant of a connection: expert in the
/// dialect and version, answers in the UI language, SQL in ```sql fences,
/// only objects from the schema, read-only by default, warnings before
/// destructive statements.
pub fn system_prompt(kind: DbKind, server_version: &str, schema_context: &str, ui_language: Locale) -> String {
    let product = kind.display_name();
    let version = server_version.trim();
    let target = if version.is_empty() { product.to_string() } else { format!("{product} {version}") };
    let mut s = String::with_capacity(schema_context.len() + 2048);
    s.push_str(&format!(
        "You are the SQL assistant built into DBiji, a desktop database client. You are an expert in {target} and help the user write, understand, fix and optimize SQL for this database.\n\n"
    ));
    s.push_str("Guidelines:\n");
    s.push_str(&format!("- Write SQL that runs on {target}. {}\n", dialect_notes(kind)));
    s.push_str("- Use only the tables, views, columns and routines listed in the schema below. If the request needs an object or column that is not listed, say so plainly and do not invent names; ask a short clarifying question when the request is ambiguous.\n");
    s.push_str("- Put every SQL statement in a fenced code block that starts with ```sql. Keep explanations short and outside the code blocks.\n");
    s.push_str("- Prefer read-only queries. Add a row limit when a query could return many rows, and qualify columns when several tables are joined.\n");
    s.push_str("- Before any statement that changes data or schema (INSERT, UPDATE, DELETE, MERGE, TRUNCATE, DROP, ALTER, CREATE, GRANT, REVOKE), warn clearly about its effect. Always include a WHERE clause in UPDATE and DELETE, and suggest running such statements in a transaction or after a backup.\n");
    s.push_str("- You cannot see or execute anything: never claim results, and say when an answer depends on the data.\n");
    s.push_str(&format!("- Answer in {}. Keep SQL keywords, identifiers and code in their original form.\n\n", language_name(ui_language)));
    s.push_str("Database schema (structure only, no data):\n");
    if schema_context.trim().is_empty() {
        s.push_str("(no schema information is available)\n");
    } else {
        s.push_str(schema_context.trim_end());
        s.push('\n');
    }
    s
}

/// A code fence longer than any backtick run inside `content`.
fn fenced(lang: &str, content: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in content.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}{lang}\n{}\n{fence}", content.trim_end())
}

/// Ask for a query answering a natural language question.
pub fn generate_sql(question: &str) -> ChatMessage {
    ChatMessage::user(format!(
        "Write a SQL query for this request. Give the query in a ```sql block, then explain briefly how it works and any assumption you made.\n\nRequest: {}",
        question.trim()
    ))
}

/// Ask for an explanation of a query.
pub fn explain_query(sql: &str) -> ChatMessage {
    ChatMessage::user(format!(
        "Explain what this SQL statement does, step by step and in plain language: the tables involved, joins, filters, grouping and the shape of the result. Point out anything surprising, risky or likely to be slow.\n\n{}",
        fenced("sql", sql)
    ))
}

/// Ask for the cause of an error and a corrected statement.
pub fn fix_error(sql: &str, error_text: &str) -> ChatMessage {
    ChatMessage::user(format!(
        "This SQL statement failed. Explain the cause of the error and give a corrected statement in a ```sql block.\n\nStatement:\n{}\n\nError message from the database:\n{}",
        fenced("sql", sql),
        fenced("", error_text)
    ))
}

/// Ask how to make a query faster, optionally with its execution plan.
pub fn optimize_query(sql: &str, plan_text: Option<&str>) -> ChatMessage {
    let mut s = format!(
        "Suggest how to make this query faster: rewrites, indexes (as CREATE INDEX statements) or schema changes, ordered by expected benefit, with the reason for each. Keep the result of the query the same.\n\nQuery:\n{}",
        fenced("sql", sql)
    );
    if let Some(p) = plan_text.map(str::trim).filter(|p| !p.is_empty()) {
        s.push_str(&format!("\n\nExecution plan:\n{}", fenced("", p)));
    }
    ChatMessage::user(s)
}

/// Ask for a description of a database object from its DDL.
pub fn describe_object(ddl: &str) -> ChatMessage {
    ChatMessage::user(format!(
        "Describe this database object from its definition: what it is for, the meaning of its important columns or parameters, its relationships and constraints, and anything worth knowing when using it.\n\n{}",
        fenced("sql", ddl)
    ))
}

/// A free chat message.
pub fn chat(text: &str) -> ChatMessage {
    ChatMessage::user(text.trim())
}

// --------------------------------------------------------- schema context

const STOP_WORDS: &[&str] = &[
    "a",
    "an",
    "and",
    "are",
    "as",
    "at",
    "be",
    "by",
    "for",
    "from",
    "get",
    "give",
    "how",
    "i",
    "in",
    "is",
    "it",
    "list",
    "me",
    "many",
    "much",
    "of",
    "on",
    "or",
    "per",
    "please",
    "query",
    "select",
    "show",
    "sql",
    "that",
    "the",
    "their",
    "them",
    "there",
    "this",
    "to",
    "what",
    "when",
    "where",
    "which",
    "who",
    "with",
    "all",
    "each",
    "every",
    "find",
    "count",
    "number",
    "total",
    "table",
    "tables",
    "column",
    "columns",
    "data",
    "yang",
    "dan",
    "di",
    "ke",
    "dari",
    "untuk",
    "dengan",
    "tampilkan",
    "semua",
    "berapa",
    "jumlah",
    "setiap",
    "per",
    "pada",
    "ada",
    "apa",
    "siapa",
    "mana",
    "buat",
    "berikan",
    "daftar",
    "tabel",
    "kolom",
];

fn stem(w: &str) -> String {
    let w = w.to_lowercase();
    if w.len() > 4 && w.ends_with("ies") {
        format!("{}y", &w[..w.len() - 3])
    } else if w.len() > 4 && (w.ends_with("ses") || w.ends_with("xes")) {
        w[..w.len() - 2].to_string()
    } else if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") && !w.ends_with("us") && !w.ends_with("is") {
        w[..w.len() - 1].to_string()
    } else {
        w
    }
}

/// Split an identifier or text into lower case, stemmed words
/// (`OrderItems`, `order_items` -> `order`, `item`).
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if c.is_uppercase() && prev_lower && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = c.is_lowercase() || c.is_ascii_digit();
            cur.push(c);
        } else {
            prev_lower = false;
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.into_iter().map(|w| stem(&w)).filter(|w| !w.is_empty()).collect()
}

fn short(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}\u{2026}", &s[..i]),
        None => s,
    }
}

struct Entry {
    /// Text block (table line plus its foreign key lines).
    text: String,
    /// Display name for the omission list.
    name: String,
    score: u32,
    order: usize,
}

/// Builds the compact schema description put into the system prompt:
///
/// ```text
/// table public.users (id integer PK, email varchar(255) NOT NULL UNIQUE, ...) -- Registered users
///   orders.user_id -> users.id
/// view public.active_users (id integer, email varchar(255))
/// function public.order_total(order_id integer) returns numeric
/// ```
///
/// Tables are ranked by relevance to the question (overlap of words in table
/// and column names) and the text is cut to a character budget. Data rows
/// are never included.
#[derive(Debug, Clone)]
pub struct SchemaContextBuilder<'a> {
    snapshots: Vec<&'a SchemaSnapshot>,
    question: Option<String>,
    budget: usize,
    include_routines: bool,
    include_comments: bool,
}

impl Default for SchemaContextBuilder<'_> {
    fn default() -> Self {
        SchemaContextBuilder { snapshots: Vec::new(), question: None, budget: DEFAULT_CONTEXT_BUDGET, include_routines: true, include_comments: true }
    }
}

/// A built schema context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaContext {
    /// Text for [`system_prompt`].
    pub text: String,
    /// Objects described.
    pub included: usize,
    /// Objects left out because of the budget.
    pub omitted: Vec<String>,
}

impl<'a> SchemaContextBuilder<'a> {
    /// Empty builder with the default budget.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a schema.
    pub fn snapshot(mut self, s: &'a SchemaSnapshot) -> Self {
        self.snapshots.push(s);
        self
    }

    /// Add several schemas.
    pub fn snapshots(mut self, s: impl IntoIterator<Item = &'a SchemaSnapshot>) -> Self {
        self.snapshots.extend(s);
        self
    }

    /// Rank objects by relevance to this question.
    pub fn question(mut self, q: impl Into<String>) -> Self {
        self.question = Some(q.into());
        self
    }

    /// Maximum length of the text in characters (default [`DEFAULT_CONTEXT_BUDGET`]).
    pub fn budget(mut self, chars: usize) -> Self {
        self.budget = chars;
        self
    }

    /// Include functions and procedures (default true).
    pub fn routines(mut self, yes: bool) -> Self {
        self.include_routines = yes;
        self
    }

    /// Include object comments (default true).
    pub fn comments(mut self, yes: bool) -> Self {
        self.include_comments = yes;
        self
    }

    /// The context text.
    pub fn build(&self) -> String {
        self.build_with_stats().text
    }

    fn qualified(schema: &Option<String>, name: &str) -> String {
        match schema.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => format!("{s}.{name}"),
            None => name.to_string(),
        }
    }

    fn table_line(&self, t: &TableDef, schema: &Option<String>) -> String {
        let single_unique: BTreeSet<&str> = t.unique_keys.iter().filter(|k| k.columns.len() == 1).map(|k| k.columns[0].as_str()).collect();
        let single_pk: Option<&str> = t.primary_key.as_ref().filter(|k| k.columns.len() == 1).map(|k| k.columns[0].as_str());
        let cols: Vec<String> = t
            .columns
            .iter()
            .map(|c| {
                let mut s = format!("{} {}", c.name, c.data_type);
                if single_pk == Some(c.name.as_str()) {
                    s.push_str(" PK");
                } else if !c.nullable {
                    s.push_str(" NOT NULL");
                }
                if single_unique.contains(c.name.as_str()) {
                    s.push_str(" UNIQUE");
                }
                if self.include_comments
                    && let Some(cm) = c.comment.as_deref().filter(|x| !x.trim().is_empty())
                {
                    s.push_str(&format!(" /* {} */", short(cm, 80).replace("*/", "* /")));
                }
                s
            })
            .collect();
        let kind = if t.table_kind == dbiji_core::ddl::TableKind::ForeignTable { "foreign table" } else { "table" };
        let mut line = format!("{kind} {} ({})", Self::qualified(schema, &t.name), cols.join(", "));
        if let Some(pk) = t.primary_key.as_ref().filter(|k| k.columns.len() > 1) {
            line.push_str(&format!(" PRIMARY KEY ({})", pk.columns.join(", ")));
        }
        if self.include_comments
            && let Some(c) = t.comment.as_deref().filter(|x| !x.trim().is_empty())
        {
            line.push_str(&format!(" -- {}", short(c, 160)));
        }
        line
    }

    fn fk_line(t: &TableDef, schema: &Option<String>, fk: &ForeignKeyDef) -> String {
        let cols = |v: &[String]| if v.len() == 1 { v[0].clone() } else { format!("({})", v.join(", ")) };
        let target = match fk.ref_schema.as_deref().filter(|s| Some(*s) != schema.as_deref() && !s.is_empty()) {
            Some(rs) => format!("{rs}.{}", fk.ref_table),
            None => fk.ref_table.clone(),
        };
        format!("  {}.{} -> {}.{}", t.name, cols(&fk.columns), target, cols(&fk.ref_columns))
    }

    /// Text plus what was included and left out.
    pub fn build_with_stats(&self) -> SchemaContext {
        let q_words: BTreeSet<String> = self.question.as_deref().map(words).unwrap_or_default().into_iter().filter(|w| !STOP_WORDS.contains(&w.as_str()) && w.len() > 1).collect();
        let mut tables: Vec<Entry> = Vec::new();
        let mut others: Vec<Entry> = Vec::new();
        // table name (lower) -> index into `tables`, for foreign key neighbours
        let mut by_name: HashMap<String, usize> = HashMap::new();
        let mut links: Vec<(String, String)> = Vec::new();
        let mut order = 0usize;
        for snap in &self.snapshots {
            for t in &snap.tables {
                let mut text = self.table_line(t, &snap.schema);
                for fk in &t.foreign_keys {
                    text.push('\n');
                    text.push_str(&Self::fk_line(t, &snap.schema, fk));
                    links.push((t.name.to_lowercase(), fk.ref_table.to_lowercase()));
                }
                let name_words: BTreeSet<String> = words(&t.name).into_iter().collect();
                let col_words: BTreeSet<String> = t.columns.iter().flat_map(|c| words(&c.name)).collect();
                let full = stem(&t.name);
                let mut score = 0;
                for w in &q_words {
                    if *w == full {
                        score += 10;
                    } else if name_words.contains(w) {
                        score += 4;
                    }
                    if col_words.contains(w) {
                        score += 1;
                    }
                    if self.include_comments && t.comment.as_deref().is_some_and(|c| words(c).contains(w)) {
                        score += 1;
                    }
                }
                by_name.insert(t.name.to_lowercase(), tables.len());
                tables.push(Entry { text, name: Self::qualified(&snap.schema, &t.name), score, order });
                order += 1;
            }
            for v in &snap.views {
                let kind = if v.materialized { "materialized view" } else { "view" };
                let mut text = format!("{kind} {}", Self::qualified(&snap.schema, &v.name));
                if !v.columns.is_empty() {
                    let cols: Vec<String> = v.columns.iter().map(|c| format!("{} {}", c.name, c.data_type)).collect();
                    text.push_str(&format!(" ({})", cols.join(", ")));
                }
                if self.include_comments
                    && let Some(c) = v.comment.as_deref().filter(|x| !x.trim().is_empty())
                {
                    text.push_str(&format!(" -- {}", short(c, 160)));
                }
                let nw: BTreeSet<String> = words(&v.name).into_iter().chain(v.columns.iter().flat_map(|c| words(&c.name))).collect();
                let score = q_words.iter().filter(|w| nw.contains(*w)).count() as u32 * 2;
                others.push(Entry { text, name: Self::qualified(&snap.schema, &v.name), score, order });
                order += 1;
            }
            for ty in &snap.types {
                let text = match ty.category.to_ascii_lowercase().as_str() {
                    "enum" => format!(
                        "enum {} ({})",
                        Self::qualified(&snap.schema, &ty.name),
                        ty.elements.iter().map(|e| format!("'{}'", e.replace('\'', "''"))).collect::<Vec<_>>().join(", ")
                    ),
                    cat => format!("{} {} ({})", if cat.is_empty() { "type" } else { cat }, Self::qualified(&snap.schema, &ty.name), ty.elements.join(", ")),
                };
                others.push(Entry { text, name: Self::qualified(&snap.schema, &ty.name), score: 0, order });
                order += 1;
            }
            if self.include_routines {
                for r in &snap.routines {
                    let kind = match r.kind {
                        Some(dbiji_core::ObjectKind::Procedure) => "procedure",
                        Some(dbiji_core::ObjectKind::Aggregate) => "aggregate",
                        _ => "function",
                    };
                    let mut text = format!("{kind} {}({})", Self::qualified(&snap.schema, &r.name), short(&r.arguments, 200));
                    if let Some(ret) = r.returns.as_deref().filter(|x| !x.is_empty()) {
                        text.push_str(&format!(" returns {ret}"));
                    }
                    let score = q_words.iter().filter(|w| words(&r.name).contains(w)).count() as u32 * 2;
                    others.push(Entry { text, name: Self::qualified(&snap.schema, &r.name), score, order });
                    order += 1;
                }
            }
        }
        // tables related to strongly matching tables help with joins
        let strong: BTreeSet<usize> = tables.iter().enumerate().filter(|(_, e)| e.score >= 4).map(|(i, _)| i).collect();
        let mut bonus: BTreeMap<usize, u32> = BTreeMap::new();
        for (a, b) in &links {
            if let (Some(&ia), Some(&ib)) = (by_name.get(a), by_name.get(b)) {
                if strong.contains(&ia) && !strong.contains(&ib) {
                    *bonus.entry(ib).or_default() += 2;
                }
                if strong.contains(&ib) && !strong.contains(&ia) {
                    *bonus.entry(ia).or_default() += 2;
                }
            }
        }
        for (i, b) in bonus {
            tables[i].score += b;
        }
        tables.sort_by(|a, b| b.score.cmp(&a.score).then(a.order.cmp(&b.order)));
        others.sort_by(|a, b| b.score.cmp(&a.score).then(a.order.cmp(&b.order)));

        let reserve = 300.min(self.budget / 4);
        let limit = self.budget.saturating_sub(reserve);
        let mut text = String::new();
        let mut included = 0;
        let mut omitted = Vec::new();
        for e in tables.into_iter().chain(others) {
            let need = e.text.chars().count() + 1;
            if text.chars().count() + need <= limit {
                text.push_str(&e.text);
                text.push('\n');
                included += 1;
            } else {
                omitted.push(e.name);
            }
        }
        if !omitted.is_empty() {
            let mut note = format!("-- {} more objects not shown:", omitted.len());
            for name in &omitted {
                let piece = format!(" {name},");
                if text.chars().count() + note.chars().count() + piece.chars().count() + 2 > self.budget {
                    note.push_str(" \u{2026}");
                    break;
                }
                note.push_str(&piece);
            }
            let note = note.trim_end_matches(',').to_string();
            if text.chars().count() + note.chars().count() < self.budget {
                text.push_str(&note);
                text.push('\n');
            }
        }
        SchemaContext { text, included, omitted }
    }
}

// ------------------------------------------------------------ extraction

const SQL_TAGS: &[&str] = &[
    "sql",
    "postgresql",
    "postgres",
    "pgsql",
    "plpgsql",
    "mysql",
    "mariadb",
    "sqlite",
    "tsql",
    "t-sql",
    "mssql",
    "plsql",
    "oracle",
    "clickhouse",
    "cql",
    "redis",
    "javascript",
    "js",
    "mongodb",
    "http",
];

/// SQL code blocks of an answer: the contents of ```sql fences (and other
/// SQL dialect tags), or, when there are none, of untagged fences. An
/// unterminated last fence (streaming) is included.
pub fn extract_sql_blocks(text: &str) -> Vec<String> {
    let mut tagged = Vec::new();
    let mut plain = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let fence_char = match trimmed.chars().next() {
            Some(c @ ('`' | '~')) => c,
            _ => continue,
        };
        let fence_len = trimmed.chars().take_while(|c| *c == fence_char).count();
        if fence_len < 3 {
            continue;
        }
        let info = trimmed[fence_len * fence_char.len_utf8()..].trim();
        // single line block: ```sql SELECT 1```
        if let Some(inner) = info.strip_suffix(&fence_char.to_string().repeat(fence_len)) {
            let mut parts = inner.trim().splitn(2, char::is_whitespace);
            let tag = parts.next().unwrap_or("").to_lowercase();
            let body = parts.next().unwrap_or("").trim().to_string();
            if SQL_TAGS.contains(&tag.as_str()) && !body.is_empty() {
                tagged.push(body);
            }
            continue;
        }
        let tag = info.split_whitespace().next().unwrap_or("").to_lowercase();
        let mut body = Vec::new();
        for l in lines.by_ref() {
            let t = l.trim();
            if t.chars().count() >= fence_len && t.chars().all(|c| c == fence_char) {
                break;
            }
            body.push(l);
        }
        let content = body.join("\n").trim().to_string();
        if content.is_empty() {
            continue;
        }
        if SQL_TAGS.contains(&tag.as_str()) {
            tagged.push(content);
        } else if tag.is_empty() {
            plain.push(content);
        }
    }
    if tagged.is_empty() { plain } else { tagged }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::ddl::{ColumnDef, KeyDef, RoutineDef, TypeDef, ViewDef};

    fn snapshot() -> SchemaSnapshot {
        let s = Some("public".to_string());
        let mut users = TableDef::new(s.clone(), "users");
        users.columns = vec![
            ColumnDef::new("id", "integer").not_null(),
            ColumnDef::new("email", "varchar(255)").not_null(),
            ColumnDef { comment: Some("Display name".into()), ..ColumnDef::new("full_name", "text") },
        ];
        users.primary_key = Some(KeyDef { name: None, columns: vec!["id".into()] });
        users.unique_keys = vec![KeyDef { name: None, columns: vec!["email".into()] }];
        users.comment = Some("Registered users".into());
        let mut orders = TableDef::new(s.clone(), "orders");
        orders.columns = vec![ColumnDef::new("id", "bigint").not_null(), ColumnDef::new("user_id", "integer").not_null(), ColumnDef::new("total", "numeric(12,2)")];
        orders.primary_key = Some(KeyDef { name: None, columns: vec!["id".into()] });
        orders.foreign_keys =
            vec![ForeignKeyDef { columns: vec!["user_id".into()], ref_schema: s.clone(), ref_table: "users".into(), ref_columns: vec!["id".into()], ..Default::default() }];
        let mut items = TableDef::new(s.clone(), "order_items");
        items.columns = vec![ColumnDef::new("order_id", "bigint").not_null(), ColumnDef::new("line", "int").not_null(), ColumnDef::new("sku", "text")];
        items.primary_key = Some(KeyDef { name: None, columns: vec!["order_id".into(), "line".into()] });
        items.foreign_keys = vec![ForeignKeyDef {
            columns: vec!["order_id".into()],
            ref_schema: Some("public".into()),
            ref_table: "orders".into(),
            ref_columns: vec!["id".into()],
            ..Default::default()
        }];
        let mut audit = TableDef::new(s.clone(), "audit_log");
        audit.columns = vec![ColumnDef::new("ts", "timestamptz"), ColumnDef::new("message", "text")];
        SchemaSnapshot {
            kind: Some(DbKind::Postgres),
            database: None,
            schema: s.clone(),
            tables: vec![audit, users, orders, items],
            views: vec![ViewDef {
                schema: s.clone(),
                name: "active_users".into(),
                columns: vec![ColumnDef::new("id", "integer")],
                definition: "SELECT id FROM users".into(),
                ..Default::default()
            }],
            sequences: vec![],
            routines: vec![RoutineDef {
                schema: s.clone(),
                name: "order_total".into(),
                arguments: "order_id bigint".into(),
                returns: Some("numeric".into()),
                definition: "CREATE FUNCTION ...".into(),
                ..Default::default()
            }],
            types: vec![TypeDef { schema: s, name: "mood".into(), category: "enum".into(), elements: vec!["ok".into(), "it's".into()], ..Default::default() }],
        }
    }

    #[test]
    fn system_prompt_content() {
        let p = system_prompt(DbKind::Postgres, "16.2", "table public.users (id integer PK)", Locale::Id);
        assert!(p.contains("expert in PostgreSQL 16.2"));
        assert!(p.contains("```sql"));
        assert!(p.contains("Use only the tables"));
        assert!(p.contains("Prefer read-only"));
        assert!(p.contains("warn clearly"));
        assert!(p.contains("Answer in Indonesian"));
        assert!(p.ends_with("table public.users (id integer PK)\n"));
        let m = system_prompt(DbKind::MySql, "", "", Locale::En);
        assert!(m.contains("expert in MySQL and") && m.contains("backticks") && m.contains("Answer in English"));
        assert!(m.contains("(no schema information is available)"));
        // stable: no timestamps or other volatile content
        assert_eq!(system_prompt(DbKind::Sqlite, "3.45", "x", Locale::En), system_prompt(DbKind::Sqlite, "3.45", "x", Locale::En));
    }

    #[test]
    fn request_builders() {
        let m = generate_sql("  users who ordered twice ");
        assert!(m.content.ends_with("Request: users who ordered twice"));
        let e = explain_query("SELECT 1");
        assert!(e.content.contains("```sql\nSELECT 1\n```"));
        let f = fix_error("SELEC 1", "syntax error at or near \"SELEC\"");
        assert!(f.content.contains("```sql\nSELEC 1\n```") && f.content.contains("```\nsyntax error"));
        let o = optimize_query("SELECT * FROM t", Some("Seq Scan on t"));
        assert!(o.content.contains("Execution plan:\n```\nSeq Scan on t\n```"));
        assert!(!optimize_query("SELECT 1", None).content.contains("Execution plan"));
        // SQL containing backticks gets a longer fence
        let d = describe_object("CREATE TABLE `t` (```odd```)");
        assert!(d.content.contains("````sql\n"));
        assert_eq!(chat(" hi ").content, "hi");
        assert!(m.role == crate::types::Role::User);
    }

    #[test]
    fn schema_context_format() {
        let snap = snapshot();
        let ctx = SchemaContextBuilder::new().snapshot(&snap).build();
        assert!(ctx.contains("table public.users (id integer PK, email varchar(255) NOT NULL UNIQUE, full_name text /* Display name */) -- Registered users"), "{ctx}");
        assert!(ctx.contains("  orders.user_id -> users.id"));
        assert!(ctx.contains("table public.order_items (order_id bigint NOT NULL, line int NOT NULL, sku text) PRIMARY KEY (order_id, line)"));
        assert!(ctx.contains("view public.active_users (id integer)"));
        assert!(ctx.contains("function public.order_total(order_id bigint) returns numeric"));
        assert!(ctx.contains("enum public.mood ('ok', 'it''s')"));
        assert!(!ctx.contains("SELECT id FROM users"), "view bodies are not included");
        let no_comments = SchemaContextBuilder::new().snapshot(&snap).comments(false).routines(false).build();
        assert!(!no_comments.contains("Display name") && !no_comments.contains("order_total("));
    }

    #[test]
    fn ranking_and_truncation() {
        let snap = snapshot();
        let ctx = SchemaContextBuilder::new().snapshot(&snap).question("What is the total of each order item SKU?").build();
        let pos = |n: &str| ctx.find(n).unwrap_or(usize::MAX);
        assert!(pos("table public.order_items") < pos("table public.users"));
        assert!(pos("table public.orders") < pos("table public.audit_log"), "neighbour of a matching table ranks higher");
        assert!(pos("table public.audit_log") < pos("view public.active_users"), "tables before other objects");
        // a small budget keeps the most relevant tables and lists the rest
        let small = SchemaContextBuilder::new().snapshot(&snap).question("emails of users").budget(400).build_with_stats();
        assert!(small.text.chars().count() <= 400, "{}", small.text.chars().count());
        assert!(small.text.starts_with("table public.users"), "{}", small.text);
        assert!(!small.omitted.is_empty());
        assert!(small.text.contains("more objects not shown:"));
        assert_eq!(small.included + small.omitted.len(), 4 + 1 + 1 + 1);
        let tiny = SchemaContextBuilder::new().snapshot(&snap).budget(20).build_with_stats();
        assert!(tiny.text.chars().count() <= 20);
        assert_eq!(tiny.included, 0);
        // Indonesian question words are ignored, names still match
        let id = SchemaContextBuilder::new().snapshot(&snap).question("tampilkan semua orders dari user").build();
        assert!(id.starts_with("table public.orders") || id.starts_with("table public.users"), "{id}");
    }

    #[test]
    fn word_splitting() {
        assert_eq!(words("OrderItems"), vec!["order", "item"]);
        assert_eq!(words("order_items"), vec!["order", "item"]);
        assert_eq!(words("categories"), vec!["category"]);
        assert_eq!(words("addresses"), vec!["address"]);
        assert_eq!(words("status"), vec!["status"]);
        assert_eq!(words("analysis"), vec!["analysis"]);
        assert_eq!(words("class"), vec!["class"]);
    }

    #[test]
    fn sql_block_extraction() {
        let t = "Here:\n```sql\nSELECT 1;\n```\nand\n```python\nprint(1)\n```\n```\nSELECT 2\n```\n```PostgreSQL\nSELECT 3\n```";
        assert_eq!(extract_sql_blocks(t), vec!["SELECT 1;", "SELECT 3"]);
        assert_eq!(extract_sql_blocks("```\nSELECT a\nFROM t\n```\n~~~\nSELECT b\n~~~"), vec!["SELECT a\nFROM t", "SELECT b"]);
        assert_eq!(extract_sql_blocks("no code"), Vec::<String>::new());
        assert_eq!(extract_sql_blocks("```sql SELECT 4```"), vec!["SELECT 4"]);
        assert_eq!(extract_sql_blocks("````sql\nSELECT '```'\n````"), vec!["SELECT '```'"]);
        assert_eq!(extract_sql_blocks("streaming...\n```sql\nSELECT 5 FROM"), vec!["SELECT 5 FROM"]);
        assert_eq!(extract_sql_blocks("```sql\n\n```"), Vec::<String>::new());
    }
}
