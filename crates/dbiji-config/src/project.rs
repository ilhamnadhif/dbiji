//! Projects (FR-S1 scripts, navigator bookmarks, FR-M2 saved diagrams) and
//! per-connection virtual models (FR-D3).
//!
//! Layout under `data/projects/`:
//!
//! ```text
//! General/                 default project
//!   Scripts/               .sql files (optionally per connection subfolder)
//!   Diagrams/<name>.erd.json
//!   .dbiji/scripts.json    script → connection / database / schema
//!   .dbiji/bookmarks.json
//! .dbiji/session.json      open editors (restored on start)
//! .dbiji/virtual/<connection id>.json
//! ```

use crate::paths::{ensure_private_dir, read_json, write_json, write_secure};
use dbiji_core::meta::ObjectRef;
use dbiji_core::value::DataKind;
use dbiji_core::{DbError, DbResult};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

/// Name of the project that always exists.
pub const DEFAULT_PROJECT: &str = "General";
const SCRIPTS_DIR: &str = "Scripts";
const DIAGRAMS_DIR: &str = "Diagrams";
const META_DIR: &str = ".dbiji";
const DIAGRAM_SUFFIX: &str = ".erd.json";

/// Makes `name` usable as a single file or folder name: path separators,
/// reserved and control characters become `_`, leading / trailing dots and
/// spaces are removed, the length is limited.
pub fn sanitize_file_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    out = out.trim().trim_matches('.').trim().to_string();
    if out.chars().count() > 120 {
        out = out.chars().take(120).collect();
    }
    out
}

fn valid_name(name: &str) -> DbResult<String> {
    let s = sanitize_file_name(name);
    if s.is_empty() || s != name.trim() {
        return Err(DbError::config(format!("'{name}' is not a valid name")));
    }
    Ok(s)
}

/// Rejects `..`, absolute paths and other escapes in a relative path.
fn safe_relative(rel: &Path) -> DbResult<PathBuf> {
    let mut out = PathBuf::new();
    for c in rel.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return Err(DbError::config(format!("Invalid path '{}'", rel.display()))),
        }
    }
    Ok(out)
}

fn rel_key(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(p) => p.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn io_err(path: &Path, e: std::io::Error) -> DbError {
    DbError::io(format!("{}: {e}", path.display()))
}

/// Connection context remembered for a script file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ScriptMeta {
    /// Connection the script runs on.
    pub connection_id: Option<String>,
    /// Active database.
    pub database: Option<String>,
    /// Active schema.
    pub schema: Option<String>,
}

/// A script file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptInfo {
    /// File name (`Script-1.sql`).
    pub name: String,
    /// Absolute path.
    pub path: PathBuf,
    /// Path relative to the Scripts folder, `/` separated.
    pub relative: String,
    /// Size in bytes.
    pub size: u64,
    /// Last modification time.
    pub modified: Option<SystemTime>,
}

/// Folder of the script tree.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScriptFolder {
    /// Folder name (empty for the root).
    pub name: String,
    /// Absolute path.
    pub path: PathBuf,
    /// Path relative to the Scripts folder (empty for the root).
    pub relative: String,
    /// Subfolders, sorted by name.
    pub folders: Vec<ScriptFolder>,
    /// Scripts directly in this folder, sorted by name.
    pub scripts: Vec<ScriptInfo>,
}

/// Navigator bookmark.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    /// Unique id.
    pub id: String,
    /// Connection the object belongs to.
    pub connection_id: String,
    /// Bookmarked object.
    pub object: ObjectRef,
    /// Display label.
    pub label: String,
    /// Bookmark folder (`/` separated).
    #[serde(default)]
    pub folder: Option<String>,
    /// Creation time, unix milliseconds.
    #[serde(default)]
    pub created_ms: i64,
}

impl Bookmark {
    /// New bookmark with a fresh id.
    pub fn new(
        connection_id: impl Into<String>,
        object: ObjectRef,
        label: impl Into<String>,
    ) -> Bookmark {
        Bookmark {
            id: uuid::Uuid::new_v4().to_string(),
            connection_id: connection_id.into(),
            object,
            label: label.into(),
            folder: None,
            created_ms: chrono::Utc::now().timestamp_millis(),
        }
    }
}

/// Kind of an editor tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TabKind {
    /// SQL editor backed by a script file.
    #[default]
    SqlScript,
    /// SQL console (no file; text kept in [`SessionTab::content`]).
    SqlConsole,
    /// Object editor (properties / data / DDL ...).
    ObjectEditor,
    /// Data editor of a table.
    DataEditor,
    /// ER diagram.
    ErDiagram,
    /// Anything a newer version wrote.
    #[serde(other)]
    Other,
}

