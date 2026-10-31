//! Statement splitting and classification for the non-SQL shell languages
//! ([`Shell::Mongo`], [`Shell::Redis`]).
//!
//! A statement ends at a `;` or at a line break outside strings, comments and
//! brackets, unless the next line continues it (`.sort(`, `)`, `]`, `}`) or
//! the current one obviously continues (a trailing `,`, `(` or `{`).
//! Classification reads the command name: shell queries never count as
//! writes, and only the commands that drop data need a confirmation.

use crate::classify::{Danger, StatementInfo, StatementKind};
use crate::position::floor_char_boundary;
use crate::split::StatementSpan;
use dbiji_core::Shell;

/// One line of a statement: byte range plus whether it ended at a delimiter.
struct Raw {
    start: usize,
    end: usize,
    outer_end: usize,
    terminated: bool,
}

/// Splits a shell script into statements (see [`crate::split_statements`]).
pub fn split_shell(src: &str) -> Vec<StatementSpan> {
    let mut out = Vec::new();
    for r in scan(src) {
        if src[r.start..r.end].trim().is_empty() {
            continue;
        }
        out.push(StatementSpan {
            start: r.start,
            end: r.end,
            outer_start: r.start,
            outer_end: r.outer_end,
            terminated: r.terminated,
            is_client_command: false,
        });
    }
    out
}

/// The statement under `offset`, as [`crate::statement_at`] picks it.
pub fn statement_at(src: &str, offset: usize) -> Option<StatementSpan> {
    let spans = split_shell(src);
    crate::split::pick_statement(&spans, src, floor_char_boundary(src, offset))
}

fn scan(src: &str) -> Vec<Raw> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut start = 0usize;
    let mut last_sig = 0usize;
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut line_comment = false;
    let mut block_comment = false;
    let mut depth = 0i32;

    // Ends the statement starting at `start` (last significant byte ends at
    // `last_sig`), then starts the next one after `next`.
    macro_rules! finish {
        ($next:expr, $terminated:expr) => {{
            if last_sig > start {
                let lead = src[start..last_sig].len() - src[start..last_sig].trim_start().len();
                out.push(Raw {
                    start: start + lead,
                    end: last_sig,
                    outer_end: $next,
                    terminated: $terminated,
                });
            }
            start = $next;
            last_sig = $next;
        }};
    }

    while i < b.len() {
        let c = b[i];
        if line_comment {
            if c == b'\n' {
                line_comment = false;
                // Fall through to the newline handling below.
            } else {
                i += 1;
                continue;
            }
        }
        if block_comment {
            if c == b'*' && b.get(i + 1) == Some(&b'/') {
                block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == b'\\' && q != b'`' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            i += 1;
            if !c.is_ascii_whitespace() {
                last_sig = i;
            }
            continue;
        }
        match c {
            b'\'' | b'"' | b'`' => {
                quote = Some(c);
                last_sig = i + 1;
                i += 1;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                line_comment = true;
                i += 2;
            }
            b'#' => {
                line_comment = true;
                i += 1;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                block_comment = true;
                i += 2;
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                last_sig = i + 1;
                i += 1;
            }
            b')' | b']' | b'}' => {
                depth = (depth - 1).max(0);
                last_sig = i + 1;
                i += 1;
            }
            b';' if depth == 0 => {
                finish!(i + 1, true);
                i += 1;
            }
            b'\n' if depth == 0 => {
                if last_sig > start
                    && !continues_after(src, start, last_sig)
                    && !next_line_continues(&src[i + 1..])
                {
                    finish!(i + 1, false);
                } else if last_sig == start {
                    start = i + 1;
                }
                i += 1;
            }
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                last_sig = i + 1;
                i += 1;
            }
        }
    }
    if last_sig > start {
        let lead = src[start..last_sig].len() - src[start..last_sig].trim_start().len();
        out.push(Raw {
            start: start + lead,
            end: last_sig,
            outer_end: last_sig,
            terminated: false,
        });
    }
    out
}

