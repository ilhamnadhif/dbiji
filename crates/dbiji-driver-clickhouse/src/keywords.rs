//! ClickHouse SQL vocabulary for highlighting and completion.

/// Keywords (upper case).
pub const KEYWORDS: &[&str] = &[
    "ACCESS", "ACTION", "ADD", "ADMIN", "AFTER", "ALGORITHM", "ALIAS", "ALL", "ALLOWED_LATENESS",
    "ALTER", "AND", "ANTI", "ANY", "APPEND", "APPLY", "ARRAY", "AS", "ASC", "ASCENDING", "ASOF",
    "ASSUME", "AST", "ASYNC", "ATTACH", "AUTO_INCREMENT", "BACKUP", "BEGIN", "BETWEEN",
    "BIDIRECTIONAL", "BOTH", "BY", "CACHE", "CACHES", "CASCADE", "CASE", "CAST", "CHANGE",
    "CHANGEABLE_IN_READONLY", "CHANGED", "CHAR", "CHARACTER", "CHECK", "CLEANUP", "CLEAR",
    "CLUSTER", "CLUSTERS", "CODEC", "COLLATE", "COLLECTION", "COLUMN", "COLUMNS", "COMMENT",
    "COMMIT", "COMPRESSION", "CONST", "CONSTRAINT", "CREATE", "CROSS", "CUBE", "CURRENT",
    "CURRENT_USER", "DATA", "DATABASE", "DATABASES", "DATE", "DAY", "DAYS", "DDL", "DEDUPLICATE",
    "DEFAULT", "DEFINER", "DELAY", "DELETE", "DELETED", "DEPENDS", "DESC", "DESCENDING",
    "DESCRIBE", "DETACH", "DETACHED", "DICTIONARIES", "DICTIONARY", "DISK", "DISTINCT",
    "DISTRIBUTED", "DIV", "DROP", "ELSE", "EMPTY", "ENABLED", "END", "ENFORCED", "ENGINE",
    "ENGINES", "EPHEMERAL", "ESTIMATE", "EVENT", "EVENTS", "EVERY", "EXCEPT", "EXCHANGE",
    "EXISTS", "EXPLAIN", "EXPRESSION", "EXTENDED", "EXTERNAL", "FAKE", "FALSE", "FETCH", "FIELDS",
    "FILE", "FILESYSTEM", "FILL", "FILTER", "FINAL", "FIRST", "FLUSH", "FOLLOWING", "FOR",
    "FOREIGN", "FORMAT", "FREEZE", "FROM", "FULL", "FULLTEXT", "FUNCTION", "FUNCTIONS", "GLOBAL",
    "GRANT", "GRANTEES", "GRANTS", "GRANULARITY", "GROUP", "GROUPING", "GROUPS", "HASH", "HAVING",
    "HDFS", "HIERARCHICAL", "HOST", "HOUR", "HOURS", "ID", "IDENTIFIED", "IF", "IGNORE", "ILIKE",
    "IMPLICIT", "IN", "INDEX", "INDEXES", "INDICES", "INFILE", "INHERIT", "INJECTIVE", "INNER",
    "INSERT", "INTERPOLATE", "INTERSECT", "INTERVAL", "INTO", "INVISIBLE", "INVOKER", "IP", "IS",
    "IS_OBJECT_ID", "JOIN", "JWT", "KERBEROS", "KEY", "KEYED", "KEYS", "KILL", "KIND", "LARGE",
    "LAST", "LAYOUT", "LDAP", "LEADING", "LEFT", "LESS", "LEVEL", "LIFETIME", "LIGHTWEIGHT",
    "LIKE", "LIMIT", "LIMITS", "LINEAR", "LIST", "LIVE", "LOCAL", "MASK", "MATERIALIZE",
    "MATERIALIZED", "MAX", "MERGES", "METRICS", "MICROSECOND", "MILLISECOND", "MIN", "MINUTE",
    "MINUTES", "MOD", "MODIFY", "MONTH", "MONTHS", "MOVE", "MUTATION", "NAME", "NAMED", "NANOSECOND",
    "NEXT", "NO", "NONE", "NOT", "NULL", "NULLS", "OBJECT", "OFFSET", "ON", "ONLY", "OPTIMIZE",
    "OPTION", "OR", "ORDER", "OUTER", "OUTFILE", "OVER", "OVERRIDE", "PARALLEL", "PART",
    "PARTIAL", "PARTITION", "PARTITIONS", "PART_MOVE_TO_SHARD", "PASTE", "PERIODIC", "PERMANENTLY",
    "PERMISSIVE", "PERSISTENT", "PIPELINE", "PLAINTEXT_PASSWORD", "PLAN", "POLICY", "POPULATE",
    "PRECEDING", "PRECISION", "PREWHERE", "PRIMARY", "PRIVILEGES", "PROCESSLIST", "PROFILE",
    "PROJECTION", "PULL", "QUALIFY", "QUARTER", "QUARTERS", "QUERY", "QUOTA", "RANDOMIZE",
    "RANDOMIZED", "RANGE", "READONLY", "REALM", "RECOMPRESS", "RECURSIVE", "REFERENCES", "REFRESH",
    "REGEXP", "REMOVE", "RENAME", "REPLACE", "RESET", "RESPECT", "RESTORE", "RESTRICT",
    "RESTRICTIVE", "RESUME", "REVOKE", "RIGHT", "ROLE", "ROLES", "ROLLBACK", "ROLLUP", "ROW",
    "ROWS", "S3", "SALT", "SAMPLE", "SAN", "SCHEME", "SECOND", "SECONDS", "SECURITY", "SELECT",
    "SEMI", "SEQUENTIAL", "SERVER", "SET", "SETS", "SETTING", "SETTINGS", "SHARD", "SHOW",
    "SIGNED", "SIMPLE", "SNAPSHOT", "SOURCE", "SPATIAL", "SQL", "SSH_KEY", "SSL_CERTIFICATE",
    "START", "STATISTICS", "STDOUT", "STEP", "STORAGE", "STRICT", "STRICTLY_ASCENDING", "SUBPARTITION",
    "SUBPARTITIONS", "SUSPEND", "SYNC", "SYNTAX", "SYSTEM", "TABLE", "TABLES", "TAG", "TAGS",
    "TEMPORARY", "TEST", "THAN", "THEN", "TIES", "TIME", "TIMESTAMP", "TO", "TOP", "TOTALS",
    "TRACKING", "TRAILING", "TRANSACTION", "TREE", "TRIGGER", "TRUE", "TRUNCATE", "TTL", "TYPE",
    "TYPEOF", "UNBOUNDED", "UNDROP", "UNFREEZE", "UNION", "UNIQUE", "UNSET", "UNSIGNED", "UNTIL",
    "UPDATE", "URL", "USE", "USER", "USING", "UUID", "VALID", "VALUES", "VARYING", "VIEW",
    "VISIBLE", "WATCH", "WATERMARK", "WEEK", "WEEKS", "WHEN", "WHERE", "WINDOW", "WITH",
    "WITH_ITEMINDEX", "WK", "WRITABLE", "YEAR", "YEARS", "ZKPATH",
];

