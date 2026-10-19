//! Catalog folders of PostgreSQL and CockroachDB (see `dbiji_core::catalog`):
//! roles, tablespaces, settings, foreign data wrappers and servers, event
//! triggers, languages, collations, access methods, available extensions,
//! and per table the row level security policies and rules.

use dbiji_core::catalog::{CatalogFolder as F, CatalogGroup as G, CatalogIcon as I, CatalogScope as S};

const ROLE_DDL: &str = "SELECT 'CREATE ROLE ' || quote_ident(r.rolname) || ' WITH'
  || CASE WHEN r.rolsuper THEN ' SUPERUSER' ELSE ' NOSUPERUSER' END
  || CASE WHEN r.rolinherit THEN ' INHERIT' ELSE ' NOINHERIT' END
  || CASE WHEN r.rolcreaterole THEN ' CREATEROLE' ELSE ' NOCREATEROLE' END
  || CASE WHEN r.rolcreatedb THEN ' CREATEDB' ELSE ' NOCREATEDB' END
  || CASE WHEN r.rolcanlogin THEN ' LOGIN' ELSE ' NOLOGIN' END
  || CASE WHEN r.rolreplication THEN ' REPLICATION' ELSE ' NOREPLICATION' END
  || CASE WHEN r.rolbypassrls THEN ' BYPASSRLS' ELSE ' NOBYPASSRLS' END
  || CASE WHEN r.rolconnlimit <> -1 THEN ' CONNECTION LIMIT ' || r.rolconnlimit ELSE '' END
  || CASE WHEN r.rolvaliduntil IS NOT NULL THEN ' VALID UNTIL ' || quote_literal(r.rolvaliduntil::text) ELSE '' END
FROM pg_roles r WHERE r.rolname = {name}";

const ROLE_GRANTS_DDL: &str = "SELECT 'GRANT ' || quote_ident(g.rolname) || ' TO ' || quote_ident(m.rolname)
  || CASE WHEN a.admin_option THEN ' WITH ADMIN OPTION' ELSE '' END
FROM pg_auth_members a JOIN pg_roles g ON g.oid = a.roleid JOIN pg_roles m ON m.oid = a.member
WHERE m.rolname = {name} ORDER BY g.rolname";

const ROLE_COMMENT_DDL: &str = "SELECT 'COMMENT ON ROLE ' || quote_ident(rolname) || ' IS ' || quote_literal(shobj_description(oid, 'pg_authid'))
FROM pg_roles WHERE rolname = {name} AND shobj_description(oid, 'pg_authid') IS NOT NULL";

const OPTIONS: &str = "(SELECT string_agg(quote_ident(split_part(o, '=', 1)) || ' ' || quote_literal(substr(o, strpos(o, '=') + 1)), ', ') FROM unnest(OPTS) o)";

fn options_of(column: &str) -> String {
    OPTIONS.replace("OPTS", column)
}