/// The statement text ends with a token that expects a continuation.
fn continues_after(src: &str, start: usize, last_sig: usize) -> bool {
    let text = src[start..last_sig].trim_end();
    let Some(last) = text.chars().next_back() else {
        return false;
    };
    matches!(
        last,
        ',' | '(' | '[' | '{' | '.' | '\\' | '+' | '=' | ':' | '&' | '|'
    )
}

/// The next line starts with something that continues the current statement.
fn next_line_continues(rest: &str) -> bool {
    let next = rest.split('\n').find(|l| !l.trim().is_empty());
    match next {
        Some(l) => {
            let t = l.trim_start();
            t.starts_with('.')
                || t.starts_with(')')
                || t.starts_with(']')
                || t.starts_with('}')
                || t.starts_with(',')
        }
        None => false,
    }
}

/// Classifies one shell statement (see [`crate::classify`]).
pub fn classify_shell(stmt: &str, shell: Shell) -> StatementInfo {
    match shell {
        Shell::Mongo => classify_mongo(stmt),
        Shell::Redis => classify_redis(stmt),
    }
}

/// Mongo shell methods that only read.
const MONGO_READS: &[&str] = &[
    "find",
    "findOne",
    "aggregate",
    "count",
    "countDocuments",
    "estimatedDocumentCount",
    "distinct",
    "explain",
    "getIndexes",
    "getIndexSpecs",
    "getIndexKeys",
    "stats",
    "dataSize",
    "totalSize",
    "storageSize",
    "validate",
    "isCapped",
    "getShardVersion",
    "listCollections",
    "listIndexes",
    "getCollectionNames",
    "getCollectionInfos",
    "getDatabaseNames",
    "dbStats",
    "serverStatus",
    "hello",
    "isMaster",
    "version",
    "help",
    "getMongo",
    "getSiblingDB",
    "getCollection",
    "getProfilingStatus",
    "print",
    "printjson",
    "watch",
];

/// Mongo shell methods that change data or schema.
const MONGO_WRITES: &[&str] = &[
    "insert",
    "insertOne",
    "insertMany",
    "update",
    "updateOne",
    "updateMany",
    "replaceOne",
    "deleteOne",
    "deleteMany",
    "remove",
    "findOneAndUpdate",
    "findOneAndReplace",
    "findOneAndDelete",
    "bulkWrite",
    "save",
    "createIndex",
    "createIndexes",
    "dropIndex",
    "dropIndexes",
    "reIndex",
    "drop",
    "createCollection",
    "createView",
    "create",
    "renameCollection",
    "dropDatabase",
    "runCommand",
    "adminCommand",
    "setProfilingLevel",
    "setLogLevel",
    "compact",
    "shardCollection",
    "enableSharding",
    "createUser",
    "updateUser",
    "dropUser",
    "grantRolesToUser",
    "revokeRolesFromUser",
    "createRole",
    "dropRole",
];

