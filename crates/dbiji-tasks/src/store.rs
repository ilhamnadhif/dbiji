//! Persistent task definitions (`tasks.json`).

use crate::model::{TaskDef, TaskKind};
use chrono::Utc;
use dbiji_core::{DbError, DbResult, tr, trf};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio::sync::watch;

const FORMAT: &str = "dbiji-tasks";
const VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    tasks: Vec<TaskDef>,
    /// Folders created explicitly (folders holding tasks exist implicitly).
    #[serde(default)]
    folders: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ExportFile {
    format: String,
    version: u32,
    tasks: Vec<TaskDef>,
}

/// Task definitions stored as `tasks.json` in a directory. All mutations are
/// written to disk immediately (atomically, mode 0600 on Unix) and announced
/// to [`TaskStore::subscribe`]rs. Safe to share between threads.
pub struct TaskStore {
    path: PathBuf,
    data: RwLock<StoreFile>,
    changes: watch::Sender<u64>,
}

impl std::fmt::Debug for TaskStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskStore").field("path", &self.path).field("tasks", &self.data.read().tasks.len()).finish()
    }
}

fn norm_folder(folder: &str) -> String {
    folder.split('/').map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>().join("/")
}

fn io_err(what: &str, path: &Path, e: std::io::Error) -> DbError {
    DbError::io(format!("{what} {}: {e}", path.display()))
}

/// Ids of composite steps reachable from `start` (detects cycles).
fn has_cycle(tasks: &HashMap<&str, &TaskDef>, start: &str) -> bool {
    fn visit<'a>(tasks: &HashMap<&'a str, &'a TaskDef>, id: &'a str, stack: &mut Vec<&'a str>, done: &mut HashSet<&'a str>) -> bool {
        if stack.contains(&id) {
            return true;
        }
        if done.contains(id) {
            return false;
        }
        stack.push(id);
        if let Some(TaskDef { kind: TaskKind::Composite { steps, .. }, .. }) = tasks.get(id) {
            for s in steps {
                if visit(tasks, s, stack, done) {
                    return true;
                }
            }
        }
        stack.pop();
        done.insert(id);
        false
    }
    visit(tasks, start, &mut Vec::new(), &mut HashSet::new())
}

impl TaskStore {
    /// File name inside the store directory.
    pub const FILE_NAME: &'static str = "tasks.json";

