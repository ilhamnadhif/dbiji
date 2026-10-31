//! Connection store (`connections.json`): connections, navigator folders,
//! user defined connection types and network profiles.
//!
//! Entries that cannot be parsed (for example written by a newer DBiji with a
//! database kind this version does not know) are kept verbatim and written
//! back on save, so they are never lost.

use crate::paths::{AppPaths, backup_corrupt_file, write_json};
use dbiji_core::config::{
    AuthMethod, ConnectionConfig, ConnectionType, NetworkProfile, ProxyConfig, SecretSlot, SshAuth,
    SshConfig, SslMode, secret_key,
};
use dbiji_core::{DbError, DbResult};
use serde::Serialize;

/// Schema version written to `connections.json`.
pub const CONNECTIONS_VERSION: u32 = 1;

/// Key of a secret that belongs to a network profile (SSH / proxy credentials
/// shared by every connection using the profile): `profile:<id>:<slot>`.
pub fn profile_secret_key(profile_id: &str, slot: SecretSlot) -> String {
    // Same suffixes as `dbiji_core::config::secret_key`.
    let suffix = match slot {
        SecretSlot::Password => "password",
        SecretSlot::SshPassword => "ssh-password",
        SecretSlot::SshPassphrase => "ssh-passphrase",
        SecretSlot::ProxyPassword => "proxy-password",
    };
    format!("profile:{profile_id}:{suffix}")
}

/// SSH / proxy settings in effect for a connection after applying its network profile.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EffectiveNetwork {
    /// Enabled SSH settings.
    pub ssh: Option<SshConfig>,
    /// Enabled proxy settings.
    pub proxy: Option<ProxyConfig>,
    /// Id of the network profile the settings come from (`None` = the connection's own).
    pub profile_id: Option<String>,
}

impl EffectiveNetwork {
    /// True when a tunnel or proxy is needed.
    pub fn needs_tunnel(&self) -> bool {
        self.ssh.is_some() || self.proxy.is_some()
    }
}

/// Folder node for the navigator tree.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FolderNode {
    /// Last path segment (empty for the root).
    pub name: String,
    /// Full `/` separated path (empty for the root).
    pub path: String,
    /// Subfolders, sorted by name.
    pub children: Vec<FolderNode>,
    /// Ids of the connections directly in this folder, sorted by name.
    pub connections: Vec<String>,
}

/// All connection related configuration.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ConnectionStore {
    /// All connections, in navigator order.
    pub connections: Vec<ConnectionConfig>,
    /// Navigator folders as `/` separated paths (ancestors included).
    pub folders: Vec<String>,
    /// User defined connection types and customised built-in ones.
    pub connection_types: Vec<ConnectionType>,
    /// Reusable SSH / proxy settings.
    pub network_profiles: Vec<NetworkProfile>,
    unparsed: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct StoreFile<'a> {
    version: u32,
    folders: &'a [String],
    connection_types: &'a [ConnectionType],
    network_profiles: &'a [NetworkProfile],
    connections: Vec<serde_json::Value>,
}