/// Frequently used built-in functions (ClickHouse function names are
/// case-sensitive except for the SQL standard ones).
pub const FUNCTIONS: &[&str] = &[
    "abs", "accurateCast", "accurateCastOrNull", "acos", "addDays", "addHours", "addMinutes",
    "addMonths", "addSeconds", "addWeeks", "addYears", "any", "anyHeavy", "anyLast", "argMax",
    "argMin", "array", "arrayAll", "arrayConcat", "arrayCount", "arrayDistinct", "arrayElement",
    "arrayEnumerate", "arrayExists", "arrayFilter", "arrayFirst", "arrayFlatten", "arrayJoin",
    "arrayMap", "arrayReduce", "arrayReverse", "arraySlice", "arraySort", "arrayStringConcat",
    "arraySum", "arrayUniq", "asin", "atan", "avg", "avgIf", "bar", "base64Decode",
    "base64Encode", "bitAnd", "bitCount", "bitOr", "bitShiftLeft", "bitShiftRight", "bitXor",
    "cast", "cbrt", "ceil", "char", "cityHash64", "coalesce", "concat", "concatWithSeparator",
    "corr", "cos", "count", "countDistinct", "countEqual", "countIf", "covarPop", "covarSamp",
    "currentDatabase", "currentUser", "dateDiff", "dateName", "dateTrunc", "date_diff",
    "date_trunc", "dictGet", "dictGetOrDefault", "dictHas", "divide", "domain", "empty",
    "endsWith", "equals", "exp", "extract", "extractAll", "farmHash64", "file", "finalizeAggregation",
    "first_value", "floor", "format", "formatDateTime", "formatReadableQuantity",
    "formatReadableSize", "formatReadableTimeDelta", "fromUnixTimestamp", "fromUnixTimestamp64Milli",
    "generateUUIDv4", "generateUUIDv7", "geoDistance", "getSetting", "greatest", "groupArray",
    "groupArrayInsertAt", "groupBitAnd", "groupBitOr", "groupUniqArray", "halfMD5", "has",
    "hasAll", "hasAny", "hex", "histogram", "if", "ifNull", "ilike", "in", "indexOf", "intDiv",
    "intHash64", "isFinite", "isInfinite", "isNaN", "isNotNull", "isNull", "JSONExtract",
    "JSONExtractBool", "JSONExtractFloat", "JSONExtractInt", "JSONExtractKeys", "JSONExtractRaw",
    "JSONExtractString", "JSONHas", "JSONLength", "lag", "lagInFrame", "last_value", "lead",
    "leadInFrame", "least", "left", "length", "lengthUTF8", "like", "ln", "log", "log10", "log2",
    "lower", "lowerUTF8", "ltrim", "map", "mapContains", "mapKeys", "mapValues", "match", "max",
    "maxIf", "maxMap", "median", "min", "minIf", "minMap", "minus", "modulo", "multiIf",
    "multiply", "multiSearchAny", "murmurHash3_64", "negate", "now", "now64", "nullIf", "numbers",
    "parseDateTime", "parseDateTime64BestEffort", "parseDateTimeBestEffort",
    "parseDateTimeBestEffortOrNull", "plus", "position", "positionCaseInsensitive", "pow",
    "quantile", "quantileExact", "quantiles", "quantileTDigest", "rand", "rand64", "range", "rank",
    "regexpExtract", "remote", "repeat", "replaceAll", "replaceOne", "replaceRegexpAll",
    "replaceRegexpOne", "reverse", "right", "round", "row_number", "rtrim", "runningDifference",
    "s3", "sipHash64", "sleep", "sleepEachRow", "splitByChar", "splitByString", "sqrt",
    "startsWith", "stddevPop", "stddevSamp", "subDays", "substring", "substringUTF8", "sum",
    "sumIf", "sumMap", "tan", "throwIf", "timeSlot", "timezone", "toDate", "toDate32",
    "toDateOrNull", "toDateTime", "toDateTime64", "toDateTimeOrNull", "toDayOfMonth",
    "toDayOfWeek", "toDayOfYear", "toDecimal128", "toDecimal32", "toDecimal64", "toFixedString",
    "toFloat32", "toFloat64", "toFloat64OrNull", "toHour", "toInt16", "toInt32", "toInt64",
    "toInt64OrNull", "toInt8", "toIPv4", "toIPv6", "toLowCardinality", "toMinute", "toMonday",
    "toMonth", "toNullable", "toQuarter", "toSecond", "toStartOfDay", "toStartOfFifteenMinutes",
    "toStartOfFiveMinutes", "toStartOfHour", "toStartOfInterval", "toStartOfMinute",
    "toStartOfMonth", "toStartOfQuarter", "toStartOfWeek", "toStartOfYear", "toString",
    "toStringCutToZero", "toTimeZone", "toTypeName", "toUInt16", "toUInt32", "toUInt64",
    "toUInt8", "toUnixTimestamp", "toUnixTimestamp64Milli", "toUUID", "toWeek", "toYear",
    "toYYYYMM", "toYYYYMMDD", "today", "topK", "transform", "trim", "trimBoth", "trimLeft",
    "trimRight", "tuple", "tupleElement", "unhex", "uniq", "uniqCombined", "uniqExact", "uniqHLL12",
    "upper", "upperUTF8", "url", "uptime", "version", "visibleWidth", "windowFunnel", "xxHash64",
    "yesterday",
];

