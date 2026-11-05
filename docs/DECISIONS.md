# Architectural decision log

| ID | Decision | Status | Rationale |
|---|---|---|---|
| ADR-001 | One Rust binary crate until a proven need for a workspace | Approved | Smaller release/test surface; single behavioral source |
| ADR-002 | Per-task feature branch and draft PR into protected main; no standing dev branch | Approved | Minimal branch drift and reliable gated review |
| ADR-003 | External shell orchestration runs Git after agent exit, not model-side push | Approved | Separates implementation and credential-bearing Git operations |
| ADR-004 | Pin Rust 1.99.1 for initial foundation | Approved for bootstrap | Latest official stable patch as checked Nov 4, 2026 |
| ADR-005 | Curated static HTTPS catalog; exact GitHub commits | Specification | No dynamic backend or publisher ingestion |
| ADR-006 | Skills copied into agent target folders with durable receipts | Specification | Predictable cross-platform operations; ownership/recovery |
| ADR-007 | MIT source license for starter | Initial choice; review before first public release | Simple permissible option per specification |
| ADR-008 | Consider Unicode skill directory names in catalog `SkillName` | Proposed | The local M1-03 validator follows the current Unicode lowercase-alphanumeric name rule; the existing catalog type remains lowercase ASCII pending an explicit compatibility decision |
| ADR-009 | Classify metadata-empty DOS ZIP entries as regular files | Approved for M2-02 | The architect approved a narrow fallback: accept only `System::Dos`, zero external attributes, no directory marker, and `is_file() == true`. Staging writes these as ordinary non-executable files. Unix entries without type bits, symlinks, special files, and ambiguous directory metadata remain rejected. The exception matches ordinary files in the pinned GitHub `frontend-design` archive. |

Proposed changes require explicit architect approval, with evidence and migration implications. A coding agent may record a proposal but must not silently change an approved decision.