fn classify_mongo(stmt: &str) -> StatementInfo {
    let text = strip_comment(stmt);
    let head = text.trim();
    let first = head
        .split(['(', '.', ' ', '\t', '\n'])
        .next()
        .unwrap_or("")
        .to_string();
    let kind = StatementKind::Select;
    // Method name: the last `.name(` before the first `(` of the call.
    let method = head
        .split('.')
        .find_map(|seg| {
            let seg = seg.trim();
            seg.find('(').map(|p| seg[..p].trim().to_string())
        })
        .unwrap_or_default();
    let method = method.trim();
    let is_write = MONGO_WRITES.iter().any(|m| m.eq_ignore_ascii_case(method));
    let is_read = MONGO_READS.iter().any(|m| m.eq_ignore_ascii_case(method));
    let mut info = StatementInfo {
        kind: if is_write {
            StatementKind::Insert
        } else {
            kind
        },
        first_keyword: if method.is_empty() {
            first.clone()
        } else {
            method.to_string()
        },
        returns_rows: !is_write || method.eq_ignore_ascii_case("findOneAndUpdate"),
        is_write,
        danger: None,
        target: None,
    };
    if !is_read
        && !is_write
        && !first.eq_ignore_ascii_case("show")
        && !first.eq_ignore_ascii_case("use")
    {
        // An unknown method (`db.c.frobnicate()`): assume it changes something.
        info.is_write = true;
        info.kind = StatementKind::Other;
    }
    if first.eq_ignore_ascii_case("show") || first.eq_ignore_ascii_case("use") {
        info.is_write = false;
        info.kind = StatementKind::Select;
        info.returns_rows = true;
    }
    if method.eq_ignore_ascii_case("drop")
        || method.eq_ignore_ascii_case("dropDatabase")
        || method.eq_ignore_ascii_case("dropIndexes")
    {
        info.danger = Some(Danger::Drop);
    }
    // `deleteMany({})` / `updateMany({}, …)` touch every document.
    let args = text.split_once('(').map(|(_, r)| r).unwrap_or("");
    if matches!(
        method,
        "deleteMany" | "remove" | "updateMany" | "findOneAndUpdate"
    ) && empty_filter(args)
    {
        info.danger = Some(if method == "updateMany" || method == "findOneAndUpdate" {
            Danger::UpdateWithoutWhere
        } else {
            Danger::DeleteWithoutWhere
        });
    }
    info
}

/// Does the argument list start with an empty filter document?
fn empty_filter(args: &str) -> bool {
    let a = args.trim_start();
    a.starts_with("{}") || a.starts_with("{ }")
}

fn strip_comment(stmt: &str) -> &str {
    let t = stmt.trim_start();
    if let Some(rest) = t.strip_prefix("//") {
        return rest.split('\n').next().unwrap_or("").trim();
    }
    t
}

/// Redis commands that only read.
const REDIS_READS: &[&str] = &[
    "GET",
    "GETRANGE",
    "MGET",
    "STRLEN",
    "SUBSTR",
    "EXISTS",
    "TYPE",
    "TTL",
    "PTTL",
    "EXPIRETIME",
    "PEXPIRETIME",
    "KEYS",
    "SCAN",
    "RANDOMKEY",
    "DBSIZE",
    "TOUCH",
    "OBJECT",
    "DUMP",
    "BITCOUNT",
    "BITPOS",
    "GETBIT",
    "LCS",
    "HGET",
    "HGETALL",
    "HMGET",
    "HLEN",
    "HEXISTS",
    "HKEYS",
    "HVALS",
    "HSCAN",
    "HSTRLEN",
    "HRANDFIELD",
    "LLEN",
    "LINDEX",
    "LRANGE",
    "LPOS",
    "SCARD",
    "SMEMBERS",
    "SISMEMBER",
    "SMISMEMBER",
    "SRANDMEMBER",
    "SSCAN",
    "ZCARD",
    "ZCOUNT",
    "ZSCORE",
    "ZMSCORE",
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZRANGEBYLEX",
    "ZREVRANGE",
    "ZREVRANGEBYSCORE",
    "ZREVRANGEBYLEX",
    "ZRANK",
    "ZREVRANK",
    "ZSCAN",
    "ZRANDMEMBER",
    "ZDIFF",
    "ZINTER",
    "ZUNION",
    "XLEN",
    "XRANGE",
    "XREVRANGE",
    "XINFO",
    "XPENDING",
    "XREAD",
    "PFCOUNT",
    "GEOHASH",
    "GEOPOS",
    "GEODIST",
    "GEOSEARCH",
    "INFO",
    "PING",
    "ECHO",
    "TIME",
    "COMMAND",
    "MEMORY",
    "DBIJI.KEYDATA",
    "SELECT",
    "CLIENT",
    "CONFIG",
    "ACL",
    "LATENCY",
    "SLOWLOG",
    "ROLE",
    "LASTSAVE",
    "WAIT",
    "BITFIELD_RO",
    "GEORADIUS_RO",
];

