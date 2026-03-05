use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt::Write;

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

// ── MemoryLoader ─────────────────────────────────────────────────────────────

pub struct MemoryLoader {
    limit: usize,
    min_relevance_score: f64,
}

impl Default for MemoryLoader {
    fn default() -> Self {
        Self {
            limit: 5,
            min_relevance_score: 0.4,
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

        let mut context = String::from("[Memory context]\n");
        let mut has_entries = false;

        for entry in &entries {
            if let Some(score) = entry.score
                && score < self.min_relevance_score
            {
                continue;
            }
            let _ = writeln!(context, "- {}: {}", entry.key, entry.content);
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

    #[tokio::test]
    async fn none_memory_returns_empty() {
        let mem = NoneMemory;
        assert_eq!(mem.name(), "none");
        assert!(mem.recall("anything", 5).await.unwrap().is_empty());
        assert_eq!(mem.count().await.unwrap(), 0);
        assert!(!mem.forget("key").await.unwrap());
        mem.store("k", "v", MemoryCategory::Core).await.unwrap();
    }

    #[tokio::test]
    async fn loader_returns_empty_for_none_memory() {
        let loader = MemoryLoader::default();
        let ctx = loader.load_context(&NoneMemory, "hello").await;
        assert!(ctx.is_empty());
    }

    #[tokio::test]
    async fn loader_formats_entries() {
        struct MockMemory;

        #[async_trait]
        impl Memory for MockMemory {
            fn name(&self) -> &str { "mock" }
            async fn store(&self, _: &str, _: &str, _: MemoryCategory) -> Result<()> { Ok(()) }
            async fn recall(&self, _: &str, _: usize) -> Result<Vec<MemoryEntry>> {
                Ok(vec![MemoryEntry {
                    id: "1".into(),
                    key: "user_pref".into(),
                    content: "likes Rust".into(),
                    category: MemoryCategory::Core,
                    timestamp: "now".into(),
                    score: Some(0.9),
                }])
            }
            async fn forget(&self, _: &str) -> Result<bool> { Ok(true) }
            async fn count(&self) -> Result<usize> { Ok(1) }
        }

        let loader = MemoryLoader::default();
        let ctx = loader.load_context(&MockMemory, "hello").await;
        assert!(ctx.contains("[Memory context]"));
        assert!(ctx.contains("- user_pref: likes Rust"));
    }

    #[tokio::test]
    async fn loader_filters_low_score_entries() {
        struct LowScoreMemory;

        #[async_trait]
        impl Memory for LowScoreMemory {
            fn name(&self) -> &str { "low" }
            async fn store(&self, _: &str, _: &str, _: MemoryCategory) -> Result<()> { Ok(()) }
            async fn recall(&self, _: &str, _: usize) -> Result<Vec<MemoryEntry>> {
                Ok(vec![MemoryEntry {
                    id: "1".into(),
                    key: "irrelevant".into(),
                    content: "noise".into(),
                    category: MemoryCategory::Daily,
                    timestamp: "now".into(),
                    score: Some(0.1),
                }])
            }
            async fn forget(&self, _: &str) -> Result<bool> { Ok(true) }
            async fn count(&self) -> Result<usize> { Ok(1) }
        }

        let loader = MemoryLoader::new(5, 0.4);
        let ctx = loader.load_context(&LowScoreMemory, "hello").await;
        assert!(ctx.is_empty());
    }
}
