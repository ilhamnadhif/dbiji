//! Redis "dialect": commands instead of SQL. Keys are the navigator's tables;
//! reading one is the command of its type, paged by the driver.

use dbiji_core::DbKind;
use dbiji_core::ddl::*;
use dbiji_core::dialect::{Dialect, IdentCase, SqlSyntax};
use dbiji_core::meta::ObjectRef;
use dbiji_core::value::{DataKind, Value};

/// Commands for highlighting and completion.
pub(crate) const COMMANDS: &[&str] = &[
    "APPEND",
    "AUTH",
    "BITCOUNT",
    "BITPOS",
    "BLPOP",
    "BRPOP",
    "BZPOPMAX",
    "BZPOPMIN",
    "CLIENT",
    "CLUSTER",
    "COMMAND",
    "CONFIG",
    "COPY",
    "DBSIZE",
    "DECR",
    "DECRBY",
    "DEL",
    "DISCARD",
    "DUMP",
    "ECHO",
    "EVAL",
    "EVALSHA",
    "EXEC",
    "EXISTS",
    "EXPIRE",
    "EXPIREAT",
    "EXPIRETIME",
    "FLUSHALL",
    "FLUSHDB",
    "FUNCTION",
    "GEOADD",
    "GEODIST",
    "GEOPOS",
    "GEOSEARCH",
    "GET",
    "GETDEL",
    "GETEX",
    "GETRANGE",
    "GETSET",
    "HDEL",
    "HEXISTS",
    "HGET",
    "HGETALL",
    "HINCRBY",
    "HINCRBYFLOAT",
    "HKEYS",
    "HLEN",
    "HMGET",
    "HRANDFIELD",
    "HSCAN",
    "HSET",
    "HSETNX",
    "HSTRLEN",
    "HVALS",
    "INCR",
    "INCRBY",
    "INCRBYFLOAT",
    "INFO",
    "JSON.ARRAPPEND",
    "JSON.DEL",
    "JSON.GET",
    "JSON.SET",
    "JSON.TYPE",
    "KEYS",
    "LASTSAVE",
    "LATENCY",
    "LINDEX",
    "LINSERT",
    "LLEN",
    "LMOVE",
    "LPOP",
    "LPOS",
    "LPUSH",
    "LPUSHX",
    "LRANGE",
    "LREM",
    "LSET",
    "LTRIM",
    "MEMORY",
    "MGET",
    "MOVE",
    "MSET",
    "MSETNX",
    "MULTI",
    "OBJECT",
    "PERSIST",
    "PEXPIRE",
    "PEXPIREAT",
    "PFADD",
    "PFCOUNT",
    "PFMERGE",
    "PING",
    "PSETEX",
    "PTTL",
    "PUBLISH",
    "PUBSUB",
    "RANDOMKEY",
    "RENAME",
    "RENAMENX",
    "RESTORE",
    "RPOP",
    "RPUSH",
    "RPUSHX",
    "SADD",
    "SCAN",
    "SCARD",
    "SCRIPT",
    "SDIFF",
    "SDIFFSTORE",
    "SELECT",
    "SET",
    "SETEX",
    "SETNX",
    "SETRANGE",
    "SINTER",
    "SINTERCARD",
    "SISMEMBER",
    "SLOWLOG",
    "SMEMBERS",
    "SMISMEMBER",
    "SMOVE",
    "SORT",
    "SPOP",
    "SRANDMEMBER",
    "SREM",
    "SSCAN",
    "STRLEN",
    "SUNION",
    "SWAPDB",
    "TIME",
    "TOUCH",
    "TTL",
    "TYPE",
    "UNLINK",
    "WAIT",
    "XACK",
    "XADD",
    "XAUTOCLAIM",
    "XCLAIM",
    "XDEL",
    "XGROUP",
    "XINFO",
    "XLEN",
    "XPENDING",
    "XRANGE",
    "XREAD",
    "XREADGROUP",
    "XREVRANGE",
    "XTRIM",
    "ZADD",
    "ZCARD",
    "ZCOUNT",
    "ZDIFF",
    "ZINCRBY",
    "ZINTER",
    "ZLEXCOUNT",
    "ZMSCORE",
    "ZPOPMAX",
    "ZPOPMIN",
    "ZRANDMEMBER",
    "ZRANGE",
    "ZRANGEBYLEX",
    "ZRANGEBYSCORE",
    "ZRANK",
    "ZREM",
    "ZREMRANGEBYRANK",
    "ZREMRANGEBYSCORE",
    "ZREVRANGE",
    "ZREVRANGEBYSCORE",
    "ZREVRANK",
    "ZSCAN",
    "ZSCORE",
    "ZUNION",
    "ZUNIONSTORE",
    "COUNT",
    "MATCH",
    "TYPE",
    "WITHSCORES",
    "LIMIT",
    "NX",
    "XX",
    "EX",
    "PX",
    "KEEPTTL",
];

