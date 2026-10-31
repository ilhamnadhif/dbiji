//! The Redis dialect: command keywords for the editor.
//!
//! Redis has no SQL. The syntax only describes how the editor splits and
//! highlights statements (see [`dbiji_core::Shell::Redis`]); the classifier of
//! `dbiji-sql` knows which commands write.

use crate::ddl::RedisDdl;
use dbiji_core::DbKind;
use dbiji_core::Shell;
use dbiji_core::ddl::DdlGenerator;
use dbiji_core::dialect::{Dialect, IdentCase, SqlSyntax};

/// Commands offered by completion and highlighted.
pub const COMMANDS: &[&str] = &[
    "GET",
    "SET",
    "SETEX",
    "SETNX",
    "MSET",
    "MGET",
    "APPEND",
    "STRLEN",
    "GETRANGE",
    "SETRANGE",
    "INCR",
    "DECR",
    "INCRBY",
    "DECRBY",
    "INCRBYFLOAT",
    "DEL",
    "UNLINK",
    "EXISTS",
    "EXPIRE",
    "PEXPIRE",
    "EXPIREAT",
    "PERSIST",
    "TTL",
    "PTTL",
    "TYPE",
    "KEYS",
    "SCAN",
    "RANDOMKEY",
    "RENAME",
    "COPY",
    "MOVE",
    "DBSIZE",
    "SELECT",
    "INFO",
    "PING",
    "ECHO",
    "TIME",
    "COMMAND",
    "CONFIG",
    "CLIENT",
    "MEMORY",
    "SLOWLOG",
    "LATENCY",
    "ACL",
    "SAVE",
    "BGSAVE",
    "BGREWRITEAOF",
    "LASTSAVE",
    "FLUSHDB",
    "FLUSHALL",
    "SWAPDB",
    "SHUTDOWN",
    "DEBUG",
    "MONITOR",
    "OBJECT",
    "DUMP",
    "RESTORE",
    "HGET",
    "HSET",
    "HSETNX",
    "HMSET",
    "HMGET",
    "HGETALL",
    "HDEL",
    "HEXISTS",
    "HKEYS",
    "HVALS",
    "HLEN",
    "HINCRBY",
    "HINCRBYFLOAT",
    "HSCAN",
    "HSTRLEN",
    "HRANDFIELD",
    "LPUSH",
    "RPUSH",
    "LPUSHX",
    "RPUSHX",
    "LPOP",
    "RPOP",
    "LLEN",
    "LINDEX",
    "LRANGE",
    "LSET",
    "LREM",
    "LTRIM",
    "LINSERT",
    "LPOS",
    "LMOVE",
    "RPOPLPUSH",
    "BLPOP",
    "BRPOP",
    "SADD",
    "SREM",
    "SMEMBERS",
    "SISMEMBER",
    "SMISMEMBER",
    "SCARD",
    "SPOP",
    "SRANDMEMBER",
    "SMOVE",
    "SSCAN",
    "SINTER",
    "SUNION",
    "SDIFF",
    "ZADD",
    "ZREM",
    "ZSCORE",
    "ZMSCORE",
    "ZCARD",
    "ZCOUNT",
    "ZINCRBY",
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZRANGEBYLEX",
    "ZREVRANGE",
    "ZRANK",
    "ZREVRANK",
    "ZSCAN",
    "ZPOPMIN",
    "ZPOPMAX",
    "ZRANDMEMBER",
    "ZDIFF",
    "ZINTER",
    "ZUNION",
    "XADD",
    "XLEN",
    "XRANGE",
    "XREVRANGE",
    "XREAD",
    "XDEL",
    "XTRIM",
    "XACK",
    "XINFO",
    "XGROUP",
    "PFADD",
    "PFCOUNT",
    "PFMERGE",
    "GEOADD",
    "GEOPOS",
    "GEODIST",
    "GEOSEARCH",
    "GEOHASH",
    "EVAL",
    "EVALSHA",
    "SCRIPT",
    "FUNCTION",
    "PUBLISH",
    "SUBSCRIBE",
    "PSUBSCRIBE",
    "PUBSUB",
];

/// Types of keys, shown as data types in the editor.
pub const TYPES: &[&str] = &["string", "list", "set", "zset", "hash", "stream", "bitmap", "hyperloglog", "geo"];

/// Lexical rules of the Redis command language.
pub const fn redis_syntax() -> SqlSyntax {
    SqlSyntax {
        keywords: COMMANDS,
        functions: &[],
        data_types: TYPES,
        identifier_quotes: &[('"', '"'), ('\'', '\'')],
        double_quoted_strings: true,
        backslash_escapes: true,
        e_strings: false,
        n_strings: false,
        x_strings: false,
        dollar_quoting: false,
        hash_comments: true,
        nested_block_comments: false,
        dash_comment_needs_space: false,
        question_params: false,
        dollar_params: false,
        at_variables: false,
        batch_separator: None,
        delimiter_command: false,
        compound_blocks: false,
        extra_ident_chars: ":-_*?[]",
        ident_case: IdentCase::Preserve,
        case_sensitive_names: true,
        shell: Some(Shell::Redis),
    }
}

/// The Redis dialect.
pub struct RedisDialect {
    syntax: SqlSyntax,
    ddl: RedisDdl,
}

impl RedisDialect {
    /// Create the dialect.
    pub fn new() -> Self {
        RedisDialect { syntax: redis_syntax(), ddl: RedisDdl::new(redis_syntax()) }
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
        &self.syntax
    }

    fn ddl(&self) -> &dyn DdlGenerator {
        &self.ddl
    }

    /// `DBIJI.KEYDATA key` — the DBiji extension that reads a key's value
    /// whatever its type (the dot keeps it apart from every Redis command).
    fn table_data_query(&self, table: &dbiji_core::meta::ObjectRef, _limit: Option<u64>) -> String {
        format!("DBIJI.KEYDATA {}", crate::ddl::quote(&table.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dialect_speaks_commands() {
        let d = RedisDialect::new();
        assert_eq!(d.kind(), DbKind::Redis);
        assert_eq!(d.syntax().shell, Some(Shell::Redis));
        assert!(d.syntax().is_keyword("HGETALL"));
        let t = dbiji_core::meta::ObjectRef::table(Some("0".into()), "user:1");
        assert_eq!(d.table_data_query(&t, None), "DBIJI.KEYDATA \"user:1\"");
    }
}
