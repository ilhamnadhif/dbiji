//! Importers (FR-C8): DBeaver workspaces, `pgpass` files and connection URLs.
//!
//! Importers return an [`ImportResult`]; the application merges it into its
//! [`ConnectionStore`] with [`merge_into_store`] and saves the returned
//! secrets in its secret store.

pub mod dbeaver;
pub mod pgpass;
pub mod url;

pub use dbeaver::{
    import_project as import_dbeaver_project, import_workspace as import_dbeaver_workspace,
};
pub use pgpass::{PgPass, PgPassEntry, import_pgpass};
pub use url::{parse_connection_url, to_url};

use crate::connections::{ConnectionStore, normalize_folder};
use dbiji_core::Secret;
use dbiji_core::config::{ConnectionConfig, ConnectionType, NetworkProfile};

/// Everything an importer found.
#[derive(Debug, Clone, Default)]
pub struct ImportResult {
    /// Imported connections (secrets not included).
    pub connections: Vec<ConnectionConfig>,
    /// `(secret key, value)` pairs to store (keys from `dbiji_core::config::secret_key`).
    pub secrets: Vec<(String, Secret)>,
    /// Navigator folders (`/` separated).
    pub folders: Vec<String>,
    /// User defined connection types.
    pub connection_types: Vec<ConnectionType>,
    /// Network profiles.
    pub network_profiles: Vec<NetworkProfile>,
    /// Human readable notes (skipped entries, unsupported options ...).
    pub warnings: Vec<String>,
}

impl ImportResult {
    /// True when nothing was found.
    pub fn is_empty(&self) -> bool {
        self.connections.is_empty() && self.folders.is_empty() && self.connection_types.is_empty()
    }

    /// Appends another result.
    pub fn merge(&mut self, other: ImportResult) {
        for f in other.folders {
            if !self.folders.contains(&f) {
                self.folders.push(f);
            }
        }
        for t in other.connection_types {
            if !self.connection_types.iter().any(|x| x.id == t.id) {
                self.connection_types.push(t);
            }
        }
        self.connections.extend(other.connections);
        self.secrets.extend(other.secrets);
        self.network_profiles.extend(other.network_profiles);
        self.warnings.extend(other.warnings);
    }

    /// Places every folder and connection under `prefix/`.
    pub fn prefix_folders(&mut self, prefix: &str) {
        let Some(prefix) = normalize_folder(prefix) else {
            return;
        };
        for f in &mut self.folders {
            *f = format!("{prefix}/{f}");
        }
        if !self.folders.contains(&prefix) {
            self.folders.insert(0, prefix.clone());
        }
        for c in &mut self.connections {
            c.folder = Some(match &c.folder {
                Some(f) => format!("{prefix}/{f}"),
                None => prefix.clone(),
            });
        }
    }
}

/// What to do with an imported connection that already exists (same id or
/// same endpoint: kind, host, port, database, user, file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DuplicatePolicy {
    /// Keep the existing connection.
    #[default]
    Skip,
    /// Overwrite the existing connection (same id kept).
    Replace,
    /// Import as an additional connection with a new id.
    KeepBoth,
}

/// Outcome of [`merge_into_store`].
#[derive(Debug, Default)]
pub struct MergeSummary {
    /// Connections added as new.
    pub added: usize,
    /// Existing connections overwritten.
    pub replaced: usize,
    /// Duplicates left untouched.
    pub skipped: usize,
    /// Secrets of the connections that were added or replaced, with keys
    /// adjusted to the final connection ids. Store them in the secret store.
    pub secrets: Vec<(String, Secret)>,
}

fn same_endpoint(a: &ConnectionConfig, b: &ConnectionConfig) -> bool {
    a.kind == b.kind
        && a.host.eq_ignore_ascii_case(&b.host)
        && a.effective_port() == b.effective_port()
        && a.database == b.database
        && a.user == b.user
        && a.file_path == b.file_path
}

