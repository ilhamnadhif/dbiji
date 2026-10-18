//! PostgreSQL type knowledge: well-known OIDs, the conversion plan used for
//! every result column ([`Conv`]), logical kinds and display names including
//! type modifiers (`varchar(255)`, `numeric(10,2)`, `geometry(Point,4326)`).

use dbiji_core::DataKind;
use postgres_types::{Kind, Type};

/// OIDs of built-in types (stable across PostgreSQL versions).
#[allow(dead_code)]
pub(crate) mod oid {
    pub const BOOL: u32 = 16;
    pub const BYTEA: u32 = 17;
    pub const CHAR: u32 = 18;
    pub const NAME: u32 = 19;
    pub const INT8: u32 = 20;
    pub const INT2: u32 = 21;
    pub const INT4: u32 = 23;
    pub const REGPROC: u32 = 24;
    pub const TEXT: u32 = 25;
    pub const OID: u32 = 26;
    pub const XID: u32 = 28;
    pub const CID: u32 = 29;
    pub const JSON: u32 = 114;
    pub const XML: u32 = 142;
    pub const CIDR: u32 = 650;
    pub const FLOAT4: u32 = 700;
    pub const FLOAT8: u32 = 701;
    pub const UNKNOWN: u32 = 705;
    pub const MACADDR8: u32 = 774;
    pub const MONEY: u32 = 790;
    pub const MACADDR: u32 = 829;
    pub const INET: u32 = 869;
    pub const BPCHAR: u32 = 1042;
    pub const VARCHAR: u32 = 1043;
    pub const DATE: u32 = 1082;
    pub const TIME: u32 = 1083;
    pub const TIMESTAMP: u32 = 1114;
    pub const TIMESTAMPTZ: u32 = 1184;
    pub const INTERVAL: u32 = 1186;
    pub const TIMETZ: u32 = 1266;
    pub const BIT: u32 = 1560;
    pub const VARBIT: u32 = 1562;
    pub const NUMERIC: u32 = 1700;
    pub const VOID: u32 = 2278;
    pub const UUID: u32 = 2950;
    pub const JSONB: u32 = 3802;
    pub const INT4RANGE: u32 = 3904;
    pub const NUMRANGE: u32 = 3906;
    pub const TSRANGE: u32 = 3908;
    pub const TSTZRANGE: u32 = 3910;
    pub const DATERANGE: u32 = 3912;
    pub const INT8RANGE: u32 = 3926;
    pub const INT4MULTIRANGE: u32 = 4451;
    pub const NUMMULTIRANGE: u32 = 4532;
    pub const TSMULTIRANGE: u32 = 4533;
    pub const TSTZMULTIRANGE: u32 = 4534;
    pub const DATEMULTIRANGE: u32 = 4535;
    pub const INT8MULTIRANGE: u32 = 4536;
    pub const XID8: u32 = 5069;
}

/// How values of one column are converted into [`dbiji_core::Value`]s.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Conv {
    Bool,
    Int2,
    Int4,
    Int8,
    /// Unsigned 32 bit (oid, xid, cid).
    Oid,
    /// Unsigned 64 bit (xid8).
    Xid8,
    Float4,
    Float8,
    Numeric,
    Money,
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
    Interval,
    Uuid,
    Json,
    Jsonb,
    Bytea,
    /// text, varchar, bpchar, name, unknown, citext ...
    Text,
    /// The single byte `"char"` type.
    Char,
    Xml,
    Inet,
    Cidr,
    MacAddr,
    MacAddr8,
    Bit,
    Enum,
    Range(Box<Conv>),
    Multirange(Box<Conv>),
    Array(Box<Conv>),
    Composite(Vec<Conv>),
    Geometry,
    Hstore,
    /// Anything else: server text form, binary shown as hex.
    Other,
}

impl Conv {
    /// Conversion plan for a column of type `ty`.
    pub(crate) fn for_type(ty: &Type) -> Conv {
        Self::for_type_depth(ty, 0)
    }