/// Redis commands that change data, configuration or the server.
const REDIS_WRITES: &[&str] = &[
    "SET",
    "SETEX",
    "PSETEX",
    "SETNX",
    "SETRANGE",
    "MSET",
    "MSETNX",
    "APPEND",
    "GETSET",
    "GETDEL",
    "GETEX",
    "INCR",
    "DECR",
    "INCRBY",
    "DECRBY",
    "INCRBYFLOAT",
    "DEL",
    "UNLINK",
    "EXPIRE",
    "PEXPIRE",
    "EXPIREAT",
    "PEXPIREAT",
    "PERSIST",
    "RENAME",
    "RENAMENX",
    "MOVE",
    "COPY",
    "RESTORE",
    "MIGRATE",
    "HSET",
    "HMSET",
    "HSETNX",
    "HDEL",
    "HINCRBY",
    "HINCRBYFLOAT",
    "LPUSH",
    "RPUSH",
    "LPUSHX",
    "RPUSHX",
    "LPOP",
    "RPOP",
    "LSET",
    "LREM",
    "LTRIM",
    "LINSERT",
    "LMOVE",
    "RPOPLPUSH",
    "BLMOVE",
    "SADD",
    "SREM",
    "SPOP",
    "SMOVE",
    "ZADD",
    "ZREM",
    "ZINCRBY",
    "ZREMRANGEBYRANK",
    "ZREMRANGEBYSCORE",
    "ZREMRANGEBYLEX",
    "ZPOPMIN",
    "ZPOPMAX",
    "ZDIFFSTORE",
    "ZINTERSTORE",
    "ZUNIONSTORE",
    "XADD",
    "XDEL",
    "XTRIM",
    "XACK",
    "XCLAIM",
    "XGROUP",
    "XSETID",
    "PFADD",
    "PFMERGE",
    "GEOADD",
    "GEOSEARCHSTORE",
    "EVAL",
    "EVALSHA",
    "EVAL_RO",
    "FCALL",
    "SCRIPT",
    "FUNCTION",
    "FLUSHDB",
    "FLUSHALL",
    "SWAPDB",
    "CONFIG",
    "ACL",
    "CLIENT",
    "DEBUG",
    "REPLICAOF",
    "SLAVEOF",
    "SHUTDOWN",
    "SAVE",
    "BGSAVE",
    "BGREWRITEAOF",
    "MODULE",
    "MONITOR",
    "SUBSCRIBE",
    "PSUBSCRIBE",
    "PUBLISH",
];