/// Merges an import into the store according to `policy`.
pub fn merge_into_store(
    store: &mut ConnectionStore,
    result: ImportResult,
    policy: DuplicatePolicy,
) -> MergeSummary {
    let mut summary = MergeSummary::default();
    for f in &result.folders {
        store.add_folder(f);
    }
    for t in result.connection_types {
        if !store.has_connection_type(&t.id) {
            store.set_connection_type(t);
        }
    }
    for p in result.network_profiles {
        if store.network_profile(&p.id).is_none() {
            store.set_network_profile(p);
        }
    }
    let secrets_of = |id: &str| -> Vec<(String, Secret)> {
        let prefix = format!("conn:{id}:");
        result
            .secrets
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .cloned()
            .collect()
    };
    for mut cfg in result.connections {
        let existing_id = store.get(&cfg.id).map(|c| c.id.clone()).or_else(|| {
            store
                .connections
                .iter()
                .find(|c| same_endpoint(c, &cfg))
                .map(|c| c.id.clone())
        });
        let original_id = cfg.id.clone();
        match (existing_id, policy) {
            (None, _) => {
                if store.add(cfg).is_ok() {
                    summary.added += 1;
                    summary.secrets.extend(secrets_of(&original_id));
                }
            }
            (Some(_), DuplicatePolicy::Skip) => summary.skipped += 1,
            (Some(existing), DuplicatePolicy::Replace) => {
                cfg.id = existing.clone();
                if store.update(cfg).is_ok() {
                    summary.replaced += 1;
                    let from = format!("conn:{original_id}:");
                    let to = format!("conn:{existing}:");
                    summary.secrets.extend(
                        secrets_of(&original_id)
                            .into_iter()
                            .map(|(k, v)| (k.replacen(&from, &to, 1), v)),
                    );
                }
            }
            (Some(_), DuplicatePolicy::KeepBoth) => {
                let new_id = uuid::Uuid::new_v4().to_string();
                cfg.id = new_id.clone();
                cfg.name = store.unique_name(&cfg.name, cfg.folder.as_deref());
                if store.add(cfg).is_ok() {
                    summary.added += 1;
                    let from = format!("conn:{original_id}:");
                    let to = format!("conn:{new_id}:");
                    summary.secrets.extend(
                        secrets_of(&original_id)
                            .into_iter()
                            .map(|(k, v)| (k.replacen(&from, &to, 1), v)),
                    );
                }
            }
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::DbKind;

    fn result_with(id: &str, host: &str) -> ImportResult {
        let mut c = ConnectionConfig::new(DbKind::Postgres, "Imported");
        c.id = id.into();
        c.host = host.into();
        c.folder = Some("F".into());
        ImportResult {
            connections: vec![c],
            secrets: vec![(format!("conn:{id}:password"), Secret::new("pw"))],
            folders: vec!["F".into()],
            ..Default::default()
        }
    }

    #[test]
    fn merge_policies() {
        let mut store = ConnectionStore::new();
        let s = merge_into_store(&mut store, result_with("x1", "h"), DuplicatePolicy::Skip);
        assert_eq!((s.added, s.skipped), (1, 0));
        assert_eq!(s.secrets.len(), 1);
        assert!(store.folders.contains(&"F".to_string()));

        // Same id again: skipped.
        let s = merge_into_store(&mut store, result_with("x1", "h"), DuplicatePolicy::Skip);
        assert_eq!((s.added, s.skipped), (0, 1));
        assert!(s.secrets.is_empty());

        // Same endpoint with another id: replaced in place, secrets re-keyed.
        let s = merge_into_store(
            &mut store,
            result_with("other", "h"),
            DuplicatePolicy::Replace,
        );
        assert_eq!(s.replaced, 1);
        assert_eq!(s.secrets[0].0, "conn:x1:password");
        assert_eq!(store.connections.len(), 1);

        let s = merge_into_store(
            &mut store,
            result_with("x1", "h"),
            DuplicatePolicy::KeepBoth,
        );
        assert_eq!(s.added, 1);
        assert_eq!(store.connections.len(), 2);
        assert_ne!(store.connections[1].id, "x1");
        assert_eq!(store.connections[1].name, "Imported 2");
        assert_eq!(
            s.secrets[0].0,
            format!("conn:{}:password", store.connections[1].id)
        );
    }

    #[test]
    fn prefixing_and_merging_results() {
        let mut r = result_with("a", "h");
        r.prefix_folders("Project");
        assert_eq!(r.connections[0].folder.as_deref(), Some("Project/F"));
        assert_eq!(
            r.folders,
            vec!["Project".to_string(), "Project/F".to_string()]
        );
        let mut all = ImportResult::default();
        all.merge(r);
        all.merge(result_with("b", "h2"));
        assert_eq!(all.connections.len(), 2);
        assert_eq!(all.secrets.len(), 2);
        assert!(!all.is_empty());
        assert!(ImportResult::default().is_empty());
    }
}
