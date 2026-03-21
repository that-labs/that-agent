//! Code analysis tools for that-tools.
//!
//! Structural code comprehension using tree-sitter AST parsing.
//! Agents get *structure* instead of raw text — AST-aware reading,
//! symbol extraction, and hybrid search.

#[cfg(feature = "code-analysis")]
pub mod astgrep;
#[cfg(feature = "code-analysis")]
pub mod edit;
pub mod git;
pub mod grep;
pub mod inventory;
#[cfg(feature = "code-analysis")]
pub mod parse;
pub mod read;
#[cfg(feature = "code-analysis")]
pub mod summary;
pub mod tree;
pub mod worktree;

pub use grep::{code_grep_filtered_with_options, GrepRuntimeOptions};
pub use read::code_read;
pub use tree::code_tree;