    fn for_type_depth(ty: &Type, depth: usize) -> Conv {
        if depth > 8 {
            return Conv::Other;
        }
        match ty.oid() {
            oid::BOOL => return Conv::Bool,
            oid::INT2 => return Conv::Int2,
            oid::INT4 => return Conv::Int4,
            oid::INT8 => return Conv::Int8,
            oid::OID | oid::XID | oid::CID => return Conv::Oid,
            oid::XID8 => return Conv::Xid8,
            oid::FLOAT4 => return Conv::Float4,
            oid::FLOAT8 => return Conv::Float8,
            oid::NUMERIC => return Conv::Numeric,
            oid::MONEY => return Conv::Money,
            oid::DATE => return Conv::Date,
            oid::TIME => return Conv::Time,
            oid::TIMETZ => return Conv::TimeTz,
            oid::TIMESTAMP => return Conv::Timestamp,
            oid::TIMESTAMPTZ => return Conv::TimestampTz,
            oid::INTERVAL => return Conv::Interval,
            oid::UUID => return Conv::Uuid,
            oid::JSON => return Conv::Json,
            oid::JSONB => return Conv::Jsonb,
            oid::BYTEA => return Conv::Bytea,
            oid::TEXT | oid::VARCHAR | oid::BPCHAR | oid::NAME | oid::UNKNOWN => return Conv::Text,
            oid::CHAR => return Conv::Char,
            oid::XML => return Conv::Xml,
            oid::INET => return Conv::Inet,
            oid::CIDR => return Conv::Cidr,
            oid::MACADDR => return Conv::MacAddr,
            oid::MACADDR8 => return Conv::MacAddr8,
            oid::BIT | oid::VARBIT => return Conv::Bit,
            _ => {}
        }
        match ty.kind() {
            Kind::Enum(_) => Conv::Enum,
            Kind::Array(elem) => Conv::Array(Box::new(Self::for_type_depth(elem, depth + 1))),
            Kind::Range(sub) => Conv::Range(Box::new(Self::for_type_depth(sub, depth + 1))),
            Kind::Multirange(sub) => Conv::Multirange(Box::new(Self::for_type_depth(sub, depth + 1))),
            Kind::Domain(base) => Self::for_type_depth(base, depth + 1),
            Kind::Composite(fields) => {
                Conv::Composite(fields.iter().map(|f| Self::for_type_depth(f.type_(), depth + 1)).collect())
            }
            _ => match ty.name() {
                "citext" => Conv::Text,
                "geometry" | "geography" => Conv::Geometry,
                "hstore" => Conv::Hstore,
                _ => Conv::Other,
            },
        }
    }

    /// Logical kind reported in [`dbiji_core::ColumnMeta::kind`].
    pub(crate) fn kind(&self) -> DataKind {
        match self {
            Conv::Bool => DataKind::Bool,
            Conv::Int2 | Conv::Int4 | Conv::Int8 | Conv::Oid | Conv::Xid8 => DataKind::Int,
            Conv::Float4 | Conv::Float8 => DataKind::Float,
            Conv::Numeric | Conv::Money => DataKind::Decimal,
            Conv::Date => DataKind::Date,
            Conv::Time => DataKind::Time,
            Conv::TimeTz => DataKind::TimeTz,
            Conv::Timestamp => DataKind::DateTime,
            Conv::TimestampTz => DataKind::DateTimeTz,
            Conv::Interval => DataKind::Interval,
            Conv::Uuid => DataKind::Uuid,
            Conv::Json | Conv::Jsonb => DataKind::Json,
            Conv::Bytea => DataKind::Binary,
            Conv::Text | Conv::Char => DataKind::Text,
            Conv::Xml => DataKind::Xml,
            Conv::Inet | Conv::Cidr | Conv::MacAddr | Conv::MacAddr8 => DataKind::Network,
            Conv::Bit => DataKind::Bit,
            Conv::Enum => DataKind::Enum,
            Conv::Range(_) | Conv::Multirange(_) => DataKind::Range,
            Conv::Array(_) => DataKind::Array,
            Conv::Geometry => DataKind::Geometry,
            Conv::Composite(_) | Conv::Hstore | Conv::Other => DataKind::Other,
        }
    }
}

