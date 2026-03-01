use adaclaw_core::memory::Memory;
use adaclaw_core::tool::Tool;
use std::sync::Arc;

/// Build the full list of built-in tools, optionally wiring up a memory backend.
///
/// When `memory` is `Some`, the three memory tools (`memory_store`,
/// `memory_recall`, `memory_forget`) are connected to the real backend and will
/// perform actual reads/writes.  When `memory` is `None` (e.g. tests that don't
/// need memory, or a none-backend configuration) the tools are still registered
/// but return a descriptive error on execution rather than panicking.
pub fn all_tools(memory: Option<Arc<dyn Memory>>) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(crate::shell::ShellTool::new()),
        Box::new(crate::file::FileReadTool::new()),
        Box::new(crate::file::FileWriteTool::new()),
        Box::new(crate::file::FileListTool::new()),
        Box::new(match &memory {
            Some(m) => crate::memory_tools::MemoryStoreTool::with_memory(Arc::clone(m)),
            None    => crate::memory_tools::MemoryStoreTool::new(),
        }),
        Box::new(match &memory {
            Some(m) => crate::memory_tools::MemoryRecallTool::with_memory(Arc::clone(m)),
            None    => crate::memory_tools::MemoryRecallTool::new(),
        }),
        Box::new(match &memory {
            Some(m) => crate::memory_tools::MemoryForgetTool::with_memory(Arc::clone(m)),
            None    => crate::memory_tools::MemoryForgetTool::new(),
        }),
        Box::new(crate::http::HttpRequestTool::new()),
    ]
}