/// Folders for PostgreSQL (`cockroach`: the subset CockroachDB supports).
pub fn folders(cockroach: bool) -> Vec<F> {
    let mut out = vec![
        F::new(
            "pg.roles",
            "Roles",
            G::Security,
            S::Server,
            I::Users,
            "SELECT rolname AS name,
  CASE WHEN rolcanlogin THEN 'login' ELSE 'group' END || CASE WHEN rolsuper THEN ', superuser' ELSE '' END AS detail,
  rolsuper AS superuser, rolinherit AS inherit, rolcreaterole AS create_role, rolcreatedb AS create_db,
  rolcanlogin AS can_login, rolreplication AS replication, rolbypassrls AS bypass_rls,
  rolconnlimit AS connection_limit, rolvaliduntil AS valid_until
FROM pg_roles ORDER BY rolname",
        )
        .ddl(ROLE_DDL)
        .ddl(ROLE_GRANTS_DDL)
        .children(&["pg.role_members", "pg.role_member_of"]),
        F::new(
            "pg.role_members",
            "Members",
            G::None,
            S::Child,
            I::User,
            "SELECT m.rolname AS name, CASE WHEN a.admin_option THEN 'admin option' END AS detail
FROM pg_auth_members a JOIN pg_roles g ON g.oid = a.roleid JOIN pg_roles m ON m.oid = a.member
WHERE g.rolname = {parent} ORDER BY 1",
        ),
        F::new(
            "pg.role_member_of",
            "Member of",
            G::None,
            S::Child,
            I::Users,
            "SELECT g.rolname AS name, CASE WHEN a.admin_option THEN 'admin option' END AS detail
FROM pg_auth_members a JOIN pg_roles g ON g.oid = a.roleid JOIN pg_roles m ON m.oid = a.member
WHERE m.rolname = {parent} ORDER BY 1",
        ),
        F::new(
            "pg.settings",
            "Settings",
            G::Administer,
            S::Server,
            I::Settings,
            "SELECT name, setting || COALESCE(' ' || unit, '') AS detail, setting, unit, category, short_desc AS description,
  context, source, boot_val AS default_value, reset_val AS reset_value
FROM pg_settings ORDER BY name",
        )
        .table_only(),
    ];
    if cockroach {
        return out;
    }
    out[0] = out[0].clone().ddl(ROLE_COMMENT_DDL);
    out.extend([
        F::new(
            "pg.tablespaces",
            "Tablespaces",
            G::Storage,
            S::Server,
            I::Storage,
            "SELECT spcname AS name, NULLIF(pg_tablespace_location(oid), '') AS detail, pg_get_userbyid(spcowner) AS owner,
  pg_tablespace_location(oid) AS location, array_to_string(spcoptions, ', ') AS options,
  shobj_description(oid, 'pg_tablespace') AS comment
FROM pg_tablespace ORDER BY spcname",
        )
        .ddl(
            "SELECT 'CREATE TABLESPACE ' || quote_ident(spcname) || ' OWNER ' || quote_ident(pg_get_userbyid(spcowner))
  || ' LOCATION ' || quote_literal(pg_tablespace_location(oid))
  || COALESCE(' WITH (' || array_to_string(spcoptions, ', ') || ')', '')
FROM pg_tablespace WHERE spcname = {name}",
        ),
        F::new(
            "pg.event_triggers",
            "Event Triggers",
            G::None,
            S::Database,
            I::Trigger,
            "SELECT e.evtname AS name, e.evtevent AS detail, e.evtevent AS event, e.evtfoid::regproc::text AS function,
  CASE e.evtenabled WHEN 'O' THEN 'enabled' WHEN 'D' THEN 'disabled' WHEN 'R' THEN 'replica' WHEN 'A' THEN 'always' END AS enabled,
  array_to_string(e.evttags, ', ') AS tags, pg_get_userbyid(e.evtowner) AS owner
FROM pg_event_trigger e ORDER BY 1",
        )
        .ddl(
            "SELECT 'CREATE EVENT TRIGGER ' || quote_ident(e.evtname) || ' ON ' || e.evtevent
  || CASE WHEN e.evttags IS NOT NULL THEN ' WHEN TAG IN (' || array_to_string(ARRAY(SELECT quote_literal(t) FROM unnest(e.evttags) t), ', ') || ')' ELSE '' END
  || ' EXECUTE FUNCTION ' || e.evtfoid::regproc::text || '()'
FROM pg_event_trigger e WHERE e.evtname = {name}",
        ),
        F::new(
            "pg.fdw",
            "Foreign Data Wrappers",
            G::None,
            S::Database,
            I::Plug,
            "SELECT fdwname AS name, NULLIF(fdwhandler, 0)::regproc::text AS detail, pg_get_userbyid(fdwowner) AS owner,
  NULLIF(fdwhandler, 0)::regproc::text AS handler, NULLIF(fdwvalidator, 0)::regproc::text AS validator,
  array_to_string(fdwoptions, ', ') AS options
FROM pg_foreign_data_wrapper ORDER BY 1",
        )
        .ddl(format!(
            "SELECT 'CREATE FOREIGN DATA WRAPPER ' || quote_ident(fdwname)
  || CASE WHEN fdwhandler <> 0 THEN ' HANDLER ' || fdwhandler::regproc::text ELSE '' END
  || CASE WHEN fdwvalidator <> 0 THEN ' VALIDATOR ' || fdwvalidator::regproc::text ELSE '' END
  || COALESCE(' OPTIONS (' || {} || ')', '')
FROM pg_foreign_data_wrapper WHERE fdwname = {{name}}",
            options_of("fdwoptions")
        )),
        F::new(
            "pg.foreign_servers",
            "Foreign Servers",
            G::None,
            S::Database,
            I::Server,
            "SELECT s.srvname AS name, w.fdwname AS detail, w.fdwname AS wrapper, pg_get_userbyid(s.srvowner) AS owner,
  s.srvtype AS type, s.srvversion AS version, array_to_string(s.srvoptions, ', ') AS options
FROM pg_foreign_server s JOIN pg_foreign_data_wrapper w ON w.oid = s.srvfdw ORDER BY 1",
        )
        .ddl(format!(
            "SELECT 'CREATE SERVER ' || quote_ident(s.srvname)
  || COALESCE(' TYPE ' || quote_literal(s.srvtype), '') || COALESCE(' VERSION ' || quote_literal(s.srvversion), '')
  || ' FOREIGN DATA WRAPPER ' || quote_ident(w.fdwname)
  || COALESCE(' OPTIONS (' || {} || ')', '')
FROM pg_foreign_server s JOIN pg_foreign_data_wrapper w ON w.oid = s.srvfdw WHERE s.srvname = {{name}}",
            options_of("s.srvoptions")
        ))
        .ddl(format!(
            "SELECT 'CREATE USER MAPPING FOR ' || CASE WHEN u.umuser = 0 THEN 'PUBLIC' ELSE quote_ident(u.usename) END
  || ' SERVER ' || quote_ident(u.srvname) || COALESCE(' OPTIONS (' || {} || ')', '')
FROM pg_user_mappings u WHERE u.srvname = {{name}} ORDER BY u.usename",
            options_of("u.umoptions")
        ))
        .children(&["pg.user_mappings"]),
        F::new(
            "pg.user_mappings",
            "User Mappings",
            G::None,
            S::Child,
            I::User,
            "SELECT CASE WHEN umuser = 0 THEN 'PUBLIC' ELSE usename END AS name, array_to_string(umoptions, ', ') AS detail
FROM pg_user_mappings WHERE srvname = {parent} ORDER BY 1",
        ),
        F::new(
            "pg.languages",
            "Languages",
            G::None,
            S::Database,
            I::Language,
            "SELECT lanname AS name, CASE WHEN lanpltrusted THEN 'trusted' ELSE 'untrusted' END AS detail,
  pg_get_userbyid(lanowner) AS owner, lanispl AS procedural, lanpltrusted AS trusted
FROM pg_language ORDER BY 1",
        ),
        F::new(
            "pg.collations",
            "Collations",
            G::None,
            S::Database,
            I::Text,
            "SELECT c.collname AS name, n.nspname AS detail, n.nspname AS schema,
  CASE c.collprovider WHEN 'c' THEN 'libc' WHEN 'i' THEN 'icu' WHEN 'b' THEN 'builtin' ELSE 'default' END AS provider,
  c.collcollate AS lc_collate, c.collctype AS lc_ctype, c.collisdeterministic AS deterministic
FROM pg_collation c JOIN pg_namespace n ON n.oid = c.collnamespace ORDER BY 1, 2",
        ),
        F::new(
            "pg.access_methods",
            "Access Methods",
            G::None,
            S::Database,
            I::Key,
            "SELECT amname AS name, CASE amtype WHEN 'i' THEN 'index' WHEN 't' THEN 'table' END AS detail, amhandler::regproc::text AS handler
FROM pg_am ORDER BY 1",
        ),
        F::new(
            "pg.available_extensions",
            "Available Extensions",
            G::None,
            S::Database,
            I::Puzzle,
            "SELECT name, default_version AS detail, default_version, installed_version, comment
FROM pg_available_extensions ORDER BY name",
        )
        .table_only(),
        F::new(
            "pg.policies",
            "Policies",
            G::None,
            S::Table,
            I::Shield,
            "SELECT policyname AS name, cmd AS detail, permissive, cmd AS command, array_to_string(roles, ', ') AS roles,
  qual AS using_expression, with_check AS check_expression
FROM pg_policies WHERE schemaname = {schema} AND tablename = {table} ORDER BY 1",
        )
        .ddl(
            "SELECT 'CREATE POLICY ' || quote_ident(policyname) || ' ON ' || quote_ident(schemaname) || '.' || quote_ident(tablename)
  || ' AS ' || permissive || ' FOR ' || cmd
  || ' TO ' || array_to_string(ARRAY(SELECT CASE WHEN r = 'public' THEN 'PUBLIC' ELSE quote_ident(r) END FROM unnest(roles) r), ', ')
  || COALESCE(' USING (' || qual || ')', '') || COALESCE(' WITH CHECK (' || with_check || ')', '')
FROM pg_policies WHERE schemaname = {schema} AND tablename = {table} AND policyname = {name}",
        ),
        F::new(
            "pg.rules",
            "Rules",
            G::None,
            S::Table,
            I::Rule,
            "SELECT rulename AS name, NULL::text AS detail, definition
FROM pg_rules WHERE schemaname = {schema} AND tablename = {table} ORDER BY 1",
        )
        .ddl("SELECT definition FROM pg_rules WHERE schemaname = {schema} AND tablename = {table} AND rulename = {name}"),
    ]);
    out
}
