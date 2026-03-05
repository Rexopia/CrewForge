pub mod content_search;
pub mod file_edit;
pub mod file_read;
pub mod file_write;
pub mod glob_search;
pub mod memory;
pub mod shell;
pub mod traits;

pub use content_search::ContentSearchTool;
pub use file_edit::FileEditTool;
pub use file_read::FileReadTool;
pub use file_write::FileWriteTool;
pub use glob_search::GlobSearchTool;
pub use memory::{MemoryForgetTool, MemoryRecallTool, MemoryStoreTool};
pub use shell::ShellTool;
pub use traits::{RuntimeAdapter, TokioRuntime};

use super::Tool;
use super::context::memory::FileMemory;
use super::sandbox::SecurityPolicy;
use std::sync::Arc;

/// All built-in tools backed by SecurityPolicy.
pub fn default_tools(
    security: Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
) -> Vec<Box<dyn Tool>> {
    let mem = Arc::new(FileMemory::new(&security.workspace_dir));

    vec![
        Box::new(ShellTool::new(security.clone(), runtime)),
        Box::new(FileReadTool::new(security.clone())),
        Box::new(FileWriteTool::new(security.clone())),
        Box::new(FileEditTool::new(security.clone())),
        Box::new(GlobSearchTool::new(security.clone())),
        Box::new(ContentSearchTool::new(security)),
        Box::new(MemoryStoreTool::new(mem.clone())),
        Box::new(MemoryRecallTool::new(mem.clone())),
        Box::new(MemoryForgetTool::new(mem)),
    ]
}