/// Normalises a folder path: trims segments, drops empty ones; `None` for the root.
pub fn normalize_folder(path: &str) -> Option<String> {
    let parts: Vec<&str> = path
        .split(['/', '\\'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

fn parent_folder(path: &str) -> Option<String> {
    path.rsplit_once('/').map(|(p, _)| p.to_string())
}

fn is_in_folder(folder: &str, root: &str) -> bool {
    folder == root || folder.starts_with(&format!("{root}/"))
}

impl ConnectionStore {
    /// An empty store.
    pub fn new() -> ConnectionStore {
        ConnectionStore::default()
    }

    /// Loads `connections.json`. A missing file yields an empty store; a file
    /// that is not valid JSON is an [`dbiji_core::ErrorKind::Config`] error
    /// and is left untouched (see [`ConnectionStore::load_or_recover`]).
    pub fn load(paths: &AppPaths) -> DbResult<ConnectionStore> {
        Ok(Self::load_with_warnings(paths)?.0)
    }

    /// Like [`ConnectionStore::load`], also returning per-entry warnings.
    pub fn load_with_warnings(paths: &AppPaths) -> DbResult<(ConnectionStore, Vec<String>)> {
        let path = paths.connections_file();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((ConnectionStore::new(), Vec::new()));
            }
            Err(e) => return Err(DbError::io(format!("Cannot read {}: {e}", path.display()))),
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| DbError::config(format!("{} is corrupt: {e}", path.display())))?;
        Ok(Self::from_json_value(value))
    }

    /// Loads the store; a corrupt file is moved aside and an empty store returned with a warning.
    pub fn load_or_recover(paths: &AppPaths) -> (ConnectionStore, Vec<String>) {
        match Self::load_with_warnings(paths) {
            Ok(r) => r,
            Err(e) => {
                let mut warning = e.message.clone();
                if paths.connections_file().exists() {
                    match backup_corrupt_file(&paths.connections_file()) {
                        Ok(b) => {
                            warning.push_str(&format!("; the file was saved as {}", b.display()))
                        }
                        Err(be) => warning.push_str(&format!("; backup failed: {be}")),
                    }
                }
                tracing::warn!("{warning}");
                (ConnectionStore::new(), vec![warning])
            }
        }
    }

    /// Builds a store from the parsed JSON document, entry by entry.
    pub fn from_json_value(value: serde_json::Value) -> (ConnectionStore, Vec<String>) {
        let mut store = ConnectionStore::new();
        let mut warnings = Vec::new();
        let serde_json::Value::Object(map) = value else {
            warnings.push("connections.json does not contain an object".to_string());
            return (store, warnings);
        };
        if let Some(arr) = map.get("folders").and_then(|v| v.as_array()) {
            for f in arr.iter().filter_map(|v| v.as_str()) {
                store.add_folder(f);
            }
        }
        if let Some(arr) = map.get("connection_types").and_then(|v| v.as_array()) {
            for v in arr {
                match serde_json::from_value::<ConnectionType>(v.clone()) {
                    Ok(t) if !t.id.is_empty() => store.set_connection_type(t),
                    Ok(_) => warnings.push("A connection type without id was ignored".into()),
                    Err(e) => warnings.push(format!("A connection type could not be read: {e}")),
                }
            }
        }
        if let Some(arr) = map.get("network_profiles").and_then(|v| v.as_array()) {
            for v in arr {
                match serde_json::from_value::<NetworkProfile>(v.clone()) {
                    Ok(p) if !p.id.is_empty() => store.set_network_profile(p),
                    Ok(_) => warnings.push("A network profile without id was ignored".into()),
                    Err(e) => warnings.push(format!("A network profile could not be read: {e}")),
                }
            }
        }
        if let Some(arr) = map.get("connections").and_then(|v| v.as_array()) {
            for v in arr {
                match serde_json::from_value::<ConnectionConfig>(v.clone()) {
                    Ok(c) if store.get(&c.id).is_none() => {
                        if let Some(f) = c.folder.as_deref().and_then(normalize_folder) {
                            store.add_folder(&f);
                        }
                        store.connections.push(c);
                    }
                    Ok(c) => warnings.push(format!("Duplicate connection id '{}' ignored", c.id)),
                    Err(e) => {
                        let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                        warnings.push(format!(
                            "Connection '{name}' could not be read ({e}); it is kept unchanged in the file"
                        ));
                        store.unparsed.push(v.clone());
                    }
                }
            }
        }
        (store, warnings)
    }

    /// Writes `connections.json` atomically with mode 0600.
    pub fn save(&self, paths: &AppPaths) -> DbResult<()> {
        let mut connections = Vec::with_capacity(self.connections.len() + self.unparsed.len());
        for c in &self.connections {
            connections.push(serde_json::to_value(c)?);
        }
        connections.extend(self.unparsed.iter().cloned());
        let file = StoreFile {
            version: CONNECTIONS_VERSION,
            folders: &self.folders,
            connection_types: &self.connection_types,
            network_profiles: &self.network_profiles,
            connections,
        };
        write_json(&paths.connections_file(), &file)
    }

    /// Number of entries kept verbatim because they could not be parsed.
    pub fn unparsed_count(&self) -> usize {
        self.unparsed.len()
    }

    // ---------------------------------------------------------------- CRUD

    /// Connection by id.
    pub fn get(&self, id: &str) -> Option<&ConnectionConfig> {
        self.connections.iter().find(|c| c.id == id)
    }

    /// Mutable connection by id.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut ConnectionConfig> {
        self.connections.iter_mut().find(|c| c.id == id)
    }

    /// Adds a connection. An empty id is replaced with a new UUID; an id that
    /// already exists is an error. Returns the stored connection's id.
    pub fn add(&mut self, mut cfg: ConnectionConfig) -> DbResult<String> {
        if cfg.id.trim().is_empty() {
            cfg.id = uuid::Uuid::new_v4().to_string();
        }
        if self.get(&cfg.id).is_some() {
            return Err(DbError::config(format!(
                "A connection with id '{}' already exists",
                cfg.id
            )));
        }
        cfg.folder = cfg.folder.as_deref().and_then(normalize_folder);
        if let Some(f) = cfg.folder.clone() {
            self.add_folder(&f);
        }
        let id = cfg.id.clone();
        self.connections.push(cfg);
        Ok(id)
    }

    /// Replaces the connection with the same id.
    pub fn update(&mut self, mut cfg: ConnectionConfig) -> DbResult<()> {
        cfg.folder = cfg.folder.as_deref().and_then(normalize_folder);
        if let Some(f) = cfg.folder.clone() {
            self.add_folder(&f);
        }
        match self.get_mut(&cfg.id) {
            Some(slot) => {
                *slot = cfg;
                Ok(())
            }
            None => Err(DbError::config(format!(
                "Connection '{}' does not exist",
                cfg.id
            ))),
        }
    }

    /// Removes a connection and returns it. The caller should delete its
    /// secrets ([`ConnectionStore::secret_keys_for`]).
    pub fn remove(&mut self, id: &str) -> Option<ConnectionConfig> {
        let pos = self.connections.iter().position(|c| c.id == id)?;
        Some(self.connections.remove(pos))
    }

    /// Duplicates a connection (new id, unique "(copy)" name) right after the
    /// original and returns the copy. Copy the secrets with
    /// [`ConnectionStore::secret_key_pairs`].
    pub fn duplicate(&mut self, id: &str) -> DbResult<ConnectionConfig> {
        let pos = self
            .connections
            .iter()
            .position(|c| c.id == id)
            .ok_or_else(|| DbError::config(format!("Connection '{id}' does not exist")))?;
        let original = &self.connections[pos];
        let mut copy = original.duplicate();
        copy.name = self.unique_name(
            &format!("{} (copy)", original.name),
            original.folder.as_deref(),
        );
        self.connections.insert(pos + 1, copy.clone());
        Ok(copy)
    }

    /// Moves a connection to `folder` (`None` = top level), creating the folder.
    pub fn move_to_folder(&mut self, id: &str, folder: Option<&str>) -> DbResult<()> {
        let folder = folder.and_then(normalize_folder);
        if let Some(f) = &folder {
            self.add_folder(f);
        }
        let c = self
            .get_mut(id)
            .ok_or_else(|| DbError::config(format!("Connection '{id}' does not exist")))?;
        c.folder = folder;
        Ok(())
    }

    /// Records a successful connect (unix seconds).
    pub fn touch_last_connected(&mut self, id: &str, unix_secs: i64) {
        if let Some(c) = self.get_mut(id) {
            c.last_connected = Some(unix_secs);
        }
    }

    /// `base` if unused in `folder`, else `base 2`, `base 3` ...
    pub fn unique_name(&self, base: &str, folder: Option<&str>) -> String {
        let folder = folder.and_then(normalize_folder);
        let taken = |n: &str| {
            self.connections.iter().any(|c| {
                c.name.eq_ignore_ascii_case(n)
                    && c.folder.as_deref().and_then(normalize_folder) == folder
            })
        };
        if !taken(base) {
            return base.to_string();
        }
        (2..)
            .map(|i| format!("{base} {i}"))
            .find(|n| !taken(n))
            .unwrap_or_else(|| format!("{base} {}", uuid::Uuid::new_v4()))
    }

    /// True when another connection (not `except_id`) in `folder` has this name.
    pub fn name_in_use(&self, name: &str, folder: Option<&str>, except_id: Option<&str>) -> bool {
        let folder = folder.and_then(normalize_folder);
        self.connections.iter().any(|c| {
            Some(c.id.as_str()) != except_id
                && c.name.eq_ignore_ascii_case(name.trim())
                && c.folder.as_deref().and_then(normalize_folder) == folder
        })
    }

    /// Case-insensitive search over name, host, database, user, folder and description.
    pub fn search(&self, text: &str) -> Vec<&ConnectionConfig> {
        let needle = text.trim().to_lowercase();
        if needle.is_empty() {
            return self.connections.iter().collect();
        }
        self.connections
            .iter()
            .filter(|c| {
                [
                    Some(c.name.as_str()),
                    Some(c.host.as_str()),
                    c.database.as_deref(),
                    c.user.as_deref(),
                    c.folder.as_deref(),
                    c.description.as_deref(),
                    c.file_path.as_deref(),
                ]
                .into_iter()
                .flatten()
                .any(|s| s.to_lowercase().contains(&needle))
            })
            .collect()
    }

    // ------------------------------------------------------------- folders

    /// Adds a folder (and its ancestors). Returns the normalised path.
    pub fn add_folder(&mut self, path: &str) -> Option<String> {
        let path = normalize_folder(path)?;
        let mut acc = String::new();
        for seg in path.split('/') {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(seg);
            if !self.folders.contains(&acc) {
                self.folders.push(acc.clone());
            }
        }
        self.folders.sort_by_key(|f| f.to_lowercase());
        Some(path)
    }

    /// Renames / moves a folder; subfolders and connections follow.
    pub fn rename_folder(&mut self, old: &str, new: &str) -> DbResult<()> {
        let old =
            normalize_folder(old).ok_or_else(|| DbError::config("The folder name is empty"))?;
        let new =
            normalize_folder(new).ok_or_else(|| DbError::config("The new folder name is empty"))?;
        if old == new {
            return Ok(());
        }
        if !self.folders.contains(&old) {
            return Err(DbError::config(format!("Folder '{old}' does not exist")));
        }
        if self.folders.contains(&new) {
            return Err(DbError::config(format!("A folder '{new}' already exists")));
        }
        if is_in_folder(&new, &old) {
            return Err(DbError::config("A folder cannot be moved into itself"));
        }
        let remap = |f: &str| -> Option<String> {
            if f == old {
                Some(new.clone())
            } else {
                f.strip_prefix(&format!("{old}/"))
                    .map(|rest| format!("{new}/{rest}"))
            }
        };
        let mut folders: Vec<String> = Vec::new();
        for f in std::mem::take(&mut self.folders) {
            folders.push(remap(&f).unwrap_or(f));
        }
        self.folders.clear();
        for f in folders {
            self.add_folder(&f);
        }
        for c in &mut self.connections {
            if let Some(f) = c.folder.as_deref().and_then(remap) {
                c.folder = Some(f);
            }
        }
        Ok(())
    }

    /// Deletes a folder. Its connections and subfolders move up to the
    /// parent folder. Returns the number of connections moved.
    pub fn delete_folder(&mut self, path: &str) -> DbResult<usize> {
        let path =
            normalize_folder(path).ok_or_else(|| DbError::config("The folder name is empty"))?;
        if !self.folders.contains(&path) {
            return Err(DbError::config(format!("Folder '{path}' does not exist")));
        }
        let parent = parent_folder(&path);
        let lift = |f: &str| -> Option<Option<String>> {
            if f == path {
                Some(parent.clone())
            } else {
                f.strip_prefix(&format!("{path}/"))
                    .map(|rest| match &parent {
                        Some(p) => Some(format!("{p}/{rest}")),
                        None => Some(rest.to_string()),
                    })
            }
        };
        let old = std::mem::take(&mut self.folders);
        for f in old {
            match lift(&f) {
                Some(Some(n)) => {
                    self.add_folder(&n);
                }
                Some(None) => {}
                None => {
                    self.add_folder(&f);
                }
            }
        }
        let mut moved = 0;
        for c in &mut self.connections {
            if let Some(target) = c.folder.as_deref().and_then(lift) {
                c.folder = target;
                moved += 1;
            }
        }
        Ok(moved)
    }

    /// Folder tree with connection ids, for the navigator.
    pub fn folder_tree(&self) -> FolderNode {
        fn insert(node: &mut FolderNode, segments: &[&str], prefix: &str) -> usize {
            let Some((first, rest)) = segments.split_first() else {
                return 0;
            };
            let path = if prefix.is_empty() {
                first.to_string()
            } else {
                format!("{prefix}/{first}")
            };
            let idx = match node.children.iter().position(|c| c.name == *first) {
                Some(i) => i,
                None => {
                    node.children.push(FolderNode {
                        name: first.to_string(),
                        path: path.clone(),
                        ..Default::default()
                    });
                    node.children.len() - 1
                }
            };
            insert(&mut node.children[idx], rest, &path)
        }
        fn find<'a>(node: &'a mut FolderNode, segments: &[&str]) -> &'a mut FolderNode {
            match segments.split_first() {
                None => node,
                Some((first, rest)) => match node.children.iter().position(|c| c.name == *first) {
                    Some(i) => find(&mut node.children[i], rest),
                    None => node,
                },
            }
        }
        fn sort(node: &mut FolderNode) {
            node.children.sort_by_key(|c| c.name.to_lowercase());
            for c in &mut node.children {
                sort(c);
            }
        }
        let mut root = FolderNode::default();
        for f in &self.folders {
            let segs: Vec<&str> = f.split('/').collect();
            insert(&mut root, &segs, "");
        }
        let mut conns: Vec<&ConnectionConfig> = self.connections.iter().collect();
        conns.sort_by_key(|c| c.name.to_lowercase());
        for c in conns {
            let folder = c.folder.as_deref().and_then(normalize_folder);
            let segs: Vec<&str> = folder
                .as_deref()
                .map(|f| f.split('/').collect())
                .unwrap_or_default();
            if !segs.is_empty() {
                insert(&mut root, &segs, "");
            }
            find(&mut root, &segs).connections.push(c.id.clone());
        }
        sort(&mut root);
        root
    }

    // ---------------------------------------------------- connection types

    /// Connection type by id: user definitions first, then the built-ins;
    /// unknown ids fall back to "Development" settings.
    pub fn connection_type(&self, id: &str) -> ConnectionType {
        if let Some(t) = self.connection_types.iter().find(|t| t.id == id) {
            return t.clone();
        }
        ConnectionType::builtins()
            .into_iter()
            .find(|t| t.id == id)
            .unwrap_or_else(ConnectionType::development)
    }

    /// True when `id` is a built-in or user defined type.
    pub fn has_connection_type(&self, id: &str) -> bool {
        self.connection_types.iter().any(|t| t.id == id)
            || ConnectionType::builtins().iter().any(|t| t.id == id)
    }

    /// Built-in types (with user customisations applied) followed by user defined ones.
    pub fn all_connection_types(&self) -> Vec<ConnectionType> {
        let mut out: Vec<ConnectionType> = ConnectionType::builtins()
            .into_iter()
            .map(|b| {
                self.connection_types
                    .iter()
                    .find(|t| t.id == b.id)
                    .cloned()
                    .unwrap_or(b)
            })
            .collect();
        for t in &self.connection_types {
            if !out.iter().any(|o| o.id == t.id) {
                out.push(t.clone());
            }
        }
        out
    }

    /// Adds or replaces a connection type (customising a built-in is allowed).
    pub fn set_connection_type(&mut self, t: ConnectionType) {
        match self.connection_types.iter_mut().find(|x| x.id == t.id) {
            Some(slot) => *slot = t,
            None => self.connection_types.push(t),
        }
    }

    /// Removes a user type, or resets a customised built-in. Connections that
    /// used a removed custom type are switched to "dev"; returns how many.
    pub fn remove_connection_type(&mut self, id: &str) -> usize {
        self.connection_types.retain(|t| t.id != id);
        if ConnectionType::builtins().iter().any(|t| t.id == id) {
            return 0;
        }
        let mut n = 0;
        for c in &mut self.connections {
            if c.connection_type == id {
                c.connection_type = "dev".into();
                n += 1;
            }
        }
        n
    }

    /// Auto-commit default of a connection: its own override, else its type's.
    pub fn effective_auto_commit(&self, cfg: &ConnectionConfig) -> bool {
        cfg.auto_commit
            .unwrap_or_else(|| self.connection_type(&cfg.connection_type).auto_commit)
    }

    /// Marker colour: the connection's own colour, else its type's.
    pub fn color(&self, cfg: &ConnectionConfig) -> [u8; 3] {
        cfg.color
            .unwrap_or_else(|| self.connection_type(&cfg.connection_type).color)
    }

    // ---------------------------------------------------- network profiles

    /// Network profile by id.
    pub fn network_profile(&self, id: &str) -> Option<&NetworkProfile> {
        self.network_profiles.iter().find(|p| p.id == id)
    }

    /// Adds or replaces a network profile (empty id gets a new UUID).
    pub fn set_network_profile(&mut self, mut p: NetworkProfile) {
        if p.id.trim().is_empty() {
            p.id = uuid::Uuid::new_v4().to_string();
        }
        match self.network_profiles.iter_mut().find(|x| x.id == p.id) {
            Some(slot) => *slot = p,
            None => self.network_profiles.push(p),
        }
    }

    /// Removes a profile; connections using it fall back to their own
    /// settings. Returns the number of connections that referenced it.
    pub fn remove_network_profile(&mut self, id: &str) -> usize {
        self.network_profiles.retain(|p| p.id != id);
        let mut n = 0;
        for c in &mut self.connections {
            if c.network_profile.as_deref() == Some(id) {
                c.network_profile = None;
                n += 1;
            }
        }
        n
    }

    /// SSH / proxy settings in effect: the network profile's when the
    /// connection references an existing one, else the connection's own.
    /// Disabled settings are dropped.
    pub fn effective_network(&self, cfg: &ConnectionConfig) -> EffectiveNetwork {
        if let Some(p) = cfg
            .network_profile
            .as_deref()
            .and_then(|id| self.network_profile(id))
        {
            return EffectiveNetwork {
                ssh: p.ssh.clone().filter(|s| s.enabled),
                proxy: p.proxy.clone().filter(|s| s.enabled),
                profile_id: Some(p.id.clone()),
            };
        }
        EffectiveNetwork {
            ssh: cfg.ssh.clone().filter(|s| s.enabled),
            proxy: cfg.proxy.clone().filter(|s| s.enabled),
            profile_id: None,
        }
    }

    /// Effective SSH settings (see [`ConnectionStore::effective_network`]).
    pub fn effective_ssh(&self, cfg: &ConnectionConfig) -> Option<SshConfig> {
        self.effective_network(cfg).ssh
    }

    /// Effective proxy settings (see [`ConnectionStore::effective_network`]).
    pub fn effective_proxy(&self, cfg: &ConnectionConfig) -> Option<ProxyConfig> {
        self.effective_network(cfg).proxy
    }

    /// Secret key for an SSH / proxy slot: the profile's key when the
    /// settings come from a network profile, else the connection's.
    pub fn network_secret_key(&self, cfg: &ConnectionConfig, slot: SecretSlot) -> String {
        match (slot, self.effective_network(cfg).profile_id) {
            (SecretSlot::Password, _) | (_, None) => secret_key(&cfg.id, slot),
            (_, Some(pid)) => profile_secret_key(&pid, slot),
        }
    }

    // ------------------------------------------------------------- secrets

    /// Every secret key a connection can own.
    pub fn secret_keys_for(connection_id: &str) -> Vec<String> {
        [
            SecretSlot::Password,
            SecretSlot::SshPassword,
            SecretSlot::SshPassphrase,
            SecretSlot::ProxyPassword,
        ]
        .into_iter()
        .map(|s| secret_key(connection_id, s))
        .collect()
    }

    /// `(old key, new key)` pairs to copy secrets from one connection to another.
    pub fn secret_key_pairs(from_id: &str, to_id: &str) -> Vec<(String, String)> {
        Self::secret_keys_for(from_id)
            .into_iter()
            .zip(Self::secret_keys_for(to_id))
            .collect()
    }

    /// All secret keys referenced by the store (connections and profiles),
    /// e.g. for migrating between secret backends.
    pub fn all_secret_keys(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .connections
            .iter()
            .flat_map(|c| Self::secret_keys_for(&c.id))
            .collect();
        for p in &self.network_profiles {
            for slot in [
                SecretSlot::SshPassword,
                SecretSlot::SshPassphrase,
                SecretSlot::ProxyPassword,
            ] {
                out.push(profile_secret_key(&p.id, slot));
            }
        }
        out
    }

    // ---------------------------------------------------------- validation

    /// Problems of `cfg` including store level checks (connection type,
    /// network profile, effective SSH / proxy settings). Empty = valid.
    pub fn validate(&self, cfg: &ConnectionConfig) -> Vec<String> {
        let net = self.effective_network(cfg);
        let mut problems = validate_basics(cfg);
        validate_network(cfg, net.ssh.as_ref(), net.proxy.as_ref(), &mut problems);
        if !self.has_connection_type(&cfg.connection_type) {
            problems.push(format!("Unknown connection type '{}'", cfg.connection_type));
        }
        if let Some(pid) = &cfg.network_profile
            && self.network_profile(pid).is_none()
        {
            problems.push(format!("The network profile '{pid}' does not exist"));
        }
        problems
    }
}

