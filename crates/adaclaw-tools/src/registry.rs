use adaclaw_core::tool::Tool;

pub fn all_tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(crate::shell::ShellTool::new()),
        Box::new(crate::file::FileReadTool::new()),
        Box::new(crate::file::FileWriteTool::new()),
        Box::new(crate::file::FileListTool::new()),
        Box::new(crate::memory_tools::MemoryStoreTool::new()),
        Box::new(crate::memory_tools::MemoryRecallTool::new()),
        Box::new(crate::memory_tools::MemoryForgetTool::new()),
        Box::new(crate::http::HttpRequestTool::new()),
    ]
}