fn classify_redis(stmt: &str) -> StatementInfo {
    let text = stmt.trim();
    let name = text
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    let upper = name.as_str();
    let flush = matches!(upper, "FLUSHDB" | "FLUSHALL");
    let server = matches!(upper, "SHUTDOWN" | "DEBUG");
    let write = REDIS_WRITES.contains(&upper);
    let read = REDIS_READS.contains(&upper);
    StatementInfo {
        kind: if write || flush || server {
            StatementKind::Other
        } else {
            StatementKind::Select
        },
        first_keyword: name.clone(),
        returns_rows: !write
            || matches!(
                upper,
                "SET" | "DEL" | "HSET" | "LPUSH" | "RPUSH" | "SADD" | "ZADD" | "INCR" | "DECR"
            ),
        is_write: write || flush || server || (!read && !name.is_empty()),
        danger: if flush {
            Some(Danger::Flush)
        } else if server {
            Some(Danger::ServerCommand)
        } else {
            None
        },
        target: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mongo_statements_split_at_semicolons_and_lines() {
        let src = "db.users.find({age: {$gt: 30}})\ndb.users.countDocuments({})\ndb.a.insertOne({x: 1}); db.a.find({})";
        let spans = split_shell(src);
        assert_eq!(spans.len(), 4, "{spans:?}");
        assert_eq!(spans[0].text(src), "db.users.find({age: {$gt: 30}})");
        assert_eq!(spans[3].text(src), "db.a.find({})");
    }

    #[test]
    fn chained_calls_and_multiline_documents_stay_together() {
        let src = "db.users.find({\n  age: 1,\n\n  name: 'x'\n}).sort({age: -1})\n.limit(5)\n\ndb.b.find({})";
        let spans = split_shell(src);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert!(
            spans[0].text(src).ends_with(".limit(5)"),
            "{:?}",
            spans[0].text(src)
        );
    }

    #[test]
    fn strings_and_comments_do_not_split() {
        let src = "db.t.insertOne({a: 'x;y', b: \"p\nq\"}) // one; two\ndb.t.find({})";
        let spans = split_shell(src);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(spans[1].text(src), "db.t.find({})");
    }

    #[test]
    fn redis_lines_are_commands() {
        let src = "GET foo\nHGETALL bar\nSCAN 0 MATCH user:* COUNT 10\n";
        let spans = split_shell(src);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[2].text(src), "SCAN 0 MATCH user:* COUNT 10");
    }

    #[test]
    fn mongo_reads_are_not_writes() {
        let i = classify_shell("db.users.find({})", Shell::Mongo);
        assert!(!i.is_write && i.returns_rows);
        assert_eq!(i.first_keyword, "find");
        let w = classify_shell("db.users.insertOne({a: 1})", Shell::Mongo);
        assert!(w.is_write && !w.returns_rows);
        assert_eq!(
            classify_shell("show collections", Shell::Mongo).first_keyword,
            "show"
        );
        assert!(!classify_shell("show collections", Shell::Mongo).is_write);
        let d = classify_shell("db.users.deleteMany({})", Shell::Mongo);
        assert_eq!(d.danger, Some(Danger::DeleteWithoutWhere));
        assert_eq!(
            classify_shell("db.users.drop()", Shell::Mongo).danger,
            Some(Danger::Drop)
        );
        assert_eq!(
            classify_shell("db.users.deleteMany({a: 1})", Shell::Mongo).danger,
            None
        );
    }

    #[test]
    fn the_sql_services_route_shell_syntax_here() {
        let mut syntax = dbiji_core::SqlSyntax::ansi();
        syntax.shell = Some(Shell::Mongo);
        let opts = crate::SplitOptions::default();
        let spans = crate::split_statements("db.a.find({})\ndb.a.count()", &syntax, &opts);
        assert_eq!(spans.len(), 2);
        let info = crate::classify("db.a.insertOne({x: 1})", &syntax);
        assert!(info.is_write);
        assert_eq!(
            crate::statement_at("db.a.find({})", 3, &syntax, &opts)
                .map(|s| s.text("db.a.find({})").to_string())
                .as_deref(),
            Some("db.a.find({})")
        );
        assert_eq!(
            crate::format_sql("db.a.find(  {x:1}  )", &syntax, &Default::default()),
            "db.a.find(  {x:1}  )"
        );
        assert!(crate::syntax_check("db.a.find({})", dbiji_core::DbKind::MongoDb).is_none());
    }

    #[test]
    fn redis_reads_and_writes() {
        assert!(!classify_shell("GET foo", Shell::Redis).is_write);
        assert!(classify_shell("SET foo 1", Shell::Redis).is_write);
        assert_eq!(
            classify_shell("FLUSHALL", Shell::Redis).danger,
            Some(Danger::Flush)
        );
        assert_eq!(
            classify_shell("SHUTDOWN", Shell::Redis).danger,
            Some(Danger::ServerCommand)
        );
        // `SELECT n` only switches the database of the session.
        assert!(!classify_shell("SELECT 2", Shell::Redis).is_write);
        assert!(!classify_shell("scan 0", Shell::Redis).is_write);
    }
}