pub const SYNTAX: SqlSyntax = SqlSyntax {
    keywords: COMMANDS,
    functions: &[],
    data_types: &[
        "string",
        "hash",
        "list",
        "set",
        "zset",
        "stream",
        "ReJSON-RL",
    ],
    identifier_quotes: &[],
    double_quoted_strings: true,
    backslash_escapes: true,
    e_strings: false,
    n_strings: false,
    x_strings: false,
    dollar_quoting: false,
    hash_comments: true,
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
    extra_ident_chars: ":.-{}*",
    ident_case: IdentCase::Preserve,
    case_sensitive_names: true,
};

/// A key or value as a command argument: bare when it is safe, else quoted.
pub(crate) fn arg(s: &str) -> String {
    let bare = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || ":._-{}/@|+=".contains(c));
    if bare {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Split a command line into arguments as `redis-cli` does: `"…"` with
/// escapes (`\n`, `\xHH` …), `'…'` literally.
pub(crate) fn split_args(line: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut args = Vec::new();
    let mut chars = line.trim().chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else { break };
        let mut cur: Vec<u8> = Vec::new();
        let push = |cur: &mut Vec<u8>, c: char| {
            let mut buf = [0u8; 4];
            cur.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        };
        match first {
            '"' => {
                chars.next();
                let mut closed = false;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => {
                            closed = true;
                            break;
                        }
                        '\\' => match chars.next() {
                            Some('n') => cur.push(b'\n'),
                            Some('r') => cur.push(b'\r'),
                            Some('t') => cur.push(b'\t'),
                            Some('b') => cur.push(8),
                            Some('a') => cur.push(7),
                            Some('x') => {
                                let h: String =
                                    [chars.next(), chars.next()].into_iter().flatten().collect();
                                cur.push(
                                    u8::from_str_radix(&h, 16)
                                        .map_err(|_| format!("Invalid escape \\x{h}"))?,
                                );
                            }
                            Some(o) => push(&mut cur, o),
                            None => return Err("Unterminated quote".into()),
                        },
                        c => push(&mut cur, c),
                    }
                }
                if !closed {
                    return Err("Unbalanced quotes".into());
                }
            }
            '\'' => {
                chars.next();
                let mut closed = false;
                while let Some(c) = chars.next() {
                    if c == '\'' {
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                            cur.push(b'\'');
                            continue;
                        }
                        closed = true;
                        break;
                    }
                    if c == '\\' && chars.peek() == Some(&'\'') {
                        chars.next();
                        cur.push(b'\'');
                        continue;
                    }
                    push(&mut cur, c);
                }
                if !closed {
                    return Err("Unbalanced quotes".into());
                }
            }
            _ => {
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() {
                        break;
                    }
                    push(&mut cur, c);
                    chars.next();
                }
            }
        }
        args.push(cur);
    }
    Ok(args)
}

/// Rows read for each key type: the navigator keeps the type in the
/// object's signature.
pub(crate) fn read_command(key: &str, ty: Option<&str>) -> String {
    let k = arg(key);
    match ty.unwrap_or("") {
        "string" => format!("GET {k}"),
        "hash" => format!("HSCAN {k} 0 COUNT 1000"),
        "list" => format!("LRANGE {k} 0 -1"),
        "set" => format!("SSCAN {k} 0 COUNT 1000"),
        "zset" => format!("ZRANGE {k} 0 -1 WITHSCORES"),
        "stream" => format!("XRANGE {k} - +"),
        "ReJSON-RL" => format!("JSON.GET {k}"),
        _ => format!("TYPE {k}"),
    }
}

