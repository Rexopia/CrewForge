use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryCategory {
    Core,
    Conversation,
    Daily,
    #[serde(untagged)]
    Custom(String),
}

impl std::fmt::Display for MemoryCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Core => write!(f, "core"),
            Self::Conversation => write!(f, "conversation"),
            Self::Daily => write!(f, "daily"),
            Self::Custom(name) => write!(f, "{name}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub key: String,
    pub content: String,
    pub category: MemoryCategory,
    pub timestamp: String,
    pub score: Option<f64>,
}

// ── FileMemory (JSONL file backend) ──────────────────────────────────────────

/// Persistent memory backed by a JSONL file in the workspace.
///
/// Storage path: `{workspace}/.crewforge/memory.jsonl`
///
/// - `store` appends a new entry (one JSON line).
/// - `recall` reads all entries and scores them by keyword overlap with the query.
/// - `forget` rewrites the file excluding entries matching the key.
pub struct FileMemory {
    path: PathBuf,
}

impl FileMemory {
    pub fn new(workspace_dir: &Path) -> Self {
        Self {
            path: workspace_dir.join(".crewforge").join("memory.jsonl"),
        }
    }

    fn read_all(&self) -> Vec<MemoryEntry> {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn write_all(&self, entries: &[MemoryEntry]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::File::create(&self.path)?;
        for entry in entries {
            let line = serde_json::to_string(entry)?;
            use std::io::Write;
            writeln!(file, "{line}")?;
        }
        Ok(())
    }

    pub async fn store(&self, key: &str, content: &str, category: MemoryCategory) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let entry = MemoryEntry {
            id: uuid::Uuid::new_v4().to_string(),
            key: key.to_string(),
            content: content.to_string(),
            category,
            timestamp: chrono::Utc::now().to_rfc3339(),
            score: None,
        };
        let line = serde_json::to_string(&entry)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        use std::io::Write;
        writeln!(file, "{line}")?;
        Ok(())
    }

    pub async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
        let entries = self.read_all();
        if entries.is_empty() {
            return Ok(vec![]);
        }

        let query_words: Vec<String> = query
            .split_whitespace()
            .map(|w| w.to_lowercase())
            .filter(|w| w.len() >= 2)
            .collect();

        if query_words.is_empty() {
            let mut recent = entries;
            recent.reverse();
            recent.truncate(limit);
            return Ok(recent);
        }

        let mut scored: Vec<MemoryEntry> = entries
            .into_iter()
            .map(|mut entry| {
                let score = keyword_score(&query_words, &entry.key, &entry.content);
                entry.score = Some(score);
                entry
            })
            .filter(|e| e.score.unwrap_or(0.0) > 0.0)
            .collect();

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(limit);

        Ok(scored)
    }

    pub async fn forget(&self, key: &str) -> Result<bool> {
        let entries = self.read_all();
        let before = entries.len();
        let remaining: Vec<_> = entries.into_iter().filter(|e| e.key != key).collect();
        let removed = before > remaining.len();
        if removed {
            self.write_all(&remaining)?;
        }
        Ok(removed)
    }

    pub async fn count(&self) -> Result<usize> {
        Ok(self.read_all().len())
    }
}

/// Score an entry by fraction of query words found in key+content.
fn keyword_score(query_words: &[String], key: &str, content: &str) -> f64 {
    if query_words.is_empty() {
        return 0.0;
    }
    let haystack = format!("{key} {content}").to_lowercase();
    let matches = query_words
        .iter()
        .filter(|w| haystack.contains(w.as_str()))
        .count();
    matches as f64 / query_words.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_category_display() {
        assert_eq!(MemoryCategory::Core.to_string(), "core");
        assert_eq!(MemoryCategory::Conversation.to_string(), "conversation");
        assert_eq!(MemoryCategory::Daily.to_string(), "daily");
        assert_eq!(
            MemoryCategory::Custom("project".into()).to_string(),
            "project"
        );
    }

    #[tokio::test]
    async fn file_memory_store_and_recall() {
        let dir = tempfile::tempdir().unwrap();
        let mem = FileMemory::new(dir.path());

        mem.store("lang", "user prefers Rust", MemoryCategory::Core)
            .await
            .unwrap();
        mem.store("editor", "uses neovim", MemoryCategory::Core)
            .await
            .unwrap();
        mem.store("food", "likes sushi", MemoryCategory::Daily)
            .await
            .unwrap();

        assert_eq!(mem.count().await.unwrap(), 3);

        let results = mem.recall("Rust programming", 5).await.unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].key, "lang", "Rust query should match lang entry");
    }

    #[tokio::test]
    async fn file_memory_recall_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mem = FileMemory::new(dir.path());
        let results = mem.recall("anything", 5).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn file_memory_forget() {
        let dir = tempfile::tempdir().unwrap();
        let mem = FileMemory::new(dir.path());

        mem.store("keep", "important", MemoryCategory::Core)
            .await
            .unwrap();
        mem.store("remove", "temporary", MemoryCategory::Daily)
            .await
            .unwrap();

        assert!(mem.forget("remove").await.unwrap());
        assert_eq!(mem.count().await.unwrap(), 1);
        assert!(!mem.forget("remove").await.unwrap());
    }

    #[tokio::test]
    async fn file_memory_recall_scores_by_relevance() {
        let dir = tempfile::tempdir().unwrap();
        let mem = FileMemory::new(dir.path());

        mem.store("rust_tips", "Rust borrow checker tips", MemoryCategory::Core)
            .await
            .unwrap();
        mem.store("python_tips", "Python virtual environments", MemoryCategory::Core)
            .await
            .unwrap();
        mem.store("rust_async", "Rust async runtime tokio", MemoryCategory::Core)
            .await
            .unwrap();

        let results = mem.recall("Rust async", 5).await.unwrap();
        assert!(results.len() >= 2);
        assert_eq!(results[0].key, "rust_async");
    }

    #[tokio::test]
    async fn file_memory_recall_no_query_words_returns_recent() {
        let dir = tempfile::tempdir().unwrap();
        let mem = FileMemory::new(dir.path());

        mem.store("a", "first", MemoryCategory::Core).await.unwrap();
        mem.store("b", "second", MemoryCategory::Core)
            .await
            .unwrap();

        let results = mem.recall("x", 5).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].key, "b", "most recent first");
    }

    #[tokio::test]
    async fn file_memory_persistence() {
        let dir = tempfile::tempdir().unwrap();

        let mem1 = FileMemory::new(dir.path());
        mem1.store("key1", "value1", MemoryCategory::Core)
            .await
            .unwrap();

        let mem2 = FileMemory::new(dir.path());
        assert_eq!(mem2.count().await.unwrap(), 1);
        let results = mem2.recall("value1", 5).await.unwrap();
        assert_eq!(results[0].key, "key1");
    }

    #[test]
    fn keyword_score_empty_query() {
        assert_eq!(keyword_score(&[], "key", "content"), 0.0);
    }

    #[test]
    fn keyword_score_full_match() {
        let words = vec!["rust".into(), "async".into()];
        let score = keyword_score(&words, "rust_async", "Rust async runtime");
        assert!((score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn keyword_score_partial_match() {
        let words = vec!["rust".into(), "python".into()];
        let score = keyword_score(&words, "lang", "I use Rust");
        assert!((score - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn keyword_score_no_match() {
        let words = vec!["java".into()];
        let score = keyword_score(&words, "lang", "I use Rust");
        assert!(score.abs() < f64::EPSILON);
    }
}
