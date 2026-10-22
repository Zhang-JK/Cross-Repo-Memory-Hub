//! Memory management layer: every memory is a Markdown file whose front
//! matter carries its properties. Supports add, fetch, update, delete,
//! keyword search and grouping.

mod error;
mod frontmatter;
mod model;
mod search;
mod store;

pub use error::{Result, StoreError};
pub use frontmatter::{parse as parse_memory, render as render_memory};
pub use model::{
    slugify, validate_label, validate_name, Change, Filter, GroupKey, Memory, MemoryId, MemoryMeta,
    MemoryPatch, NewMemory, Scope, SearchHit, SearchQuery,
};
pub use store::MemoryStore;