/// One restored editor tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SessionTab {
    /// What the tab shows.
    pub kind: TabKind,
    /// Tab title.
    pub title: Option<String>,
    /// Script or diagram file.
    pub path: Option<PathBuf>,
    /// Project of a script tab.
    pub project: Option<String>,
    /// Active connection.
    pub connection_id: Option<String>,
    /// Active database.
    pub database: Option<String>,
    /// Active schema.
    pub schema: Option<String>,
    /// Object of object / data editors.
    pub object: Option<ObjectRef>,
    /// Cursor position (character offset).
    pub cursor: Option<usize>,
    /// Selection (character offsets).
    pub selection: Option<(usize, usize)>,
    /// First visible line.
    pub scroll_line: Option<u32>,
    /// The tab is pinned.
    pub pinned: bool,
    /// Text of consoles, which have no file.
    pub content: Option<String>,
}

/// Open editors to restore.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SessionState {
    /// Tabs in display order.
    pub tabs: Vec<SessionTab>,
    /// Index of the active tab.
    pub active: Option<usize>,
    /// Opaque UI layout (dock state).
    pub layout: Option<serde_json::Value>,
}

/// Location of a table in a virtual model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct VirtualTableKey {
    /// Database (catalog), if relevant.
    pub database: Option<String>,
    /// Schema.
    pub schema: Option<String>,
    /// Table name.
    pub table: String,
}

impl VirtualTableKey {
    /// Key of a table object.
    pub fn of(obj: &ObjectRef) -> VirtualTableKey {
        VirtualTableKey {
            database: obj.database.clone(),
            schema: obj.schema.clone(),
            table: obj.name.clone(),
        }
    }
}

/// Calculated column added to result sets of a table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct VirtualColumn {
    /// Column name shown in result sets.
    pub name: String,
    /// Expression evaluated on the client (e.g. `price * quantity`).
    pub expression: String,
    /// Logical type of the computed values.
    pub kind: DataKind,
}

/// Dictionary (lookup) settings: columns shown instead of the key when the table is referenced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DictionaryConfig {
    /// Columns shown instead of the key.
    pub description_columns: Vec<String>,
}

/// Client side additions to a table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct VirtualTable {
    /// Columns identifying rows when the table has no primary key.
    pub virtual_key: Option<Vec<String>>,
    /// Computed columns.
    pub virtual_columns: Vec<VirtualColumn>,
    /// Dictionary settings.
    pub dictionary: Option<DictionaryConfig>,
}

impl VirtualTable {
    /// Nothing configured.
    pub fn is_empty(&self) -> bool {
        self.virtual_key.as_ref().is_none_or(|k| k.is_empty())
            && self.virtual_columns.is_empty()
            && self.dictionary.is_none()
    }
}

/// One table of a [`VirtualModel`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VirtualTableEntry {
    /// Table location.
    #[serde(flatten)]
    pub key: VirtualTableKey,
    /// Table settings.
    #[serde(flatten)]
    pub table: VirtualTable,
}

/// Virtual keys, columns and dictionaries of one connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct VirtualModel {
    /// Configured tables, sorted by key.
    pub tables: Vec<VirtualTableEntry>,
}

impl VirtualModel {
    /// Settings of a table.
    pub fn get(&self, key: &VirtualTableKey) -> Option<&VirtualTable> {
        self.tables.iter().find(|e| &e.key == key).map(|e| &e.table)
    }

    /// Settings of a table object.
    pub fn get_for(&self, obj: &ObjectRef) -> Option<&VirtualTable> {
        self.get(&VirtualTableKey::of(obj))
    }

    /// Sets (or, when empty, removes) the settings of a table.
    pub fn set(&mut self, key: VirtualTableKey, table: VirtualTable) {
        if table.is_empty() {
            self.remove(&key);
            return;
        }
        match self.tables.iter_mut().find(|e| e.key == key) {
            Some(e) => e.table = table,
            None => self.tables.push(VirtualTableEntry { key, table }),
        }
        self.tables.sort_by(|a, b| a.key.cmp(&b.key));
    }

    /// Removes a table; returns whether it existed.
    pub fn remove(&mut self, key: &VirtualTableKey) -> bool {
        let before = self.tables.len();
        self.tables.retain(|e| &e.key != key);
        before != self.tables.len()
    }

    /// True when no table is configured.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

/// A project directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    name: String,
    path: PathBuf,
}

/// Manages the projects directory.
#[derive(Debug, Clone)]
pub struct ProjectManager {
    root: PathBuf,
}

impl ProjectManager {
    /// Projects under `paths.projects_dir()`; the default project is created if missing.
    pub fn new(paths: &crate::paths::AppPaths) -> DbResult<ProjectManager> {
        Self::with_root(paths.projects_dir())
    }

    /// Projects under `root`.
    pub fn with_root(root: impl Into<PathBuf>) -> DbResult<ProjectManager> {
        let root = root.into();
        ensure_private_dir(&root)?;
        let pm = ProjectManager { root };
        let default = pm.project_handle(DEFAULT_PROJECT);
        default.ensure_layout()?;
        Ok(pm)
    }

