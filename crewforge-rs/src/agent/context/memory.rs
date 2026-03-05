use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt::Write;
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

// ── Memory trait ─────────────────────────────────────────────────────────────

#[async_trait]
pub trait Memory: Send + Sync {
    fn name(&self) -> &str;

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
    ) -> Result<()>;

    async fn recall(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>>;

    async fn forget(&self, key: &str) -> Result<bool>;

    async fn count(&self) -> Result<usize>;
}

// ── NoneMemory (no-op backend) ───────────────────────────────────────────────

pub struct NoneMemory;

#[async_trait]
impl Memory for NoneMemory {
    fn name(&self) -> &str {
        "none"
    }

    async fn store(&self, _key: &str, _content: &str, _category: MemoryCategory) -> Result<()> {
        Ok(())
    }

    async fn recall(&self, _query: &str, _limit: usize) -> Result<Vec<MemoryEntry>> {
        Ok(vec![])
    }

    async fn forget(&self, _key: &str) -> Result<bool> {
        Ok(false)
    }

    async fn count(&self) -> Result<usize> {
        Ok(0)
    }
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

    /// Storage path for this memory backend.
    pub fn path(&self) -> &Path {
        &self.path
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
            use std::io::Write as IoWrite;
            writeln!(file, "{line}")?;
        }
        Ok(())
    }
}

#[async_trait]
impl Memory for FileMemory {
    fn name(&self) -> &str {
        "file"
    }

    async fn store(&self, key: &str, content: &str, category: MemoryCategory) -> Result<()> {
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
        use std::io::Write as IoWrite;
        writeln!(file, "{line}")?;
        Ok(())
    }

    async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
        let entries = self.read_all();
        if entries.is_empty() {
            return Ok(vec![]);
        }

        let query_words: Vec<String> = query
            .split_whitespace()
            .map(|w| w.to_lowercase())
            .filter(|w| w.len() >= 2) // skip single-char noise
            .collect();

        if query_words.is_empty() {
            // No meaningful query words — return most recent entries.
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

    async fn forget(&self, key: &str) -> Result<bool> {
        let entries = self.read_all();
        let before = entries.len();
        let remaining: Vec<_> = entries.into_iter().filter(|e| e.key != key).collect();
        let removed = before > remaining.len();
        if removed {
            self.write_all(&remaining)?;
        }
        Ok(removed)
    }

    async fn count(&self) -> Result<usize> {
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

// ── MemoryLoader ─────────────────────────────────────────────────────────────

pub struct MemoryLoader {
    limit: usize,
    min_relevance_score: f64,
}

impl Default for MemoryLoader {
    fn default() -> Self {
        Self {
            limit: 5,
            min_relevance_score: 0.3,
        }
    }
}

impl MemoryLoader {
    pub fn new(limit: usize, min_relevance_score: f64) -> Self {
        Self {
            limit: limit.max(1),
            min_relevance_score,
        }
    }

    pub async fn load_context(&self, memory: &dyn Memory, user_message: &str) -> String {
        let entries = match memory.recall(user_message, self.limit).await {
            Ok(e) => e,
            Err(_) => return String::new(),
        };

        if entries.is_empty() {
            return String::new();
        }

        let mut context = String::from("## Memory\n\nRecalled from persistent memory:\n");
        let mut has_entries = false;

        for entry in &entries {
            if let Some(score) = entry.score
                && score < self.min_relevance_score
            {
                continue;
            }
            let _ = writeln!(context, "- **{}**: {}", entry.key, entry.content);
            has_entries = true;
        }

        if !has_entries {
            return String::new();
        }

        context.push('\n');
        context
    }
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

    // ── NoneMemory tests ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn none_memory_returns_empty() {
        let mem = NoneMemory;
        assert_eq!(mem.name(), "none");
        assert!(mem.recall("anything", 5).await.unwrap().is_empty());
        assert_eq!(mem.count().await.unwrap(), 0);
        assert!(!mem.forget("key").await.unwrap());
        mem.store("k", "v", MemoryCategory::Core).await.unwrap();
    }

    // ── FileMemory tests ─────────────────────────────────────────────────────

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

        // Forgetting again returns false.
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
        // "rust_async" should score highest (matches both "rust" and "async").
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

        // Single-char query words are filtered out, so returns recent entries.
        let results = mem.recall("x", 5).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].key, "b", "most recent first");
    }

    #[tokio::test]
    async fn file_memory_persistence() {
        let dir = tempfile::tempdir().unwrap();

        // Store with one instance.
        let mem1 = FileMemory::new(dir.path());
        mem1.store("key1", "value1", MemoryCategory::Core)
            .await
            .unwrap();

        // Read with a fresh instance — data should persist.
        let mem2 = FileMemory::new(dir.path());
        assert_eq!(mem2.count().await.unwrap(), 1);
        let results = mem2.recall("value1", 5).await.unwrap();
        assert_eq!(results[0].key, "key1");
    }

    // ── keyword_score tests ──────────────────────────────────────────────────

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

    // ── MemoryLoader tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn loader_returns_empty_for_none_memory() {
        let loader = MemoryLoader::default();
        let ctx = loader.load_context(&NoneMemory, "hello").await;
        assert!(ctx.is_empty());
    }

    #[tokio::test]
    async fn loader_formats_file_memory_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mem = FileMemory::new(dir.path());
        mem.store("user_pref", "likes Rust", MemoryCategory::Core)
            .await
            .unwrap();

        let loader = MemoryLoader::new(5, 0.0);
        let ctx = loader.load_context(&mem, "Rust programming").await;
        assert!(ctx.contains("## Memory"));
        assert!(ctx.contains("**user_pref**"));
        assert!(ctx.contains("likes Rust"));
    }

    #[tokio::test]
    async fn loader_filters_low_score_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mem = FileMemory::new(dir.path());
        mem.store("irrelevant", "something about cats", MemoryCategory::Daily)
            .await
            .unwrap();

        let loader = MemoryLoader::new(5, 0.5);
        let ctx = loader.load_context(&mem, "Rust programming").await;
        assert!(ctx.is_empty(), "low-score entry should be filtered out");
    }
}
