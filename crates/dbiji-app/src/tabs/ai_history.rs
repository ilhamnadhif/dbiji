//! Conversations of the AI assistant kept on disk, one JSON file each in
//! `data/ai/`, to open again from the panel's history.

use crate::tabs::ai::Turn;
use dbiji_core::meta::SchemaRef;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Conversations kept; the oldest go first.
pub const MAX_CONVERSATIONS: usize = 100;

/// A conversation as saved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedConversation {
    pub id: String,
    /// Its first question, on one line.
    pub title: String,
    pub connection: Option<String>,
    pub schema: Option<SchemaRef>,
    /// Unix seconds of its last answer.
    pub updated: i64,
    pub turns: Vec<Turn>,
}

/// The title of a conversation that starts with `question`.
pub fn title_of(question: &str) -> String {
    let line = question.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(80) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line,
    }
}

/// The saved conversations of a directory.
pub struct ConversationStore {
    dir: PathBuf,
}

impl ConversationStore {
    pub fn new(dir: impl Into<PathBuf>) -> ConversationStore {
        ConversationStore { dir: dir.into() }
    }

    fn file(&self, id: &str) -> PathBuf {
        // Ids are UUIDs; anything else cannot name a path outside the directory.
        let safe: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        self.dir.join(format!("{safe}.json"))
    }

    /// Every readable conversation, the latest first.
    pub fn list(&self) -> Vec<SavedConversation> {
        let mut all: Vec<SavedConversation> =
            std::fs::read_dir(&self.dir).into_iter().flatten().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "json")).filter_map(|e| read(&e.path())).collect();
        all.sort_by_key(|c| std::cmp::Reverse(c.updated));
        all
    }

    /// Write `c` (replacing its earlier version), then drop the oldest beyond
    /// [`MAX_CONVERSATIONS`].
    pub fn save(&self, c: &SavedConversation) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.file(&c.id);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(c)?)?;
        std::fs::rename(&tmp, &path)?;
        // By the files' times: without reading them.
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&self.dir)?
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path()))
            .collect();
        files.sort_by_key(|f| std::cmp::Reverse(f.0));
        for (_, old) in files.into_iter().skip(MAX_CONVERSATIONS) {
            let _ = std::fs::remove_file(old);
        }
        Ok(())
    }

    pub fn delete(&self, id: &str) -> std::io::Result<()> {
        match std::fs::remove_file(self.file(id)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    pub fn clear(&self) -> std::io::Result<()> {
        for c in self.list() {
            self.delete(&c.id)?;
        }
        Ok(())
    }
}

fn read(path: &Path) -> Option<SavedConversation> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation(id: &str, updated: i64) -> SavedConversation {
        SavedConversation { id: id.into(), title: title_of(&format!("Question\n {id}")), connection: None, schema: None, updated, turns: Vec::new() }
    }

    #[test]
    fn saved_listed_replaced_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(dir.path().join("ai"));
        assert!(store.list().is_empty(), "no directory yet");
        store.save(&conversation("a", 1)).unwrap();
        store.save(&conversation("b", 2)).unwrap();
        let mut a = conversation("a", 3);
        a.title = "again".into();
        store.save(&a).unwrap();
        let list = store.list();
        assert_eq!(list.iter().map(|c| (c.id.as_str(), c.title.as_str())).collect::<Vec<_>>(), [("a", "again"), ("b", "Question b")]);
        store.delete("a").unwrap();
        store.delete("a").unwrap();
        assert_eq!(store.list().len(), 1);
        // Not a way out of the directory.
        store.delete("../../x").unwrap();
        store.clear().unwrap();
        assert!(store.list().is_empty());
    }

    #[test]
    fn the_oldest_go() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(dir.path());
        for i in 0..MAX_CONVERSATIONS as i64 + 2 {
            store.save(&conversation(&format!("c{i}"), i)).unwrap();
        }
        let list = store.list();
        assert_eq!(list.len(), MAX_CONVERSATIONS);
        assert!(list.iter().all(|c| c.id != "c0" && c.id != "c1"));
    }
}
