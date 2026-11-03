//! Memory management layer: every memory is a Markdown file whose front
//! matter carries its properties. Supports add, fetch, update, delete,
//! keyword search and grouping.
//!
//! Two layers: curated experience memories ([`MemoryStore`], with
//! [`ExperienceMeta`]) and append-only evidence records ([`EvidenceStore`])
//! that experience cites.

mod error;
mod evidence;
mod experience;
mod frontmatter;
mod model;
mod search;
mod store;

pub use error::{Result, StoreError};
pub use evidence::{
    render_evidence, Evidence, EvidenceHit, EvidenceId, EvidenceMeta, EvidenceQuery,
    EvidenceStore, NewEvidence, Outcome, Source, EVIDENCE_PREFIX, LEGACY_SEGMENT,
    MAX_EVIDENCE_CHARS,
};
pub use experience::{
    code_ref_commit, code_ref_path, validate_code_ref, Cites, ExperienceMeta, Status, Verified,
};
pub use frontmatter::{parse as parse_memory, render as render_memory};
pub use model::{
    is_under, normalize_folder, slugify, validate_label, validate_name, validate_title, Change,
    Filter, FolderInfo, GroupKey, Memory, MemoryId, MemoryMeta, MemoryPatch, NewMemory, Scope,
    SearchHit, SearchQuery, MAX_FOLDER_DEPTH, WORK_FOLDER,
};
pub use store::{MemoryStore, FOLDER_INDEX};