/// Precision / scale / length derived from a type modifier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TypmodInfo {
    pub precision: Option<u32>,
    pub scale: Option<u32>,
    pub max_length: Option<u32>,
}

/// Decode the type modifier of built-in types.
pub(crate) fn typmod_info(type_oid: u32, typmod: i32) -> TypmodInfo {
    let mut info = TypmodInfo::default();
    if typmod < 0 {
        return info;
    }
    match type_oid {
        oid::VARCHAR | oid::BPCHAR if typmod >= 4 => info.max_length = Some((typmod - 4) as u32),
        oid::NUMERIC if typmod >= 4 => {
            let t = typmod - 4;
            info.precision = Some(((t >> 16) & 0xffff) as u32);
            // scale is a signed 11 bit field since PostgreSQL 15
            let scale = (((t & 0x7ff) ^ 1024) - 1024).max(0);
            info.scale = Some(scale as u32);
        }
        oid::TIME | oid::TIMETZ | oid::TIMESTAMP | oid::TIMESTAMPTZ => info.precision = Some(typmod as u32),
        oid::INTERVAL => {
            let p = typmod & 0xffff;
            if p != 0xffff {
                info.precision = Some(p as u32);
            }
        }
        oid::BIT | oid::VARBIT => info.max_length = Some(typmod as u32),
        _ => {}
    }
    info
}

/// Short display name of a type, e.g. `int4`, `varchar(20)`, `numeric(10,2)`,
/// `timestamptz(3)`, `int4[]`, `geometry(Point,4326)`.
pub(crate) fn display_name(ty: &Type, typmod: i32) -> String {
    if let Kind::Array(elem) = ty.kind() {
        return format!("{}[]", display_name(elem, typmod));
    }
    let base = ty.name();
    let suffix = modifier_text(ty.oid(), base, typmod);
    format!("{base}{suffix}")
}

/// Text of the type modifier (`(255)`, `(10,2)`, `(Point,4326)`), empty when none.
pub(crate) fn modifier_text(type_oid: u32, type_name: &str, typmod: i32) -> String {
    if typmod < 0 {
        return String::new();
    }
    match type_oid {
        oid::VARCHAR | oid::BPCHAR if typmod >= 4 => format!("({})", typmod - 4),
        oid::NUMERIC if typmod >= 4 => {
            let t = typmod - 4;
            let p = (t >> 16) & 0xffff;
            let s = ((t & 0x7ff) ^ 1024) - 1024;
            format!("({p},{s})")
        }
        oid::TIME | oid::TIMETZ | oid::TIMESTAMP | oid::TIMESTAMPTZ => format!("({typmod})"),
        oid::INTERVAL => interval_modifier(typmod),
        oid::BIT | oid::VARBIT => format!("({typmod})"),
        _ if type_name == "geometry" || type_name == "geography" => postgis_modifier(typmod),
        _ => String::new(),
    }
}

fn interval_modifier(typmod: i32) -> String {
    const YEAR: i32 = 1 << 2;
    const MONTH: i32 = 1 << 1;
    const DAY: i32 = 1 << 3;
    const HOUR: i32 = 1 << 10;
    const MINUTE: i32 = 1 << 11;
    const SECOND: i32 = 1 << 12;
    let range = (typmod >> 16) & 0x7fff;
    let precision = typmod & 0xffff;
    let fields = match range {
        r if r == YEAR => " year",
        r if r == MONTH => " month",
        r if r == DAY => " day",
        r if r == HOUR => " hour",
        r if r == MINUTE => " minute",
        r if r == SECOND => " second",
        r if r == YEAR | MONTH => " year to month",
        r if r == DAY | HOUR => " day to hour",
        r if r == DAY | HOUR | MINUTE => " day to minute",
        r if r == DAY | HOUR | MINUTE | SECOND => " day to second",
        r if r == HOUR | MINUTE => " hour to minute",
        r if r == HOUR | MINUTE | SECOND => " hour to second",
        r if r == MINUTE | SECOND => " minute to second",
        _ => "",
    };
    if precision != 0xffff { format!("{fields}({precision})") } else { fields.to_string() }
}