    /// The projects directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn project_handle(&self, name: &str) -> Project {
        Project {
            name: name.to_string(),
            path: self.root.join(name),
        }
    }

    /// All projects, the default one first, then alphabetically.
    pub fn list(&self) -> DbResult<Vec<Project>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(&self.root)
            .map_err(|e| io_err(&self.root, e))?
            .filter_map(Result::ok)
        {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !e.path().is_dir() {
                continue;
            }
            out.push(self.project_handle(&name));
        }
        out.sort_by_key(|p| (p.name != DEFAULT_PROJECT, p.name.to_lowercase()));
        Ok(out)
    }

    /// An existing project.
    pub fn get(&self, name: &str) -> DbResult<Project> {
        let name = valid_name(name)?;
        let p = self.project_handle(&name);
        if !p.path.is_dir() {
            return Err(DbError::config(format!("Project '{name}' does not exist")));
        }
        Ok(p)
    }

    /// The default project ("General").
    pub fn default_project(&self) -> DbResult<Project> {
        let p = self.project_handle(DEFAULT_PROJECT);
        p.ensure_layout()?;
        Ok(p)
    }

    /// Creates a project.
    pub fn create(&self, name: &str) -> DbResult<Project> {
        let name = valid_name(name)?;
        let p = self.project_handle(&name);
        if p.path.exists() {
            return Err(DbError::config(format!("Project '{name}' already exists")));
        }
        p.ensure_layout()?;
        Ok(p)
    }

    /// Renames a project (not the default one).
    pub fn rename(&self, old: &str, new: &str) -> DbResult<Project> {
        let from = self.get(old)?;
        if from.name == DEFAULT_PROJECT {
            return Err(DbError::config("The default project cannot be renamed"));
        }
        let new = valid_name(new)?;
        let to = self.project_handle(&new);
        if to.path.exists() {
            return Err(DbError::config(format!("Project '{new}' already exists")));
        }
        std::fs::rename(&from.path, &to.path).map_err(|e| io_err(&from.path, e))?;
        Ok(to)
    }

    /// Next free script path in `project` (DBeaver naming, optional
    /// per-connection subfolder); see [`Project::new_script_path`].
    pub fn new_script_path(
        &self,
        project: &str,
        connection_name: Option<&str>,
    ) -> DbResult<PathBuf> {
        self.get(project)?.new_script_path(connection_name)
    }

    /// Deletes a project and all its scripts and diagrams (not the default one).
    pub fn delete(&self, name: &str) -> DbResult<()> {
        let p = self.get(name)?;
        if p.name == DEFAULT_PROJECT {
            return Err(DbError::config("The default project cannot be deleted"));
        }
        std::fs::remove_dir_all(&p.path).map_err(|e| io_err(&p.path, e))
    }

    fn session_file(&self) -> PathBuf {
        self.root.join(META_DIR).join("session.json")
    }

    /// Open editors of the last session; missing or unreadable → empty.
    pub fn load_session(&self) -> SessionState {
        match read_json::<SessionState>(&self.session_file()) {
            Ok(s) => s.unwrap_or_default(),
            Err(e) => {
                tracing::warn!("cannot restore the previous session: {e}");
                SessionState::default()
            }
        }
    }

    /// Persists the open editors.
    pub fn save_session(&self, session: &SessionState) -> DbResult<()> {
        write_json(&self.session_file(), session)
    }

    fn virtual_file(&self, connection_id: &str) -> DbResult<PathBuf> {
        let name = sanitize_file_name(connection_id);
        if name.is_empty() {
            return Err(DbError::config("Invalid connection id"));
        }
        Ok(self
            .root
            .join(META_DIR)
            .join("virtual")
            .join(format!("{name}.json")))
    }

    /// Virtual model of a connection (empty when none was saved).
    pub fn virtual_model(&self, connection_id: &str) -> DbResult<VirtualModel> {
        Ok(read_json(&self.virtual_file(connection_id)?)?.unwrap_or_default())
    }

    /// Saves (or, when empty, deletes) the virtual model of a connection.
    pub fn save_virtual_model(&self, connection_id: &str, model: &VirtualModel) -> DbResult<()> {
        let path = self.virtual_file(connection_id)?;
        if model.is_empty() {
            return match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io_err(&path, e)),
            };
        }
        write_json(&path, model)
    }

    /// Deletes the virtual model of a removed connection.
    pub fn delete_virtual_model(&self, connection_id: &str) -> DbResult<()> {
        self.save_virtual_model(connection_id, &VirtualModel::default())
    }
}

impl Project {
    /// Project name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Project directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `Scripts/` folder.
    pub fn scripts_dir(&self) -> PathBuf {
        self.path.join(SCRIPTS_DIR)
    }

    /// `Diagrams/` folder.
    pub fn diagrams_dir(&self) -> PathBuf {
        self.path.join(DIAGRAMS_DIR)
    }

    /// `.dbiji/` metadata folder.
    pub fn meta_dir(&self) -> PathBuf {
        self.path.join(META_DIR)
    }