pub struct RedisDialect {
    ddl: RedisDdl,
}

impl RedisDialect {
    pub fn new() -> Self {
        RedisDialect { ddl: RedisDdl }
    }
}

impl Default for RedisDialect {
    fn default() -> Self {
        Self::new()
    }
}

impl Dialect for RedisDialect {
    fn kind(&self) -> DbKind {
        DbKind::Redis
    }
    fn syntax(&self) -> &SqlSyntax {
        &SYNTAX
    }
    fn ddl(&self) -> &dyn DdlGenerator {
        &self.ddl
    }
    fn quote_ident(&self, ident: &str) -> String {
        arg(ident)
    }
    fn force_quote_ident(&self, ident: &str) -> String {
        arg(ident)
    }
    fn needs_quoting(&self, ident: &str) -> bool {
        arg(ident) != ident
    }
    fn qualified(&self, _schema: Option<&str>, name: &str) -> String {
        arg(name)
    }
    fn string_literal(&self, s: &str) -> String {
        arg(s)
    }
    fn literal(&self, value: &Value, _type_name: Option<&str>) -> String {
        match value {
            Value::Null => "\"\"".into(),
            other => arg(&other.to_text()),
        }
    }
    /// The driver pages every reply itself.
    fn limit_query(&self, sql: &str, _limit: u64, _offset: u64) -> String {
        sql.to_string()
    }
    /// The number of rows the read command of a key returns.
    fn count_query(&self, sql: &str) -> String {
        let args = split_args(sql).unwrap_or_default();
        let name = args
            .first()
            .map(|a| String::from_utf8_lossy(a).to_ascii_uppercase())
            .unwrap_or_default();
        let key = args
            .get(1)
            .map(|a| arg(&String::from_utf8_lossy(a)))
            .unwrap_or_default();
        match name.as_str() {
            "HSCAN" | "HGETALL" => format!("HLEN {key}"),
            "SSCAN" | "SMEMBERS" => format!("SCARD {key}"),
            "ZSCAN" | "ZRANGE" => format!("ZCARD {key}"),
            "LRANGE" => format!("LLEN {key}"),
            "XRANGE" => format!("XLEN {key}"),
            "SCAN" | "KEYS" => "DBSIZE".into(),
            _ => format!("EXISTS {key}"),
        }
    }
    fn aggregate_query(&self, sql: &str, _select_list: &str) -> String {
        self.count_query(sql)
    }
    fn select_table(
        &self,
        table: &ObjectRef,
        _columns: Option<&[String]>,
        _where_clause: Option<&str>,
        _order_by: Option<&str>,
        _limit: Option<u64>,
    ) -> String {
        read_command(&table.name, table.signature.as_deref())
    }
    fn truncate_table(&self, table: &ObjectRef) -> String {
        format!("DEL {}", arg(&table.name))
    }
}

/// "DDL" of keys: deleting and renaming; the rest does not apply.
pub struct RedisDdl;

const NOT_REDIS: &str = "# Not available for Redis keys";

