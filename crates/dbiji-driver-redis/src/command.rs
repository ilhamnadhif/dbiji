//! Parsing of Redis command lines: `GET user:1`, `SCAN 0 MATCH user:* COUNT 100`,
//! `HSET h f "a b" 'c d'`.
//!
//! The line is split like a shell does — spaces separate arguments, single and
//! double quotes group them, `\` escapes the next character — but nothing is
//! expanded: what is typed reaches the server.

/// Splits a command line into arguments.
pub fn parse_command(line: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' | '\n' => {
                if started {
                    args.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                        None => return Err("The single quoted text is not closed".into()),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => cur.push('\n'),
                            Some('t') => cur.push('\t'),
                            Some('r') => cur.push('\r'),
                            Some('x') => {
                                let hex: String = chars.by_ref().take(2).collect();
                                match u8::from_str_radix(&hex, 16) {
                                    Ok(b) => cur.push(b as char),
                                    Err(_) => return Err(format!("\\x{hex} is not a byte")),
                                }
                            }
                            Some(other) => cur.push(other),
                            None => return Err("The double quoted text is not closed".into()),
                        },
                        Some(c) => cur.push(c),
                        None => return Err("The double quoted text is not closed".into()),
                    }
                }
            }
            '\\' => {
                started = true;
                match chars.next() {
                    Some('n') => cur.push('\n'),
                    Some('t') => cur.push('\t'),
                    Some('r') => cur.push('\r'),
                    Some(other) => cur.push(other),
                    None => cur.push('\\'),
                }
            }
            '#' if !started => break,
            c => {
                started = true;
                cur.push(c);
            }
        }
    }
    if started {
        args.push(cur);
    }
    if args.is_empty() {
        return Err("The command is empty".into());
    }
    Ok(args)
}

/// Redis commands that only read. Anything not listed counts as a write.
const READS: &[&str] = &[
    "GET",
    "GETRANGE",
    "SUBSTR",
    "MGET",
    "STRLEN",
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
    "LATENCY",
    "SLOWLOG",
    "ROLE",
    "LASTSAVE",
    "WAIT",
    "ACL",
    "CLIENT",
    "CONFIG",
    "BITFIELD_RO",
    "GEORADIUS_RO",
];

/// True when the command does not change data, the configuration or the server.
pub fn is_read(args: &[String]) -> bool {
    let Some(name) = args.first() else {
        return true;
    };
    let name = name.to_ascii_uppercase();
    // `CONFIG GET` and `CLIENT LIST` read; the other subcommands change state.
    if name == "CONFIG" {
        return args.get(1).is_some_and(|s| matches!(s.to_ascii_uppercase().as_str(), "GET" | "RESETSTAT" | "HELP"));
    }
    if name == "CLIENT" {
        return args.get(1).is_some_and(|s| matches!(s.to_ascii_uppercase().as_str(), "LIST" | "INFO" | "GETNAME" | "ID"));
    }
    if name == "ACL" {
        return args.get(1).is_some_and(|s| matches!(s.to_ascii_uppercase().as_str(), "LIST" | "WHOAMI" | "CAT" | "GETUSER" | "HELP"));
    }
    if name == "MEMORY" {
        return args.get(1).is_some_and(|s| matches!(s.to_ascii_uppercase().as_str(), "USAGE" | "STATS" | "DOCTOR" | "MALLOC-STATS"));
    }
    if name == "OBJECT" {
        return true;
    }
    READS.contains(&name.as_str()) && !name.starts_with("CONFIG")
}

/// The command name as typed (upper case) and its first key argument, for
/// statements that address one key.
pub fn command_name(args: &[String]) -> String {
    args.first().map(|a| a.to_ascii_uppercase()).unwrap_or_default()
}

/// First argument after the command: the key of most commands.
pub fn first_key(args: &[String]) -> Option<&str> {
    args.get(1).map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_and_quotes() {
        assert_eq!(parse_command("GET user:1").unwrap(), ["GET", "user:1"]);
        assert_eq!(parse_command("SET k 'a b'").unwrap(), ["SET", "k", "a b"]);
        assert_eq!(parse_command(r#"SET k "a\"b""#).unwrap(), ["SET", "k", "a\"b"]);
        assert_eq!(parse_command("  HSET   h f v  ").unwrap(), ["HSET", "h", "f", "v"]);
        assert!(parse_command("").is_err());
        assert!(parse_command("SET k 'open").is_err());
    }

    #[test]
    fn reads_and_writes() {
        assert!(is_read(&parse_command("GET a").unwrap()));
        assert!(is_read(&parse_command("scan 0").unwrap()));
        assert!(!is_read(&parse_command("SET a 1").unwrap()));
        assert!(!is_read(&parse_command("DEL a").unwrap()));
        assert!(is_read(&parse_command("CONFIG GET maxmemory").unwrap()));
        assert!(!is_read(&parse_command("CONFIG SET maxmemory 100mb").unwrap()));
        assert!(!is_read(&parse_command("FLUSHALL").unwrap()));
    }

    #[test]
    fn keys_are_read_from_the_first_argument() {
        assert_eq!(first_key(&parse_command("GET a").unwrap()), Some("a"));
        assert_eq!(command_name(&parse_command("get a").unwrap()), "GET");
    }
}