/// Data type names.
pub const TYPES: &[&str] = &[
    "AggregateFunction", "Array", "BFloat16", "Bool", "Boolean", "Date", "Date32", "DateTime",
    "DateTime64", "Decimal", "Decimal128", "Decimal256", "Decimal32", "Decimal64", "Dynamic",
    "Enum", "Enum16", "Enum8", "FixedString", "Float32", "Float64", "Int128", "Int16", "Int256",
    "Int32", "Int64", "Int8", "IntervalDay", "IntervalHour", "IntervalMinute", "IntervalMonth",
    "IntervalSecond", "IntervalWeek", "IntervalYear", "IPv4", "IPv6", "JSON", "LineString",
    "LowCardinality", "Map", "MultiLineString", "MultiPolygon", "Nested", "Nothing", "Nullable",
    "Object", "Point", "Polygon", "Ring", "SimpleAggregateFunction", "String", "Time", "Time64",
    "Tuple", "UInt128", "UInt16", "UInt256", "UInt32", "UInt64", "UInt8", "UUID", "Variant",
];

/// Words that must be quoted when used as identifiers (a conservative
/// subset: ClickHouse has few truly reserved words, but quoting these
/// avoids ambiguities in generated SQL).
pub const RESERVED: &[&str] = &[
    "ALL", "AND", "ANTI", "ANY", "ARRAY", "AS", "ASC", "ASOF", "BETWEEN", "BY", "CASE", "CAST",
    "CROSS", "DESC", "DISTINCT", "ELSE", "END", "EXCEPT", "EXISTS", "FALSE", "FINAL", "FORMAT",
    "FROM", "FULL", "GLOBAL", "GROUP", "HAVING", "ILIKE", "IN", "INNER", "INTERSECT", "INTERVAL",
    "INTO", "IS", "JOIN", "LEFT", "LIKE", "LIMIT", "NOT", "NULL", "OFFSET", "ON", "OR", "ORDER",
    "OUTER", "PREWHERE", "RIGHT", "SAMPLE", "SELECT", "SEMI", "SETTINGS", "THEN", "TOP", "TRUE",
    "UNION", "USING", "WHEN", "WHERE", "WINDOW", "WITH",
];

/// Reserved word check (case-insensitive).
pub fn is_reserved(word: &str) -> bool {
    RESERVED.iter().any(|k| k.eq_ignore_ascii_case(word))
}
