use crate::agent::ToolResult;
use crate::agent::context::memory::{FileMemory, MemoryCategory};
use async_trait::async_trait;
use std::sync::Arc;

// ── MemoryStoreTool ──────────────────────────────────────────────────────────

pub struct MemoryStoreTool {
    memory: Arc<FileMemory>,
}

impl MemoryStoreTool {
    pub fn new(memory: Arc<FileMemory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl crate::agent::Tool for MemoryStoreTool {
    fn name(&self) -> &str {
        "memory_store"
    }

    fn description(&self) -> &str {
        "Store a key-value pair in persistent memory. Use this to remember facts, preferences, \
         decisions, or anything that should persist across conversations."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Short identifier for this memory (e.g. 'user_lang_pref', 'project_db_type')"
                },
                "content": {
                    "type": "string",
                    "description": "The information to remember"
                },
                "category": {
                    "type": "string",
                    "enum": ["core", "conversation", "daily"],
                    "description": "Category: 'core' for long-term facts, 'conversation' for session context, 'daily' for ephemeral notes. Default: 'core'"
                }
            },
            "required": ["key", "content"]
        })
    }

    fn is_mutating(&self) -> bool {
        true
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'key' parameter"))?;

        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'content' parameter"))?;

        let category = match args.get("category").and_then(|v| v.as_str()) {
            Some("conversation") => MemoryCategory::Conversation,
            Some("daily") => MemoryCategory::Daily,
            _ => MemoryCategory::Core,
        };

        self.memory.store(key, content, category).await?;

        Ok(ToolResult::ok(format!("Stored memory: {key}")))
    }
}

// ── MemoryRecallTool ─────────────────────────────────────────────────────────

pub struct MemoryRecallTool {
    memory: Arc<FileMemory>,
}

impl MemoryRecallTool {
    pub fn new(memory: Arc<FileMemory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl crate::agent::Tool for MemoryRecallTool {
    fn name(&self) -> &str {
        "memory_recall"
    }

    fn description(&self) -> &str {
        "Search persistent memory by keywords. Returns the most relevant stored memories."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords to search for in stored memories"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return (default: 5)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'query' parameter"))?;

        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(5);

        let entries = self.memory.recall(query, limit).await?;

        if entries.is_empty() {
            return Ok(ToolResult::ok("No memories found."));
        }

        let mut output = String::new();
        for entry in &entries {
            let score_str = entry
                .score
                .map(|s| format!(" (relevance: {s:.2})"))
                .unwrap_or_default();
            output.push_str(&format!(
                "- [{}] **{}**: {}{}\n",
                entry.category, entry.key, entry.content, score_str
            ));
        }
        output.push_str(&format!("\n{} result(s) found.", entries.len()));

        Ok(ToolResult::ok(output))
    }
}

// ── MemoryForgetTool ─────────────────────────────────────────────────────────

pub struct MemoryForgetTool {
    memory: Arc<FileMemory>,
}

impl MemoryForgetTool {
    pub fn new(memory: Arc<FileMemory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl crate::agent::Tool for MemoryForgetTool {
    fn name(&self) -> &str {
        "memory_forget"
    }

    fn description(&self) -> &str {
        "Remove a memory entry by key. Use when information is outdated or incorrect."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The key of the memory entry to remove"
                }
            },
            "required": ["key"]
        })
    }

    fn is_mutating(&self) -> bool {
        true
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'key' parameter"))?;

        let removed = self.memory.forget(key).await?;

        if removed {
            Ok(ToolResult::ok(format!("Removed memory: {key}")))
        } else {
            Ok(ToolResult::ok(format!("No memory found with key: {key}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Tool;
    use serde_json::json;

    fn make_memory() -> (tempfile::TempDir, Arc<FileMemory>) {
        let dir = tempfile::tempdir().unwrap();
        let mem = Arc::new(FileMemory::new(dir.path()));
        (dir, mem)
    }

    // ── MemoryStoreTool ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn store_basic() {
        let (_dir, mem) = make_memory();
        let tool = MemoryStoreTool::new(mem.clone());

        let result = tool
            .execute(json!({"key": "lang", "content": "prefers Rust"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("lang"));
        assert_eq!(mem.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn store_with_category() {
        let (_dir, mem) = make_memory();
        let tool = MemoryStoreTool::new(mem.clone());

        tool.execute(json!({
            "key": "meeting",
            "content": "standup at 10am",
            "category": "daily"
        }))
        .await
        .unwrap();

        let entries = mem.recall("standup", 5).await.unwrap();
        assert_eq!(entries[0].category, MemoryCategory::Daily);
    }

    #[tokio::test]
    async fn store_is_mutating() {
        let (_dir, mem) = make_memory();
        let tool = MemoryStoreTool::new(mem);
        assert!(tool.is_mutating());
    }

    // ── MemoryRecallTool ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn recall_empty() {
        let (_dir, mem) = make_memory();
        let tool = MemoryRecallTool::new(mem);

        let result = tool
            .execute(json!({"query": "anything"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("No memories found"));
    }

    #[tokio::test]
    async fn recall_finds_relevant() {
        let (_dir, mem) = make_memory();
        mem.store("rust_pref", "user prefers Rust over Python", MemoryCategory::Core)
            .await
            .unwrap();
        mem.store("food_pref", "likes sushi", MemoryCategory::Daily)
            .await
            .unwrap();

        let tool = MemoryRecallTool::new(mem);
        let result = tool
            .execute(json!({"query": "Rust programming"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("rust_pref"));
        assert!(result.output.contains("prefers Rust"));
    }

    #[tokio::test]
    async fn recall_respects_limit() {
        let (_dir, mem) = make_memory();
        for i in 0..10 {
            mem.store(&format!("key{i}"), &format!("Rust note {i}"), MemoryCategory::Core)
                .await
                .unwrap();
        }

        let tool = MemoryRecallTool::new(mem);
        let result = tool
            .execute(json!({"query": "Rust", "limit": 3}))
            .await
            .unwrap();
        assert!(result.output.contains("3 result(s)"));
    }

    #[tokio::test]
    async fn recall_is_not_mutating() {
        let (_dir, mem) = make_memory();
        let tool = MemoryRecallTool::new(mem);
        assert!(!tool.is_mutating());
    }

    // ── MemoryForgetTool ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn forget_existing() {
        let (_dir, mem) = make_memory();
        mem.store("temp", "disposable", MemoryCategory::Daily)
            .await
            .unwrap();

        let tool = MemoryForgetTool::new(mem.clone());
        let result = tool.execute(json!({"key": "temp"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Removed"));
        assert_eq!(mem.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn forget_nonexistent() {
        let (_dir, mem) = make_memory();
        let tool = MemoryForgetTool::new(mem);

        let result = tool.execute(json!({"key": "nope"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No memory found"));
    }
}