/// PostGIS geometry type names indexed by type number.
const POSTGIS_TYPES: [&str; 16] = [
    "Geometry",
    "Point",
    "LineString",
    "Polygon",
    "MultiPoint",
    "MultiLineString",
    "MultiPolygon",
    "GeometryCollection",
    "CircularString",
    "CompoundCurve",
    "CurvePolygon",
    "MultiCurve",
    "MultiSurface",
    "PolyhedralSurface",
    "Triangle",
    "Tin",
];

fn postgis_modifier(typmod: i32) -> String {
    let srid = ((typmod & 0x0FFF_FF00) - (typmod & 0x1000_0000)) >> 8;
    let ty = ((typmod & 0x0000_00FC) >> 2) as usize;
    let z = (typmod & 0x2) != 0;
    let m = (typmod & 0x1) != 0;
    if srid == 0 && ty == 0 && !z && !m {
        return String::new();
    }
    let mut s = String::from("(");
    s.push_str(POSTGIS_TYPES.get(ty).copied().unwrap_or("Geometry"));
    if z {
        s.push('Z');
    }
    if m {
        s.push('M');
    }
    if srid != 0 {
        s.push(',');
        s.push_str(&srid.to_string());
    }
    s.push(')');
    s
}

/// Logical kind from a PostgreSQL type name as used in DDL and catalogs
/// (`character varying(10)`, `timestamp with time zone`, `_int4`, `int4[]`).
pub(crate) fn kind_from_type_name(type_name: &str) -> DataKind {
    let t = type_name.trim().to_ascii_lowercase();
    if t.ends_with(']') || t.starts_with('_') {
        return DataKind::Array;
    }
    // strip schema qualification and modifiers: "timestamp(3) with time zone" -> "timestamp with time zone"
    let mut no_mods = String::with_capacity(t.len());
    let mut depth = 0usize;
    for c in t.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 => no_mods.push(c),
            _ => {}
        }
    }
    let no_mods = no_mods.split_whitespace().collect::<Vec<_>>().join(" ");
    let unq = no_mods.rsplit_once('.').map(|(_, n)| n).unwrap_or(&no_mods).trim_matches('"').to_string();
    let base = unq.trim();
    match base {
        "bool" | "boolean" => DataKind::Bool,
        "bit" | "varbit" | "bit varying" => DataKind::Bit,
        "smallint" | "integer" | "int" | "bigint" | "int2" | "int4" | "int8" | "smallserial"
        | "serial" | "bigserial" | "serial2" | "serial4" | "serial8" | "oid" | "xid" | "xid8"
        | "cid" => DataKind::Int,
        "real" | "float4" | "float8" | "double precision" | "float" => DataKind::Float,
        "numeric" | "decimal" | "money" => DataKind::Decimal,
        "date" => DataKind::Date,
        "time" | "time without time zone" => DataKind::Time,
        "timetz" | "time with time zone" => DataKind::TimeTz,
        "timestamp" | "timestamp without time zone" => DataKind::DateTime,
        "timestamptz" | "timestamp with time zone" => DataKind::DateTimeTz,
        "interval" => DataKind::Interval,
        "uuid" => DataKind::Uuid,
        "json" | "jsonb" => DataKind::Json,
        "xml" => DataKind::Xml,
        "bytea" => DataKind::Binary,
        "inet" | "cidr" | "macaddr" | "macaddr8" => DataKind::Network,
        "geometry" | "geography" => DataKind::Geometry,
        "int4range" | "int8range" | "numrange" | "tsrange" | "tstzrange" | "daterange"
        | "int4multirange" | "int8multirange" | "nummultirange" | "tsmultirange"
        | "tstzmultirange" | "datemultirange" => DataKind::Range,
        "text" | "varchar" | "character varying" | "char" | "character" | "bpchar" | "name"
        | "citext" | "\"char\"" => DataKind::Text,
        _ if base.starts_with("interval") => DataKind::Interval,
        _ if base.starts_with("timestamp") && base.contains("with time zone") && !base.contains("without") => {
            DataKind::DateTimeTz
        }
        _ if base.starts_with("timestamp") => DataKind::DateTime,
        _ if base.starts_with("time") && base.contains("with time zone") && !base.contains("without") => {
            DataKind::TimeTz
        }
        _ if base.starts_with("time") => DataKind::Time,
        _ if base.starts_with("character varying") || base.starts_with("character") => DataKind::Text,
        _ if base.starts_with("bit varying") => DataKind::Bit,
        _ => DataKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typmods() {
        assert_eq!(display_name(&Type::VARCHAR, 259), "varchar(255)");
        assert_eq!(display_name(&Type::BPCHAR, 14), "bpchar(10)");
        assert_eq!(display_name(&Type::NUMERIC, (10 << 16) + 2 + 4), "numeric(10,2)");
        assert_eq!(display_name(&Type::NUMERIC, -1), "numeric");
        assert_eq!(display_name(&Type::TIMESTAMPTZ, 3), "timestamptz(3)");
        assert_eq!(display_name(&Type::INT4_ARRAY, -1), "int4[]");
        assert_eq!(display_name(&Type::VARCHAR_ARRAY, 14), "varchar(10)[]");
        assert_eq!(display_name(&Type::BIT, 8), "bit(8)");
        let i = typmod_info(oid::NUMERIC, (12 << 16) + 4 + 4);
        assert_eq!((i.precision, i.scale), (Some(12), Some(4)));
        assert_eq!(typmod_info(oid::VARCHAR, 24).max_length, Some(20));
        // interval day to second(3)
        let tm = ((8 | 1024 | 2048 | 4096) << 16) | 3;
        assert_eq!(modifier_text(oid::INTERVAL, "interval", tm), " day to second(3)");
    }

    #[test]
    fn postgis_typmods() {
        // geometry(Point,4326): srid << 8 | type << 2
        let tm = (4326 << 8) | (1 << 2);
        assert_eq!(modifier_text(99999, "geometry", tm), "(Point,4326)");
        let tm = (4326 << 8) | (3 << 2) | 2;
        assert_eq!(modifier_text(99999, "geography", tm), "(PolygonZ,4326)");
        assert_eq!(modifier_text(99999, "geometry", -1), "");
    }

    #[test]
    fn conv_kinds() {
        assert_eq!(Conv::for_type(&Type::INT4).kind(), DataKind::Int);
        assert_eq!(Conv::for_type(&Type::TEXT_ARRAY), Conv::Array(Box::new(Conv::Text)));
        assert_eq!(Conv::for_type(&Type::INT4_RANGE).kind(), DataKind::Range);
        assert_eq!(Conv::for_type(&Type::MONEY).kind(), DataKind::Decimal);
        let geo = Type::new("geometry".into(), 70000, Kind::Simple, "public".into());
        assert_eq!(Conv::for_type(&geo), Conv::Geometry);
        let en = Type::new("mood".into(), 70001, Kind::Enum(vec!["a".into()]), "public".into());
        assert_eq!(Conv::for_type(&en).kind(), DataKind::Enum);
        let other = Type::new("tsvector".into(), 3614, Kind::Simple, "pg_catalog".into());
        assert_eq!(Conv::for_type(&other).kind(), DataKind::Other);
    }

    #[test]
    fn kinds_from_names() {
        assert_eq!(kind_from_type_name("character varying(20)"), DataKind::Text);
        assert_eq!(kind_from_type_name("timestamp(3) with time zone"), DataKind::DateTimeTz);
        assert_eq!(kind_from_type_name("timestamp without time zone"), DataKind::DateTime);
        assert_eq!(kind_from_type_name("time with time zone"), DataKind::TimeTz);
        assert_eq!(kind_from_type_name("bit(1)"), DataKind::Bit);
        assert_eq!(kind_from_type_name("integer[]"), DataKind::Array);
        assert_eq!(kind_from_type_name("_int4"), DataKind::Array);
        assert_eq!(kind_from_type_name("public.geometry(Point,4326)"), DataKind::Geometry);
        assert_eq!(kind_from_type_name("money"), DataKind::Decimal);
        assert_eq!(kind_from_type_name("interval day to second"), DataKind::Interval);
        assert_eq!(kind_from_type_name("tsvector"), DataKind::Other);
    }
}