impl DdlGenerator for RedisDdl {
    fn create_table(&self, _table: &TableDef, _include_foreign_keys: bool) -> Vec<String> {
        vec![NOT_REDIS.into()]
    }
    fn drop_table(&self, _schema: Option<&str>, name: &str, _cascade: bool) -> String {
        format!("UNLINK {}", arg(name))
    }
    fn rename_table(&self, _schema: Option<&str>, old_name: &str, new_name: &str) -> String {
        format!("RENAME {} {}", arg(old_name), arg(new_name))
    }
    fn alter_table(
        &self,
        _from: &TableDef,
        _to: &TableDef,
        _renamed: &[(String, String)],
    ) -> Vec<String> {
        Vec::new()
    }
    fn add_column(&self, _table: &TableDef, _column: &ColumnDef) -> Vec<String> {
        Vec::new()
    }
    fn drop_column(&self, _table: &TableDef, _column: &str) -> String {
        NOT_REDIS.into()
    }
    fn create_index(&self, _table: &TableDef, _index: &IndexDef) -> String {
        NOT_REDIS.into()
    }
    fn drop_index(&self, _table: &TableDef, _index: &IndexDef) -> String {
        NOT_REDIS.into()
    }
    fn add_primary_key(&self, _table: &TableDef, _key: &KeyDef) -> String {
        NOT_REDIS.into()
    }
    fn drop_primary_key(&self, _table: &TableDef, _key: &KeyDef) -> String {
        NOT_REDIS.into()
    }
    fn add_unique_key(&self, _table: &TableDef, _key: &KeyDef) -> String {
        NOT_REDIS.into()
    }
    fn drop_unique_key(&self, _table: &TableDef, _key: &KeyDef) -> String {
        NOT_REDIS.into()
    }
    fn add_check(&self, _table: &TableDef, _check: &CheckDef) -> String {
        NOT_REDIS.into()
    }
    fn drop_check(&self, _table: &TableDef, _check: &CheckDef) -> String {
        NOT_REDIS.into()
    }
    fn add_foreign_key(&self, _table: &TableDef, _fk: &ForeignKeyDef) -> String {
        NOT_REDIS.into()
    }
    fn drop_foreign_key(&self, _table: &TableDef, _fk: &ForeignKeyDef) -> String {
        NOT_REDIS.into()
    }
    fn create_trigger(&self, _table: &TableDef, _trigger: &TriggerDef) -> Vec<String> {
        Vec::new()
    }
    fn drop_trigger(&self, _table: &TableDef, _trigger: &TriggerDef) -> String {
        NOT_REDIS.into()
    }
    fn create_view(&self, _view: &ViewDef, _or_replace: bool) -> Vec<String> {
        Vec::new()
    }
    fn drop_view(&self, _view: &ViewDef) -> String {
        NOT_REDIS.into()
    }
    fn create_sequence(&self, _seq: &SequenceDef) -> String {
        NOT_REDIS.into()
    }
    fn alter_sequence(&self, _from: &SequenceDef, _to: &SequenceDef) -> Vec<String> {
        Vec::new()
    }
    fn drop_sequence(&self, _seq: &SequenceDef) -> String {
        NOT_REDIS.into()
    }
    fn create_routine(&self, _routine: &RoutineDef) -> Vec<String> {
        Vec::new()
    }
    fn drop_routine(&self, _routine: &RoutineDef) -> String {
        NOT_REDIS.into()
    }
    fn create_schema(&self, _name: &str) -> String {
        NOT_REDIS.into()
    }
    fn drop_schema(&self, _name: &str, _cascade: bool) -> String {
        "FLUSHDB".into()
    }
    fn map_type(&self, _source: DbKind, _data_type: &str, _kind: DataKind) -> String {
        "string".into()
    }
    fn default_type_for(&self, _kind: DataKind) -> String {
        "string".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_like_redis_cli() {
        let a = split_args(r#"SET "user:1 name" 'it''s' plain "a\"b\n\x41""#).unwrap();
        let s: Vec<String> = a
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect();
        assert_eq!(s, ["SET", "user:1 name", "it's", "plain", "a\"b\nA"]);
        assert!(split_args("GET \"open").is_err());
        assert_eq!(arg("user:1"), "user:1");
        assert_eq!(arg("has space"), "\"has space\"");
        assert_eq!(arg("q\"x"), "\"q\\\"x\"");
        let back = split_args(&format!("GET {}", arg("we\"ird key\n"))).unwrap();
        assert_eq!(back[1], b"we\"ird key\n");
    }

    #[test]
    fn keys_read_and_count_by_type() {
        let d = RedisDialect::new();
        let key = |ty: &str| {
            ObjectRef::new(
                dbiji_core::meta::ObjectKind::Table,
                Some("db0".into()),
                "user:1",
            )
            .with_signature(ty)
        };
        assert_eq!(
            d.select_table(&key("hash"), None, None, None, Some(200)),
            "HSCAN user:1 0 COUNT 1000"
        );
        assert_eq!(
            d.select_table(&key("zset"), None, None, None, None),
            "ZRANGE user:1 0 -1 WITHSCORES"
        );
        assert_eq!(d.count_query("HSCAN user:1 0 COUNT 1000"), "HLEN user:1");
        assert_eq!(d.count_query("GET \"a b\""), "EXISTS \"a b\"");
        assert_eq!(
            d.ddl().drop_table(Some("db0"), "user:1", false),
            "UNLINK user:1"
        );
    }
}
