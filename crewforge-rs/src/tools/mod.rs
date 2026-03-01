pub mod content_search;
pub mod file_edit;
pub mod file_read;
pub mod file_write;
pub mod glob_search;
pub mod shell;
pub mod traits;

pub use content_search::ContentSearchTool;
pub use file_edit::FileEditTool;
pub use file_read::FileReadTool;
pub use file_write::FileWriteTool;
pub use glob_search::GlobSearchTool;
pub use shell::ShellTool;
pub use traits::{RuntimeAdapter, TokioRuntime};

use crate::agent::Tool;
use crate::security::SecurityPolicy;
use std::sync::Arc;

/// All built-in tools backed by SecurityPolicy.
pub fn default_tools(
    security: Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ShellTool::new(security.clone(), runtime)),
        Box::new(FileReadTool::new(security.clone())),
        Box::new(FileWriteTool::new(security.clone())),
        Box::new(FileEditTool::new(security.clone())),
        Box::new(GlobSearchTool::new(security.clone())),
        Box::new(ContentSearchTool::new(security)),
    ]
}

/// All tools including future memory/browser tools.
/// For now, same as default_tools.
pub fn all_tools(
    security: Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
) -> Vec<Box<dyn Tool>> {
    default_tools(security, runtime)
}