    /// Open (or create) the store in `dir`.
    pub fn open(dir: impl AsRef<Path>) -> DbResult<TaskStore> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(|e| io_err("Cannot create", dir, e))?;
        let path = dir.join(Self::FILE_NAME);
        let data = Self::read_file(&path)?;
        let (changes, _) = watch::channel(0);
        Ok(TaskStore { path, data: RwLock::new(data), changes })
    }

    fn read_file(path: &Path) -> DbResult<StoreFile> {
        match std::fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => Ok(StoreFile { version: VERSION, ..Default::default() }),
            Ok(text) => serde_json::from_str(&text).map_err(|e| DbError::config(trf("Cannot read {}: {}", &[&path.display(), &e]))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StoreFile { version: VERSION, ..Default::default() }),
            Err(e) => Err(io_err("Cannot read", path, e)),
        }
    }

    /// Path of `tasks.json`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-read the file (after another process changed it).
    pub fn reload(&self) -> DbResult<()> {
        let data = Self::read_file(&self.path)?;
        *self.data.write() = data;
        self.notify();
        Ok(())
    }

    /// Receiver that changes whenever the store is modified.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn notify(&self) {
        self.changes.send_modify(|v| *v = v.wrapping_add(1));
    }

    fn persist(&self, data: &StoreFile) -> DbResult<()> {
        let mut out = data.clone();
        out.version = VERSION;
        let text = serde_json::to_string_pretty(&out).map_err(|e| DbError::internal(e.to_string()))?;
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp).map_err(|e| io_err("Cannot write", &tmp, e))?;
            f.write_all(text.as_bytes()).map_err(|e| io_err("Cannot write", &tmp, e))?;
            f.sync_all().map_err(|e| io_err("Cannot write", &tmp, e))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| io_err("Cannot write", &self.path, e))
    }

    /// Apply `f` to a copy of the data, validate, persist and publish it.
    fn mutate<T>(&self, f: impl FnOnce(&mut StoreFile) -> DbResult<T>) -> DbResult<T> {
        let mut guard = self.data.write();
        let mut copy = guard.clone();
        let result = f(&mut copy)?;
        Self::check_composites(&copy.tasks)?;
        self.persist(&copy)?;
        *guard = copy;
        drop(guard);
        self.notify();
        Ok(result)
    }

    fn check_composites(tasks: &[TaskDef]) -> DbResult<()> {
        let by_id: HashMap<&str, &TaskDef> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
        for t in tasks {
            if let TaskKind::Composite { steps, .. } = &t.kind {
                if let Some(missing) = steps.iter().find(|s| !by_id.contains_key(s.as_str())) {
                    return Err(DbError::config(trf("Step task {} of {} does not exist", &[missing, &t.name])));
                }
                if has_cycle(&by_id, &t.id) {
                    return Err(DbError::config(trf("The composite task {} contains itself through its steps", &[&t.name])));
                }
            }
        }
        Ok(())
    }

    /// All tasks, ordered by folder and name.
    pub fn list(&self) -> Vec<TaskDef> {
        let mut v = self.data.read().tasks.clone();
        v.sort_by(|a, b| a.path().to_lowercase().cmp(&b.path().to_lowercase()).then_with(|| a.id.cmp(&b.id)));
        v
    }

    /// A task by id.
    pub fn get(&self, id: &str) -> Option<TaskDef> {
        self.data.read().tasks.iter().find(|t| t.id == id).cloned()
    }

    /// Find a task by id, by `folder/name` path or by (unique, case-insensitive) name.
    pub fn find(&self, id_or_name: &str) -> DbResult<TaskDef> {
        let key = id_or_name.trim();
        let data = self.data.read();
        if let Some(t) = data.tasks.iter().find(|t| t.id == key) {
            return Ok(t.clone());
        }
        let by_path: Vec<&TaskDef> = data.tasks.iter().filter(|t| t.path().eq_ignore_ascii_case(key)).collect();
        if by_path.len() == 1 {
            return Ok(by_path[0].clone());
        }
        let by_name: Vec<&TaskDef> = data.tasks.iter().filter(|t| t.name.trim().eq_ignore_ascii_case(key)).collect();
        match by_name.len() {
            1 => Ok(by_name[0].clone()),
            0 => Err(DbError::config(trf("No task named {}", &[&key]))),
            _ => Err(DbError::config(trf("Several tasks are named {}; use the task id", &[&key]))),
        }
    }

    /// Add a new task (an empty id gets a fresh one). Returns the stored task.
    pub fn create(&self, mut task: TaskDef) -> DbResult<TaskDef> {
        if task.id.trim().is_empty() {
            task.id = uuid::Uuid::new_v4().to_string();
        }
        task.folder = task.folder.as_deref().map(norm_folder).filter(|f| !f.is_empty());
        let now = Utc::now();
        task.created = now;
        task.updated = now;
        task.validate()?;
        self.mutate(|d| {
            if d.tasks.iter().any(|t| t.id == task.id) {
                return Err(DbError::config(trf("A task with id {} already exists", &[&task.id])));
            }
            d.tasks.push(task.clone());
            Ok(task.clone())
        })
    }

    /// Replace an existing task (matched by id). Returns the stored task.
    pub fn update(&self, mut task: TaskDef) -> DbResult<TaskDef> {
        task.folder = task.folder.as_deref().map(norm_folder).filter(|f| !f.is_empty());
        task.updated = Utc::now();
        task.validate()?;
        self.mutate(|d| {
            let slot = d.tasks.iter_mut().find(|t| t.id == task.id).ok_or_else(|| DbError::config(trf("No task with id {}", &[&task.id])))?;
            task.created = slot.created;
            *slot = task.clone();
            Ok(task.clone())
        })
    }

    /// Enable or disable a task.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> DbResult<()> {
        let mut t = self.get(id).ok_or_else(|| DbError::config(trf("No task with id {}", &[&id])))?;
        t.enabled = enabled;
        self.update(t).map(|_| ())
    }

    /// Delete a task; it is also removed from composite tasks. Returns false
    /// when no task had that id.
    pub fn delete(&self, id: &str) -> DbResult<bool> {
        self.mutate(|d| {
            let before = d.tasks.len();
            d.tasks.retain(|t| t.id != id);
            if d.tasks.len() == before {
                return Ok(false);
            }
            let now = Utc::now();
            for t in &mut d.tasks {
                if let TaskKind::Composite { steps, .. } = &mut t.kind
                    && steps.iter().any(|s| s == id)
                {
                    steps.retain(|s| s != id);
                    t.updated = now;
                }
            }
            // a composite left without steps is invalid: keep it, but disabled
            for t in &mut d.tasks {
                if matches!(&t.kind, TaskKind::Composite { steps, .. } if steps.is_empty()) {
                    t.enabled = false;
                }
            }
            Ok(true)
        })
    }

    /// Copy a task under a new id and name ("<name> (copy)").
    pub fn duplicate(&self, id: &str) -> DbResult<TaskDef> {
        let mut t = self.get(id).ok_or_else(|| DbError::config(trf("No task with id {}", &[&id])))?;
        t.id = uuid::Uuid::new_v4().to_string();
        t.name = trf("{} (copy)", &[&t.name]);
        t.os_scheduler = false;
        self.create(t)
    }

    /// Move a task to `folder` (`None` = top level).
    pub fn move_task(&self, id: &str, folder: Option<&str>) -> DbResult<()> {
        let mut t = self.get(id).ok_or_else(|| DbError::config(trf("No task with id {}", &[&id])))?;
        t.folder = folder.map(str::to_string);
        self.update(t).map(|_| ())
    }

    /// All folders: explicit ones and those holding tasks (with their parents), sorted.
    pub fn folders(&self) -> Vec<String> {
        let data = self.data.read();
        let mut set = BTreeSet::new();
        for f in data.folders.iter().cloned().chain(data.tasks.iter().filter_map(|t| t.folder.clone())) {
            let f = norm_folder(&f);
            let mut acc = String::new();
            for part in f.split('/').filter(|p| !p.is_empty()) {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(part);
                set.insert(acc.clone());
            }
        }
        set.into_iter().collect()
    }

    /// Create an (empty) folder.
    pub fn create_folder(&self, path: &str) -> DbResult<()> {
        let p = norm_folder(path);
        if p.is_empty() {
            return Err(DbError::config(tr("The folder needs a name")));
        }
        self.mutate(|d| {
            if !d.folders.contains(&p) {
                d.folders.push(p.clone());
            }
            Ok(())
        })
    }

    /// Rename a folder (and its sub-folders). Returns the number of tasks moved.
    pub fn rename_folder(&self, old: &str, new: &str) -> DbResult<usize> {
        let (old, new) = (norm_folder(old), norm_folder(new));
        if old.is_empty() || new.is_empty() {
            return Err(DbError::config(tr("The folder needs a name")));
        }
        let rename = |f: &str| -> Option<String> { if f == old { Some(new.clone()) } else { f.strip_prefix(&format!("{old}/")).map(|rest| format!("{new}/{rest}")) } };
        self.mutate(|d| {
            let mut moved = 0;
            for t in &mut d.tasks {
                if let Some(n) = t.folder.as_deref().and_then(rename) {
                    t.folder = Some(n);
                    moved += 1;
                }
            }
            for f in &mut d.folders {
                if let Some(n) = rename(f) {
                    *f = n;
                }
            }
            Ok(moved)
        })
    }

    /// Delete a folder; its tasks and sub-folders move to the parent folder.
    pub fn delete_folder(&self, path: &str) -> DbResult<()> {
        let p = norm_folder(path);
        let parent = p.rsplit_once('/').map(|(a, _)| a.to_string());
        let lift = |f: &str| -> Option<Option<String>> {
            if f == p {
                Some(parent.clone())
            } else {
                f.strip_prefix(&format!("{p}/")).map(|rest| {
                    Some(match &parent {
                        Some(pp) => format!("{pp}/{rest}"),
                        None => rest.to_string(),
                    })
                })
            }
        };
        self.mutate(|d| {
            for t in &mut d.tasks {
                if let Some(n) = t.folder.as_deref().and_then(lift) {
                    t.folder = n;
                }
            }
            d.folders = d
                .folders
                .iter()
                .filter_map(|f| match lift(f) {
                    Some(n) => n,
                    None => Some(f.clone()),
                })
                .collect();
            Ok(())
        })
    }

    /// Export task definitions as JSON. Empty `ids` exports every task;
    /// composite steps are included automatically.
    pub fn export_json(&self, ids: &[String]) -> DbResult<String> {
        let data = self.data.read();
        let mut wanted: BTreeSet<String> = if ids.is_empty() { data.tasks.iter().map(|t| t.id.clone()).collect() } else { ids.iter().cloned().collect() };
        loop {
            let mut more = Vec::new();
            for t in data.tasks.iter().filter(|t| wanted.contains(&t.id)) {
                if let TaskKind::Composite { steps, .. } = &t.kind {
                    more.extend(steps.iter().filter(|s| !wanted.contains(*s)).cloned());
                }
            }
            if more.is_empty() {
                break;
            }
            wanted.extend(more);
        }
        if let Some(missing) = ids.iter().find(|id| !data.tasks.iter().any(|t| &t.id == *id)) {
            return Err(DbError::config(trf("No task with id {}", &[missing])));
        }
        let tasks: Vec<TaskDef> = data.tasks.iter().filter(|t| wanted.contains(&t.id)).cloned().collect();
        serde_json::to_string_pretty(&ExportFile { format: FORMAT.into(), version: VERSION, tasks }).map_err(|e| DbError::internal(e.to_string()))
    }

    /// Import task definitions exported by [`TaskStore::export_json`] (or a
    /// plain JSON array of tasks). Tasks whose id already exists get a new id;
    /// composite steps are remapped. Returns the imported tasks.
    pub fn import_json(&self, json: &str) -> DbResult<Vec<TaskDef>> {
        let tasks: Vec<TaskDef> = match serde_json::from_str::<ExportFile>(json) {
            Ok(f) => {
                if f.format != FORMAT {
                    return Err(DbError::config(trf("Not a DBiji task export (format {})", &[&f.format])));
                }
                f.tasks
            }
            Err(_) => serde_json::from_str::<Vec<TaskDef>>(json).map_err(|e| DbError::config(trf("Invalid task file: {}", &[&e])))?,
        };
        self.mutate(|d| {
            // An id per task: taken, empty or repeated in the file → a new one. Composite
            // steps refer to the first task of an id.
            let mut used: HashSet<String> = d.tasks.iter().map(|t| t.id.clone()).collect();
            let mut map: HashMap<String, String> = HashMap::new();
            let mut ids: Vec<String> = Vec::with_capacity(tasks.len());
            for t in &tasks {
                let new_id = if t.id.trim().is_empty() || used.contains(&t.id) { uuid::Uuid::new_v4().to_string() } else { t.id.clone() };
                used.insert(new_id.clone());
                map.entry(t.id.clone()).or_insert_with(|| new_id.clone());
                ids.push(new_id);
            }
            let now = Utc::now();
            let mut imported = Vec::new();
            for (t, id) in tasks.iter().zip(ids) {
                let mut n = t.clone();
                n.id = id;
                if let TaskKind::Composite { steps, .. } = &mut n.kind {
                    for s in steps.iter_mut() {
                        if let Some(m) = map.get(s) {
                            *s = m.clone();
                        }
                    }
                }
                n.updated = now;
                n.os_scheduler = false;
                n.folder = n.folder.as_deref().map(norm_folder).filter(|f| !f.is_empty());
                n.validate()?;
                imported.push(n);
            }
            d.tasks.extend(imported.iter().cloned());
            Ok(imported)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::Schedule;

    fn shell(name: &str) -> TaskDef {
        TaskDef::new(name, TaskKind::Shell { command: format!("echo {name}") })
    }

    #[test]
    fn crud_roundtrip_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::open(dir.path()).unwrap();
        let mut rx = store.subscribe();
        let a = store.create(shell("Backup prod").with_folder("/ops//nightly/").with_schedule(Schedule::Interval { every_secs: 60 })).unwrap();
        assert_eq!(a.folder.as_deref(), Some("ops/nightly"));
        assert!(rx.has_changed().unwrap());
        rx.borrow_and_update();
        let b = store.create(shell("Export")).unwrap();
        let mut a2 = a.clone();
        a2.description = Some("desc".into());
        let a3 = store.update(a2).unwrap();
        assert_eq!(a3.created, a.created);
        assert!(a3.updated >= a.updated);
        assert!(rx.has_changed().unwrap());
        // persisted
        let reopened = TaskStore::open(dir.path()).unwrap();
        assert_eq!(reopened.list().len(), 2);
        assert_eq!(reopened.get(&a.id).unwrap().description.as_deref(), Some("desc"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // find by id, path and name
        assert_eq!(store.find(&b.id).unwrap().name, "Export");
        assert_eq!(store.find("export").unwrap().id, b.id);
        assert_eq!(store.find("ops/nightly/Backup prod").unwrap().id, a.id);
        assert!(store.find("nope").is_err());
        store.create(shell("export")).unwrap();
        assert!(store.find("Export").unwrap_err().message.contains("Several"));
        // duplicate / delete
        let c = store.duplicate(&a.id).unwrap();
        assert_eq!(c.name, "Backup prod (copy)");
        assert_ne!(c.id, a.id);
        assert!(store.delete(&c.id).unwrap());
        assert!(!store.delete(&c.id).unwrap());
        assert!(store.update(c).is_err());
        // invalid tasks are rejected and nothing is written
        assert!(store.create(shell("")).is_err());
        assert_eq!(TaskStore::open(dir.path()).unwrap().list().len(), 3);
    }

    #[test]
    fn composites_are_validated() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::open(dir.path()).unwrap();
        let a = store.create(shell("a")).unwrap();
        let comp = |steps: Vec<String>| TaskDef::new("c", TaskKind::Composite { steps, stop_on_error: true, parallel: false });
        assert!(store.create(comp(vec!["missing".into()])).is_err());
        let c1 = store.create(comp(vec![a.id.clone()])).unwrap();
        let c2 = store.create(comp(vec![c1.id.clone()])).unwrap();
        // c1 -> c2 -> c1 is a cycle
        let mut c1b = c1.clone();
        c1b.kind = TaskKind::Composite { steps: vec![c2.id.clone()], stop_on_error: true, parallel: false };
        let err = store.update(c1b).unwrap_err();
        assert!(err.message.contains("contains itself"), "{err}");
        // deleting a step removes it from composites
        assert!(store.delete(&a.id).unwrap());
        let c1 = store.get(&c1.id).unwrap();
        assert!(matches!(&c1.kind, TaskKind::Composite { steps, .. } if steps.is_empty()));
        assert!(!c1.enabled);
    }

    #[test]
    fn folders() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::open(dir.path()).unwrap();
        let t = store.create(shell("x").with_folder("a/b")).unwrap();
        store.create_folder("empty").unwrap();
        assert_eq!(store.folders(), vec!["a", "a/b", "empty"]);
        assert_eq!(store.rename_folder("a", "z").unwrap(), 1);
        assert_eq!(store.get(&t.id).unwrap().folder.as_deref(), Some("z/b"));
        store.delete_folder("z").unwrap();
        assert_eq!(store.get(&t.id).unwrap().folder.as_deref(), Some("b"));
        store.move_task(&t.id, None).unwrap();
        assert_eq!(store.get(&t.id).unwrap().folder, None);
        assert_eq!(store.folders(), vec!["empty"]);
        assert!(store.create_folder(" / ").is_err());
    }

    #[test]
    fn export_import() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::open(dir.path()).unwrap();
        let a = store.create(shell("a")).unwrap();
        let b = store.create(shell("b")).unwrap();
        let c = store.create(TaskDef::new("c", TaskKind::Composite { steps: vec![a.id.clone(), b.id.clone()], stop_on_error: false, parallel: true })).unwrap();
        let json = store.export_json(std::slice::from_ref(&c.id)).unwrap();
        assert!(json.contains("\"format\": \"dbiji-tasks\""));
        // importing into the same store duplicates with fresh ids and remapped steps
        let imported = store.import_json(&json).unwrap();
        assert_eq!(imported.len(), 3);
        let ic = imported.iter().find(|t| t.name == "c").unwrap();
        assert_ne!(ic.id, c.id);
        let TaskKind::Composite { steps, .. } = &ic.kind else { panic!("composite") };
        assert!(steps.iter().all(|s| imported.iter().any(|t| &t.id == s) && s != &a.id && s != &b.id));
        assert_eq!(store.list().len(), 6);
        // into an empty store the ids are kept; a plain array is accepted too
        let dir2 = tempfile::tempdir().unwrap();
        let other = TaskStore::open(dir2.path()).unwrap();
        let imported = other.import_json(&json).unwrap();
        assert!(imported.iter().any(|t| t.id == c.id));
        let arr = serde_json::to_string(&vec![shell("solo")]).unwrap();
        assert_eq!(other.import_json(&arr).unwrap().len(), 1);
        assert!(other.import_json("{\"format\":\"other\",\"version\":1,\"tasks\":[]}").is_err());
        assert!(other.import_json("not json").is_err());
        assert!(store.export_json(&["missing".to_string()]).is_err());
    }

    #[test]
    fn corrupt_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(TaskStore::FILE_NAME), "{ not json").unwrap();
        assert!(TaskStore::open(dir.path()).is_err());
        std::fs::write(dir.path().join(TaskStore::FILE_NAME), "").unwrap();
        assert!(TaskStore::open(dir.path()).unwrap().list().is_empty());
    }
}