    fn ensure_layout(&self) -> DbResult<()> {
        for d in [
            self.path.clone(),
            self.scripts_dir(),
            self.diagrams_dir(),
            self.meta_dir(),
        ] {
            ensure_private_dir(&d)?;
        }
        Ok(())
    }

    /// Resolves a script path (absolute inside the Scripts folder, or relative
    /// to it) and returns `(absolute, relative)`.
    fn resolve(&self, path: &Path) -> DbResult<(PathBuf, PathBuf)> {
        let scripts = self.scripts_dir();
        let rel = if path.is_absolute() {
            let canon_root = scripts.canonicalize().unwrap_or_else(|_| scripts.clone());
            let candidate = match path.canonicalize() {
                Ok(c) => c,
                Err(_) => {
                    let parent = path.parent().and_then(|p| p.canonicalize().ok());
                    match (parent, path.file_name()) {
                        (Some(p), Some(f)) => p.join(f),
                        _ => path.to_path_buf(),
                    }
                }
            };
            candidate
                .strip_prefix(&canon_root)
                .map(Path::to_path_buf)
                .or_else(|_| path.strip_prefix(&scripts).map(Path::to_path_buf))
                .map_err(|_| {
                    DbError::config(format!(
                        "{} is not inside the project's Scripts folder",
                        path.display()
                    ))
                })?
        } else {
            path.to_path_buf()
        };
        let rel = safe_relative(&rel)?;
        if rel.as_os_str().is_empty() {
            return Err(DbError::config("A script path is required"));
        }
        Ok((scripts.join(&rel), rel))
    }

    /// Script files, sorted by relative path.
    pub fn list_scripts(&self) -> DbResult<Vec<ScriptInfo>> {
        let mut out = Vec::new();
        fn walk(dir: &Path, root: &Path, out: &mut Vec<ScriptInfo>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.filter_map(Result::ok) {
                let path = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                if path.is_dir() {
                    walk(&path, root, out);
                } else if name.to_ascii_lowercase().ends_with(".sql") {
                    let meta = e.metadata().ok();
                    let relative = path
                        .strip_prefix(root)
                        .map(rel_key)
                        .unwrap_or_else(|_| name.clone());
                    out.push(ScriptInfo {
                        name,
                        relative,
                        size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                        modified: meta.and_then(|m| m.modified().ok()),
                        path,
                    });
                }
            }
        }
        walk(&self.scripts_dir(), &self.scripts_dir(), &mut out);
        out.sort_by_key(|s| s.relative.to_lowercase());
        Ok(out)
    }