/// Problems of a connection configuration on its own (its own SSH / proxy
/// settings, no store lookups). Empty = valid.
pub fn validate(cfg: &ConnectionConfig) -> Vec<String> {
    let mut problems = validate_basics(cfg);
    validate_network(
        cfg,
        cfg.ssh.as_ref().filter(|s| s.enabled),
        cfg.proxy.as_ref().filter(|p| p.enabled),
        &mut problems,
    );
    problems
}

fn validate_basics(cfg: &ConnectionConfig) -> Vec<String> {
    let mut p = Vec::new();
    if cfg.id.trim().is_empty() {
        p.push("The connection id is empty".to_string());
    }
    if cfg.name.trim().is_empty() {
        p.push("The connection name is empty".to_string());
    }
    if cfg.kind == dbiji_core::DbKind::Generic {
        p.push("Choose a database type".to_string());
    }
    if cfg.kind.is_file_based() {
        if cfg
            .file_path
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            p.push("A database file is required".to_string());
        }
    } else {
        if cfg.host.trim().is_empty() {
            p.push("The host is empty".to_string());
        }
        if cfg.port == Some(0) {
            p.push("The port must be between 1 and 65535".to_string());
        }
    }
    match &cfg.auth {
        AuthMethod::Command { command } if command.trim().is_empty() => {
            p.push("The password command is empty".to_string());
        }
        AuthMethod::Pgpass if !cfg.kind.is_postgres_family() => {
            p.push(
                "pgpass authentication is only available for PostgreSQL-compatible databases"
                    .to_string(),
            );
        }
        _ => {}
    }
    let ssl = &cfg.ssl;
    if !cfg.kind.is_file_based() {
        if ssl.only_custom_ca && ssl.ca_cert.as_deref().unwrap_or("").trim().is_empty() {
            p.push(
                "A CA certificate file is required when only the custom CA is trusted".to_string(),
            );
        }
        if ssl.client_cert.is_some() != ssl.client_key.is_some() {
            p.push("The client certificate and the client key must be set together".to_string());
        }
        if ssl.mode == SslMode::Disable && (ssl.client_cert.is_some() || ssl.only_custom_ca) {
            p.push("Certificates are configured but SSL is disabled".to_string());
        }
    }
    if cfg.init.fetch_size == Some(0) {
        p.push("The fetch size must be at least 1".to_string());
    }
    p
}