    /// Script folders and files as a tree.
    pub fn script_tree(&self) -> DbResult<ScriptFolder> {
        fn build(dir: &Path, root: &Path) -> ScriptFolder {
            let relative = dir.strip_prefix(root).map(rel_key).unwrap_or_default();
            let mut node = ScriptFolder {
                name: dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string(),
                path: dir.to_path_buf(),
                relative,
                ..Default::default()
            };
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.filter_map(Result::ok) {
                    let path = e.path();
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') {
                        continue;
                    }
                    if path.is_dir() {
                        node.folders.push(build(&path, root));
                    } else if name.to_ascii_lowercase().ends_with(".sql") {
                        let meta = e.metadata().ok();
                        node.scripts.push(ScriptInfo {
                            relative: path
                                .strip_prefix(root)
                                .map(rel_key)
                                .unwrap_or_else(|_| name.clone()),
                            name,
                            size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                            modified: meta.and_then(|m| m.modified().ok()),
                            path,
                        });
                    }
                }
            }
            node.folders.sort_by_key(|f| f.name.to_lowercase());
            node.scripts.sort_by_key(|s| s.name.to_lowercase());
            node
        }
        let root = self.scripts_dir();
        ensure_private_dir(&root)?;
        Ok(build(&root, &root))
    }

    /// Next free DBeaver style script name: `Script.sql`, `Script-1.sql` ...
    /// inside a subfolder named after the connection when given.
    pub fn new_script_path(&self, connection_name: Option<&str>) -> DbResult<PathBuf> {
        let mut dir = self.scripts_dir();
        if let Some(c) = connection_name
            .map(sanitize_file_name)
            .filter(|c| !c.is_empty())
        {
            dir.push(c);
        }
        ensure_private_dir(&dir)?;
        for i in 0..100_000u32 {
            let name = if i == 0 {
                "Script.sql".to_string()
            } else {
                format!("Script-{i}.sql")
            };
            let candidate = dir.join(name);
            if !candidate.exists() {
                return Ok(candidate);
            }
        }
        Err(DbError::config("Too many scripts in this folder"))
    }

    /// Creates a new script file (see [`Project::new_script_path`]) with `content`.
    pub fn create_script(&self, connection_name: Option<&str>, content: &str) -> DbResult<PathBuf> {
        let path = self.new_script_path(connection_name)?;
        write_secure(&path, content.as_bytes())?;
        Ok(path)
    }

    /// Reads a script.
    pub fn read_script(&self, path: &Path) -> DbResult<String> {
        let (abs, _) = self.resolve(path)?;
        std::fs::read_to_string(&abs).map_err(|e| io_err(&abs, e))
    }

    /// Writes a script atomically.
    pub fn save_script(&self, path: &Path, content: &str) -> DbResult<PathBuf> {
        let (abs, _) = self.resolve(path)?;
        if let Some(parent) = abs.parent() {
            ensure_private_dir(parent)?;
        }
        write_secure(&abs, content.as_bytes())?;
        Ok(abs)
    }

    /// Renames a script within its folder; its metadata follows.
    pub fn rename_script(&self, path: &Path, new_name: &str) -> DbResult<PathBuf> {
        let mut new_name = valid_name(new_name)?;
        if !new_name.to_ascii_lowercase().ends_with(".sql") {
            new_name.push_str(".sql");
        }
        let (abs, rel) = self.resolve(path)?;
        let target_rel = rel.with_file_name(&new_name);
        self.relocate(&abs, &rel, &target_rel)
    }

    /// Moves a script into `folder` (relative to Scripts; `None` = top level).
    pub fn move_script(&self, path: &Path, folder: Option<&str>) -> DbResult<PathBuf> {
        let (abs, rel) = self.resolve(path)?;
        let file = rel
            .file_name()
            .map(PathBuf::from)
            .ok_or_else(|| DbError::config("Invalid script path"))?;
        let target_rel = match folder.filter(|f| !f.trim().is_empty()) {
            Some(f) => safe_relative(Path::new(f))?.join(file),
            None => file,
        };
        self.relocate(&abs, &rel, &target_rel)
    }

    fn relocate(&self, abs: &Path, rel: &Path, target_rel: &Path) -> DbResult<PathBuf> {
        let target = self.scripts_dir().join(target_rel);
        if target == abs {
            return Ok(target);
        }
        if target.exists() {
            return Err(DbError::config(format!(
                "{} already exists",
                target.display()
            )));
        }
        if let Some(parent) = target.parent() {
            ensure_private_dir(parent)?;
        }
        std::fs::rename(abs, &target).map_err(|e| io_err(abs, e))?;
        let mut metas = self.load_metas();
        if let Some(m) = metas.remove(&rel_key(rel)) {
            metas.insert(rel_key(target_rel), m);
            self.save_metas(&metas)?;
        }
        Ok(target)
    }

    /// Deletes a script and its metadata.
    pub fn delete_script(&self, path: &Path) -> DbResult<()> {
        let (abs, rel) = self.resolve(path)?;
        std::fs::remove_file(&abs).map_err(|e| io_err(&abs, e))?;
        let mut metas = self.load_metas();
        if metas.remove(&rel_key(&rel)).is_some() {
            self.save_metas(&metas)?;
        }
        Ok(())
    }

    /// Creates a folder for scripts (relative to Scripts).
    pub fn create_script_folder(&self, relative: &str) -> DbResult<PathBuf> {
        let rel = safe_relative(Path::new(relative))?;
        if rel.as_os_str().is_empty() {
            return Err(DbError::config("The folder name is empty"));
        }
        let dir = self.scripts_dir().join(rel);
        ensure_private_dir(&dir)?;
        Ok(dir)
    }

    /// Deletes a script folder with its content and metadata.
    pub fn delete_script_folder(&self, relative: &str) -> DbResult<()> {
        let rel = safe_relative(Path::new(relative))?;
        if rel.as_os_str().is_empty() {
            return Err(DbError::config(
                "The Scripts folder itself cannot be deleted",
            ));
        }
        let dir = self.scripts_dir().join(&rel);
        std::fs::remove_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
        let prefix = format!("{}/", rel_key(&rel));
        let mut metas = self.load_metas();
        let before = metas.len();
        metas.retain(|k, _| !k.starts_with(&prefix));
        if metas.len() != before {
            self.save_metas(&metas)?;
        }
        Ok(())
    }

    fn metas_file(&self) -> PathBuf {
        self.meta_dir().join("scripts.json")
    }

    fn load_metas(&self) -> BTreeMap<String, ScriptMeta> {
        match read_json(&self.metas_file()) {
            Ok(m) => m.unwrap_or_default(),
            Err(e) => {
                tracing::warn!("script metadata unreadable, starting empty: {e}");
                BTreeMap::new()
            }
        }
    }

    fn save_metas(&self, metas: &BTreeMap<String, ScriptMeta>) -> DbResult<()> {
        write_json(&self.metas_file(), metas)
    }

    /// Connection context of a script.
    pub fn script_meta(&self, path: &Path) -> DbResult<Option<ScriptMeta>> {
        let (_, rel) = self.resolve(path)?;
        Ok(self.load_metas().get(&rel_key(&rel)).cloned())
    }

    /// Sets (or clears) the connection context of a script.
    pub fn set_script_meta(&self, path: &Path, meta: Option<ScriptMeta>) -> DbResult<()> {
        let (_, rel) = self.resolve(path)?;
        let mut metas = self.load_metas();
        match meta {
            Some(m) => {
                metas.insert(rel_key(&rel), m);
            }
            None => {
                metas.remove(&rel_key(&rel));
            }
        }
        self.save_metas(&metas)
    }

    fn bookmarks_file(&self) -> PathBuf {
        self.meta_dir().join("bookmarks.json")
    }

    /// Bookmarks of this project.
    pub fn bookmarks(&self) -> DbResult<Vec<Bookmark>> {
        Ok(read_json(&self.bookmarks_file())?.unwrap_or_default())
    }

    fn save_bookmarks(&self, list: &[Bookmark]) -> DbResult<()> {
        write_json(&self.bookmarks_file(), list)
    }

    /// Adds a bookmark (an empty id is replaced by a new one) and returns it.
    pub fn add_bookmark(&self, mut b: Bookmark) -> DbResult<Bookmark> {
        let mut list = self.bookmarks()?;
        if b.id.trim().is_empty() || list.iter().any(|x| x.id == b.id) {
            b.id = uuid::Uuid::new_v4().to_string();
        }
        list.push(b.clone());
        self.save_bookmarks(&list)?;
        Ok(b)
    }

    /// Replaces the bookmark with the same id.
    pub fn update_bookmark(&self, b: &Bookmark) -> DbResult<()> {
        let mut list = self.bookmarks()?;
        let slot = list
            .iter_mut()
            .find(|x| x.id == b.id)
            .ok_or_else(|| DbError::config(format!("Bookmark '{}' does not exist", b.id)))?;
        *slot = b.clone();
        self.save_bookmarks(&list)
    }

    /// Removes a bookmark; returns whether it existed.
    pub fn remove_bookmark(&self, id: &str) -> DbResult<bool> {
        let mut list = self.bookmarks()?;
        let before = list.len();
        list.retain(|b| b.id != id);
        if list.len() == before {
            return Ok(false);
        }
        self.save_bookmarks(&list)?;
        Ok(true)
    }

    /// Removes the bookmarks of a deleted connection; returns how many.
    pub fn remove_bookmarks_of(&self, connection_id: &str) -> DbResult<usize> {
        let mut list = self.bookmarks()?;
        let before = list.len();
        list.retain(|b| b.connection_id != connection_id);
        let removed = before - list.len();
        if removed > 0 {
            self.save_bookmarks(&list)?;
        }
        Ok(removed)
    }

    fn diagram_file(&self, name: &str) -> DbResult<PathBuf> {
        let name = valid_name(name)?;
        Ok(self.diagrams_dir().join(format!("{name}{DIAGRAM_SUFFIX}")))
    }

    /// Names of the saved diagrams, sorted.
    pub fn list_diagrams(&self) -> DbResult<Vec<String>> {
        let dir = self.diagrams_dir();
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.filter_map(Result::ok) {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(stem) = name.strip_suffix(DIAGRAM_SUFFIX) {
                    out.push(stem.to_string());
                }
            }
        }
        out.sort_by_key(|n| n.to_lowercase());
        Ok(out)
    }

    /// Loads a diagram document.
    pub fn load_diagram(&self, name: &str) -> DbResult<serde_json::Value> {
        let path = self.diagram_file(name)?;
        read_json(&path)?.ok_or_else(|| DbError::config(format!("Diagram '{name}' does not exist")))
    }

    /// Saves a diagram document (opaque JSON) and returns its path.
    pub fn save_diagram(&self, name: &str, document: &serde_json::Value) -> DbResult<PathBuf> {
        let path = self.diagram_file(name)?;
        write_json(&path, document)?;
        Ok(path)
    }

    /// Renames a diagram.
    pub fn rename_diagram(&self, old: &str, new: &str) -> DbResult<()> {
        let from = self.diagram_file(old)?;
        let to = self.diagram_file(new)?;
        if to.exists() {
            return Err(DbError::config(format!("Diagram '{new}' already exists")));
        }
        std::fs::rename(&from, &to).map_err(|e| io_err(&from, e))
    }

    /// Deletes a diagram.
    pub fn delete_diagram(&self, name: &str) -> DbResult<()> {
        let path = self.diagram_file(name)?;
        std::fs::remove_file(&path).map_err(|e| io_err(&path, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbiji_core::meta::ObjectKind;

    fn pm() -> (tempfile::TempDir, ProjectManager) {
        let dir = tempfile::tempdir().unwrap();
        let pm = ProjectManager::with_root(dir.path().join("projects")).unwrap();
        (dir, pm)
    }

    #[test]
    fn projects_crud() {
        let (_d, pm) = pm();
        let list = pm.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name(), DEFAULT_PROJECT);
        assert!(list[0].scripts_dir().is_dir());
        pm.create("Analytics").unwrap();
        pm.create("ads").unwrap();
        assert!(pm.create("Analytics").is_err());
        assert!(pm.create("../escape").is_err());
        assert!(pm.create("").is_err());
        let names: Vec<String> = pm
            .list()
            .unwrap()
            .iter()
            .map(|p| p.name().to_string())
            .collect();
        assert_eq!(names, vec!["General", "ads", "Analytics"]);
        pm.rename("Analytics", "BI").unwrap();
        assert!(pm.get("Analytics").is_err());
        assert!(pm.get("BI").unwrap().diagrams_dir().is_dir());
        assert!(pm.rename(DEFAULT_PROJECT, "X").is_err());
        assert!(pm.delete(DEFAULT_PROJECT).is_err());
        pm.delete("BI").unwrap();
        assert_eq!(pm.list().unwrap().len(), 2);
    }

    #[test]
    fn script_naming_and_files() {
        let (_d, pm) = pm();
        assert!(pm.new_script_path("Missing", None).is_err());
        let first = pm
            .new_script_path(DEFAULT_PROJECT, Some("local pg"))
            .unwrap();
        assert!(
            first.ends_with("General/Scripts/local pg/Script.sql"),
            "{}",
            first.display()
        );
        let p = pm.default_project().unwrap();
        let s0 = p.create_script(None, "select 1;").unwrap();
        assert_eq!(s0.file_name().unwrap(), "Script.sql");
        let s1 = p.create_script(None, "select 2;").unwrap();
        assert_eq!(s1.file_name().unwrap(), "Script-1.sql");
        let c0 = p.create_script(Some("Prod: Orders/DB"), "").unwrap();
        assert_eq!(c0.parent().unwrap().file_name().unwrap(), "Prod_ Orders_DB");
        assert_eq!(c0.file_name().unwrap(), "Script.sql");
        // A freed name is reused.
        p.delete_script(&s0).unwrap();
        assert_eq!(
            p.new_script_path(None).unwrap().file_name().unwrap(),
            "Script.sql"
        );

        assert_eq!(p.read_script(&s1).unwrap(), "select 2;");
        p.save_script(&s1, "select 3;").unwrap();
        assert_eq!(
            p.read_script(Path::new("Script-1.sql")).unwrap(),
            "select 3;"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&s1).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // Paths outside the project are refused.
        assert!(p.read_script(Path::new("../../etc/passwd")).is_err());
        assert!(p.save_script(Path::new("/tmp/elsewhere.sql"), "x").is_err());

        let list = p.list_scripts().unwrap();
        let rels: Vec<&str> = list.iter().map(|s| s.relative.as_str()).collect();
        assert_eq!(rels, vec!["Prod_ Orders_DB/Script.sql", "Script-1.sql"]);
        let tree = p.script_tree().unwrap();
        assert_eq!(tree.scripts.len(), 1);
        let names: Vec<&str> = tree.folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["local pg", "Prod_ Orders_DB"],
            "sorted case-insensitively"
        );
        let prod = &tree.folders[1];
        assert_eq!(prod.relative, "Prod_ Orders_DB");
        assert_eq!(prod.scripts[0].relative, "Prod_ Orders_DB/Script.sql");
        assert!(tree.folders[0].scripts.is_empty());
    }

    #[test]
    fn meta_follows_rename_move_delete() {
        let (_d, pm) = pm();
        let p = pm.default_project().unwrap();
        let s = p.create_script(None, "x").unwrap();
        let meta = ScriptMeta {
            connection_id: Some("c1".into()),
            database: Some("db".into()),
            schema: Some("public".into()),
        };
        p.set_script_meta(&s, Some(meta.clone())).unwrap();
        assert_eq!(p.script_meta(&s).unwrap(), Some(meta.clone()));
        let renamed = p.rename_script(&s, "orders report").unwrap();
        assert_eq!(renamed.file_name().unwrap(), "orders report.sql");
        assert_eq!(p.script_meta(&renamed).unwrap(), Some(meta.clone()));
        assert!(p.script_meta(Path::new("Script.sql")).unwrap().is_none());
        let moved = p.move_script(&renamed, Some("reports/2026")).unwrap();
        assert!(moved.ends_with("Scripts/reports/2026/orders report.sql"));
        assert_eq!(p.script_meta(&moved).unwrap(), Some(meta));
        assert!(p.rename_script(&moved, "bad/name").is_err());
        let other = p.create_script(None, "y").unwrap();
        assert!(p.move_script(&other, Some("../x")).is_err());
        p.delete_script_folder("reports").unwrap();
        assert!(!moved.exists());
        assert!(p.script_meta(&moved).unwrap_or(None).is_none());
        p.create_script_folder("empty/dir").unwrap();
        assert!(p.scripts_dir().join("empty/dir").is_dir());
        assert!(p.delete_script_folder("").is_err());
    }

    #[test]
    fn session_bookmarks_diagrams() {
        let (_d, pm) = pm();
        assert_eq!(pm.load_session(), SessionState::default());
        let table = ObjectRef::table(Some("public".into()), "users");
        let session = SessionState {
            tabs: vec![
                SessionTab {
                    kind: TabKind::SqlScript,
                    path: Some(PathBuf::from("/x/Script.sql")),
                    connection_id: Some("c1".into()),
                    cursor: Some(42),
                    selection: Some((40, 45)),
                    ..Default::default()
                },
                SessionTab {
                    kind: TabKind::DataEditor,
                    object: Some(table.clone()),
                    ..Default::default()
                },
                SessionTab {
                    kind: TabKind::SqlConsole,
                    content: Some("select now()".into()),
                    ..Default::default()
                },
            ],
            active: Some(1),
            layout: Some(serde_json::json!({"split": 0.6})),
        };
        pm.save_session(&session).unwrap();
        assert_eq!(pm.load_session(), session);
        // Unknown tab kinds from newer versions still load.
        std::fs::write(
            pm.root().join(".dbiji/session.json"),
            r#"{"tabs":[{"kind":"hologram"}],"active":0}"#,
        )
        .unwrap();
        assert_eq!(pm.load_session().tabs[0].kind, TabKind::Other);
        std::fs::write(pm.root().join(".dbiji/session.json"), "nope").unwrap();
        assert_eq!(pm.load_session(), SessionState::default());
        // The hidden metadata folder is not a project.
        assert_eq!(pm.list().unwrap().len(), 1);

        let p = pm.default_project().unwrap();
        let b = p
            .add_bookmark(Bookmark::new("c1", table.clone(), "Users"))
            .unwrap();
        let mut b2 = Bookmark::new("c2", ObjectRef::new(ObjectKind::View, None, "v"), "View");
        b2.id = b.id.clone(); // duplicate id gets replaced
        let b2 = p.add_bookmark(b2).unwrap();
        assert_ne!(b.id, b2.id);
        let mut changed = b.clone();
        changed.folder = Some("Important".into());
        p.update_bookmark(&changed).unwrap();
        assert_eq!(
            p.bookmarks().unwrap()[0].folder.as_deref(),
            Some("Important")
        );
        assert!(p.remove_bookmark(&b.id).unwrap());
        assert!(!p.remove_bookmark(&b.id).unwrap());
        assert_eq!(p.remove_bookmarks_of("c2").unwrap(), 1);
        assert!(p.bookmarks().unwrap().is_empty());

        let doc = serde_json::json!({"tables": [{"name": "users", "x": 10, "y": 20}]});
        let path = p.save_diagram("Sales ERD", &doc).unwrap();
        assert!(path.ends_with("Diagrams/Sales ERD.erd.json"));
        assert_eq!(p.load_diagram("Sales ERD").unwrap(), doc);
        assert_eq!(p.list_diagrams().unwrap(), vec!["Sales ERD".to_string()]);
        p.rename_diagram("Sales ERD", "Sales").unwrap();
        assert!(p.load_diagram("Sales ERD").is_err());
        assert!(p.save_diagram("../evil", &doc).is_err());
        p.delete_diagram("Sales").unwrap();
        assert!(p.list_diagrams().unwrap().is_empty());
    }

    #[test]
    fn virtual_models() {
        let (_d, pm) = pm();
        assert!(pm.virtual_model("c1").unwrap().is_empty());
        let key = VirtualTableKey {
            database: Some("shop".into()),
            schema: Some("public".into()),
            table: "orders".into(),
        };
        let mut m = VirtualModel::default();
        m.set(
            key.clone(),
            VirtualTable {
                virtual_key: Some(vec!["order_no".into()]),
                virtual_columns: vec![VirtualColumn {
                    name: "total".into(),
                    expression: "price * qty".into(),
                    kind: DataKind::Decimal,
                }],
                dictionary: Some(DictionaryConfig {
                    description_columns: vec!["name".into()],
                }),
            },
        );
        pm.save_virtual_model("c1", &m).unwrap();
        let back = pm.virtual_model("c1").unwrap();
        assert_eq!(back, m);
        let obj =
            ObjectRef::table(Some("public".into()), "orders").with_database(Some("shop".into()));
        assert_eq!(
            back.get_for(&obj).unwrap().virtual_key.as_ref().unwrap(),
            &vec!["order_no".to_string()]
        );
        let json = std::fs::read_to_string(pm.root().join(".dbiji/virtual/c1.json")).unwrap();
        assert!(json.contains("\"table\": \"orders\""), "{json}");
        // Setting an empty definition removes the table; an empty model deletes the file.
        m.set(key.clone(), VirtualTable::default());
        assert!(m.is_empty());
        pm.save_virtual_model("c1", &m).unwrap();
        assert!(!pm.root().join(".dbiji/virtual/c1.json").exists());
        pm.delete_virtual_model("c1").unwrap();
        assert!(pm.virtual_model("../x").is_ok(), "ids are sanitized");
    }
}