fn validate_network(
    cfg: &ConnectionConfig,
    ssh: Option<&SshConfig>,
    proxy: Option<&ProxyConfig>,
    p: &mut Vec<String>,
) {
    if cfg.kind.is_file_based() {
        if ssh.is_some() {
            p.push("SSH tunnels cannot be used with file-based databases".to_string());
        }
        if proxy.is_some() {
            p.push("Proxies cannot be used with file-based databases".to_string());
        }
        return;
    }
    if let Some(s) = ssh {
        if s.host.trim().is_empty() {
            p.push("The SSH host is empty".to_string());
        }
        if s.port == 0 {
            p.push("The SSH port must be between 1 and 65535".to_string());
        }
        if s.user.trim().is_empty() {
            p.push("The SSH user is empty".to_string());
        }
        if let SshAuth::PublicKey { key_path } = &s.auth
            && key_path.trim().is_empty()
        {
            p.push("The SSH private key file is not set".to_string());
        }
        if s.local_port == Some(0) {
            p.push(
                "The SSH local port must be between 1 and 65535 (leave it empty for automatic)"
                    .to_string(),
            );
        }
    }
    if let Some(x) = proxy {
        if x.host.trim().is_empty() {
            p.push("The proxy host is empty".to_string());
        }
        if x.port == 0 {
            p.push("The proxy port must be between 1 and 65535".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::DbKind;
    use dbiji_core::config::ProxyKind;

    fn conn(name: &str, folder: Option<&str>) -> ConnectionConfig {
        let mut c = ConnectionConfig::new(DbKind::Postgres, name);
        c.folder = folder.map(str::to_string);
        c
    }

    #[test]
    fn crud_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::with_root(dir.path());
        let mut store = ConnectionStore::load(&paths).unwrap();
        assert!(store.connections.is_empty());
        let id = store.add(conn("local", Some(" Prod / EU "))).unwrap();
        assert_eq!(store.get(&id).unwrap().folder.as_deref(), Some("Prod/EU"));
        assert_eq!(
            store.folders,
            vec!["Prod".to_string(), "Prod/EU".to_string()]
        );
        assert!(
            store.add(store.get(&id).unwrap().clone()).is_err(),
            "duplicate id rejected"
        );
        let mut c = store.get(&id).unwrap().clone();
        c.host = "db.example".into();
        store.update(c).unwrap();
        let copy = store.duplicate(&id).unwrap();
        assert_ne!(copy.id, id);
        assert_eq!(copy.name, "local (copy)");
        let copy2 = store.duplicate(&id).unwrap();
        assert_eq!(copy2.name, "local (copy) 2");
        assert_eq!(
            store.connections[1].id, copy2.id,
            "inserted after the original"
        );
        store.save(&paths).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(paths.connections_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let back = ConnectionStore::load(&paths).unwrap();
        assert_eq!(back, store);
        assert!(store.remove(&copy.id).is_some());
        assert!(store.remove(&copy.id).is_none());
        let text = std::fs::read_to_string(paths.connections_file()).unwrap();
        assert!(
            !text.to_lowercase().contains("\"password\":\""),
            "no passwords in connections.json"
        );
    }

    #[test]
    fn unknown_entries_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::with_root(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(
            paths.connections_file(),
            r#"{"version": 9, "connections": [
                {"id": "a", "name": "ok", "kind": "mysql"},
                {"id": "b", "name": "future", "kind": "duckdb", "file_path": "/data/w.duckdb"}
            ], "folders": ["X"], "future_field": 1}"#,
        )
        .unwrap();
        let (store, warnings) = ConnectionStore::load_with_warnings(&paths).unwrap();
        assert_eq!(store.connections.len(), 1);
        assert_eq!(store.unparsed_count(), 1);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("future"));
        store.save(&paths).unwrap();
        let text = std::fs::read_to_string(paths.connections_file()).unwrap();
        assert!(
            text.contains("duckdb"),
            "unparsed entry must be written back"
        );
    }

    #[test]
    fn corrupt_file_handling() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::with_root(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.connections_file(), "[[[").unwrap();
        let err = ConnectionStore::load(&paths).unwrap_err();
        assert_eq!(err.kind, dbiji_core::ErrorKind::Config);
        assert!(
            paths.connections_file().exists(),
            "load leaves the file alone"
        );
        let (store, w) = ConnectionStore::load_or_recover(&paths);
        assert!(store.connections.is_empty());
        assert_eq!(w.len(), 1);
        assert!(!paths.connections_file().exists());
    }

    #[test]
    fn folders_rename_delete_and_tree() {
        let mut s = ConnectionStore::new();
        let a = s.add(conn("a", Some("Prod/EU"))).unwrap();
        let b = s.add(conn("b", Some("Prod"))).unwrap();
        let c = s.add(conn("c", None)).unwrap();
        s.add_folder("Dev");
        s.rename_folder("Prod", "Production").unwrap();
        assert_eq!(s.get(&a).unwrap().folder.as_deref(), Some("Production/EU"));
        assert_eq!(s.get(&b).unwrap().folder.as_deref(), Some("Production"));
        assert!(s.folders.contains(&"Production/EU".to_string()));
        assert!(
            !s.folders
                .iter()
                .any(|f| f.starts_with("Prod/") || f == "Prod")
        );
        assert!(s.rename_folder("Production", "Dev").is_err());
        assert!(s.rename_folder("Production", "Production/x").is_err());

        let tree = s.folder_tree();
        assert_eq!(tree.connections, vec![c.clone()]);
        let names: Vec<&str> = tree.children.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["Dev", "Production"]);
        assert_eq!(tree.children[1].connections, vec![b.clone()]);
        assert_eq!(tree.children[1].children[0].path, "Production/EU");
        assert_eq!(tree.children[1].children[0].connections, vec![a.clone()]);

        let moved = s.delete_folder("Production").unwrap();
        assert_eq!(moved, 2);
        assert_eq!(s.get(&b).unwrap().folder, None);
        assert_eq!(s.get(&a).unwrap().folder.as_deref(), Some("EU"));
        assert!(s.folders.contains(&"EU".to_string()));
        s.move_to_folder(&c, Some("New/Sub")).unwrap();
        assert!(s.folders.contains(&"New".to_string()));
        assert!(s.delete_folder("Missing").is_err());
    }

    #[test]
    fn connection_types_and_colors() {
        let mut s = ConnectionStore::new();
        assert_eq!(s.connection_type("prod").name, "Production");
        assert_eq!(s.connection_type("whatever").id, "dev");
        let mut custom = ConnectionType::development();
        custom.id = "staging".into();
        custom.name = "Staging".into();
        custom.color = [1, 2, 3];
        custom.auto_commit = false;
        s.set_connection_type(custom);
        let mut prod = ConnectionType::production();
        prod.color = [9, 9, 9];
        s.set_connection_type(prod);
        let all = s.all_connection_types();
        assert_eq!(
            all.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["dev", "test", "prod", "staging"]
        );
        assert_eq!(all[2].color, [9, 9, 9]);
        let mut c = conn("x", None);
        c.connection_type = "staging".into();
        let id = s.add(c).unwrap();
        assert!(!s.effective_auto_commit(s.get(&id).unwrap()));
        assert_eq!(s.color(s.get(&id).unwrap()), [1, 2, 3]);
        assert_eq!(s.remove_connection_type("staging"), 1);
        assert_eq!(s.get(&id).unwrap().connection_type, "dev");
        assert_eq!(s.remove_connection_type("prod"), 0);
        assert_eq!(s.connection_type("prod"), ConnectionType::production());
    }

    #[test]
    fn network_profiles_resolution() {
        let mut s = ConnectionStore::new();
        let ssh = SshConfig {
            enabled: true,
            host: "bastion".into(),
            user: "me".into(),
            ..Default::default()
        };
        s.set_network_profile(NetworkProfile {
            id: "corp".into(),
            name: "Corp".into(),
            ssh: Some(ssh.clone()),
            proxy: None,
        });
        let mut c = conn("x", None);
        c.ssh = Some(SshConfig {
            enabled: true,
            host: "own".into(),
            user: "u".into(),
            ..Default::default()
        });
        c.proxy = Some(ProxyConfig {
            enabled: false,
            kind: ProxyKind::Http,
            host: "p".into(),
            ..Default::default()
        });
        let id = s.add(c).unwrap();
        let own = s.effective_network(s.get(&id).unwrap());
        assert_eq!(own.ssh.as_ref().unwrap().host, "own");
        assert!(own.proxy.is_none(), "disabled proxy dropped");
        assert_eq!(own.profile_id, None);
        assert_eq!(
            s.network_secret_key(s.get(&id).unwrap(), SecretSlot::SshPassword),
            format!("conn:{id}:ssh-password")
        );

        s.get_mut(&id).unwrap().network_profile = Some("corp".into());
        let net = s.effective_network(s.get(&id).unwrap());
        assert_eq!(net.ssh.unwrap().host, "bastion");
        assert_eq!(net.profile_id.as_deref(), Some("corp"));
        let cfg = s.get(&id).unwrap();
        assert_eq!(
            s.network_secret_key(cfg, SecretSlot::SshPassword),
            "profile:corp:ssh-password"
        );
        assert_eq!(
            s.network_secret_key(cfg, SecretSlot::Password),
            format!("conn:{id}:password")
        );
        assert!(
            s.all_secret_keys()
                .contains(&"profile:corp:proxy-password".to_string())
        );
        assert_eq!(s.remove_network_profile("corp"), 1);
        assert_eq!(s.get(&id).unwrap().network_profile, None);
    }

    #[test]
    fn validation() {
        let s = ConnectionStore::new();
        let good = conn("ok", None);
        assert!(validate(&good).is_empty(), "{:?}", validate(&good));
        assert!(s.validate(&good).is_empty());

        let mut bad = conn(" ", None);
        bad.host = "".into();
        bad.port = Some(0);
        bad.auth = AuthMethod::Command {
            command: " ".into(),
        };
        bad.ssh = Some(SshConfig {
            enabled: true,
            auth: SshAuth::PublicKey {
                key_path: "".into(),
            },
            local_port: Some(0),
            port: 0,
            ..Default::default()
        });
        bad.proxy = Some(ProxyConfig {
            enabled: true,
            port: 0,
            ..Default::default()
        });
        bad.ssl.client_cert = Some("c.pem".into());
        bad.connection_type = "nope".into();
        bad.network_profile = Some("ghost".into());
        let problems = s.validate(&bad);
        for needle in [
            "name is empty",
            "host is empty",
            "port must be",
            "password command",
            "SSH host",
            "SSH user",
            "private key",
            "SSH local port",
            "SSH port",
            "proxy host",
            "proxy port",
            "client key",
            "Unknown connection type",
            "network profile",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(needle)),
                "missing '{needle}' in {problems:?}"
            );
        }

        let mut file = ConnectionConfig::new(DbKind::Sqlite, "f");
        assert!(validate(&file).iter().any(|p| p.contains("database file")));
        file.file_path = Some("/tmp/x.db".into());
        file.ssh = Some(SshConfig {
            enabled: true,
            host: "h".into(),
            user: "u".into(),
            ..Default::default()
        });
        assert!(validate(&file).iter().any(|p| p.contains("file-based")));
        let mut pgpass = ConnectionConfig::new(DbKind::MySql, "m");
        pgpass.auth = AuthMethod::Pgpass;
        assert!(validate(&pgpass).iter().any(|p| p.contains("pgpass")));
    }

    #[test]
    fn search_and_names() {
        let mut s = ConnectionStore::new();
        let mut c = conn("Orders DB", Some("Shop"));
        c.host = "orders.internal".into();
        s.add(c).unwrap();
        s.add(conn("Other", None)).unwrap();
        assert_eq!(s.search("ORDERS").len(), 1);
        assert_eq!(s.search("shop").len(), 1);
        assert_eq!(s.search("").len(), 2);
        assert!(s.name_in_use("orders db", Some("Shop"), None));
        assert!(!s.name_in_use("orders db", None, None));
        assert_eq!(s.unique_name("Other", None), "Other 2");
        assert_eq!(s.unique_name("Other", Some("Shop")), "Other");
        assert_eq!(
            profile_secret_key("p1", SecretSlot::ProxyPassword),
            "profile:p1:proxy-password"
        );
        assert_eq!(ConnectionStore::secret_keys_for("z").len(), 4);
        assert_eq!(
            ConnectionStore::secret_key_pairs("a", "b")[0],
            ("conn:a:password".into(), "conn:b:password".into())
        );
    }
}
