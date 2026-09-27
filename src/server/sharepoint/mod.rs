//! Case documents in a SharePoint document library (SSR only).
//!
//! Case files live in a SharePoint document library, not in this database. Each
//! case gets a folder there, mirroring the standing tree in
//! [`NEW_CASE_FOLDERS`](crate::helpers::new_case_folders::NEW_CASE_FOLDERS), and
//! people reach it either through the case page (which renders the folder with
//! the app's own credentials) or in SharePoint itself (through a sharing
//! invitation the app issues on their behalf).
//!
//! Postgres keeps exactly two things: where a case's folder is, and which
//! invitations we issued. Everything else is read live, so there is no mirror
//! to drift.
//!
//! # The two audiences
//!
//! A case's top-level folders each declare an audience — volunteer-only or
//! shared with the client. That distinction is real access control, so
//! **invitations are issued per top-level folder, never on the case root**: a
//! volunteer is invited to all of them, a client only to the shared ones.
//! [`sync_case_access`] is the only thing that issues or withdraws them.
//!
//! # Reaching the library
//!
//! Everything goes through the [`DocumentStore`] trait, which has two
//! implementations: [`graph`], the real library, and [`local`], a directory
//! tree used when no tenant is configured. The trait is the only path to a
//! store, so "does this work locally?" and "does this work in production?" are
//! the same question asked of two implementations.

pub mod graph;
pub mod local;
pub mod sync;
pub mod volunteers;

pub use sync::sync_case_access;

use std::sync::OnceLock;

use crate::helpers::new_case_folders::NEW_CASE_FOLDERS;
use crate::helpers::visibility::Visibility;
use crate::server::config::SharePointConfig;

/// The process-wide store, chosen once at startup by [`init`].
static STORE: OnceLock<Box<dyn DocumentStore>> = OnceLock::new();

/// One entry in a case folder: a file or a sub-folder.
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentEntry {
    /// Name as it appears in the library.
    pub name: String,
    /// Whether this is a folder (rather than a file).
    pub is_folder: bool,
    /// Size in bytes; 0 for folders.
    pub size_bytes: i64,
    /// Last-modified stamp, pre-formatted for display.
    pub modified: String,
    /// Who last changed it, as the library records it. May be empty.
    pub modified_by: String,
    /// Browser link to the item in SharePoint. May be empty for the local store.
    pub web_url: String,
}

/// A folder that was created or found, and where to reach it.
#[derive(Clone, Debug, PartialEq)]
pub struct FolderRef {
    pub item_id: String,
    pub web_url: String,
}

/// A sharing invitation that was issued.
#[derive(Clone, Debug, PartialEq)]
pub struct GrantedPermission {
    pub permission_id: String,
}

/// Read access, or read and write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentRole {
    Read,
    Write,
}

impl DocumentRole {
    /// The stored representation, which is also Graph's own word for it.
    pub fn slug(self) -> &'static str {
        match self {
            DocumentRole::Read => "read",
            DocumentRole::Write => "write",
        }
    }
}

/// Everything the app does to a document library.
///
/// Deliberately small and free of Graph vocabulary beyond the item id, so the
/// on-disk store can implement it honestly rather than by pretending to be an
/// HTTP API.
#[async_trait::async_trait]
pub trait DocumentStore: Send + Sync {
    /// Create `name` inside `parent_id`, or return the existing folder if it is
    /// already there. Idempotent, so provisioning can be retried.
    async fn ensure_folder(&self, parent_id: &str, name: &str) -> Result<FolderRef, String>;

    /// Create `name` directly under the library root, or return it if it is
    /// already there. The app owns two such roots — the case root and the
    /// volunteer root — so this is named rather than fixed.
    async fn ensure_named_root(&self, name: &str) -> Result<FolderRef, String>;

    /// The root of the library subtree case folders live in
    /// (`SHAREPOINT_ROOT_FOLDER`), creating it if absent.
    async fn ensure_root(&self) -> Result<FolderRef, String>;

    /// The root of the library subtree volunteer folders live in
    /// (`SHAREPOINT_VOLUNTEER_ROOT_FOLDER`), creating it if absent. A sibling
    /// of the case root, so nothing filed here is reachable by a case path.
    async fn ensure_volunteer_root(&self) -> Result<FolderRef, String>;

    /// The entries directly inside `item_id`, folders first then files, each
    /// alphabetically.
    async fn list_children(&self, item_id: &str) -> Result<Vec<DocumentEntry>, String>;

    /// Resolve a path *relative to* `root_id` to an item id. `None` when any
    /// step is missing. Callers must have validated the path with
    /// [`clean_relative_path`] first.
    async fn resolve_path(&self, root_id: &str, path: &str) -> Result<Option<String>, String>;

    /// Upload `bytes` as `filename` into `parent_id`, replacing any file of the
    /// same name.
    async fn upload(
        &self,
        parent_id: &str,
        filename: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<(), String>;

    /// The full bytes of a file, with its content type.
    async fn download(&self, item_id: &str) -> Result<(Vec<u8>, String), String>;

    /// Delete an item. A missing item is success, so deletion is idempotent.
    async fn delete(&self, item_id: &str) -> Result<(), String>;

    /// Invite `email` to `item_id`, returning the permission that was created.
    async fn invite(
        &self,
        item_id: &str,
        email: &str,
        role: DocumentRole,
    ) -> Result<GrantedPermission, String>;

    /// Withdraw a permission. A permission that is already gone is success.
    async fn revoke(&self, item_id: &str, permission_id: &str) -> Result<(), String>;

    /// How this store describes itself in logs and on the case page.
    fn describe(&self) -> String;
}

/// The top-level folder every case note's filed document is written to.
///
/// Named here rather than spelled out at each use because three things have to
/// agree on it: the standing tree in
/// [`NEW_CASE_FOLDERS`](crate::helpers::new_case_folders::NEW_CASE_FOLDERS),
/// the filing code that writes into it, and the paths the browser addresses
/// those documents by.
pub const CASE_NOTES_FOLDER: &str = "Case Notes";

/// The folder inside a volunteer's own folder their resume is filed in.
pub const RESUME_FOLDER: &str = "Resume";

/// Whether a document store is available at all.
pub fn is_configured() -> bool {
    STORE.get().is_some()
}

/// The initialized store. Errors rather than panics when none was configured,
/// so a request handler can report it instead of taking the server down.
pub fn store() -> Result<&'static dyn DocumentStore, String> {
    STORE
        .get()
        .map(|boxed| boxed.as_ref())
        .ok_or_else(|| "case documents are not configured on this server".to_string())
}

/// Choose and connect the document store from the environment. Safe to call
/// once at startup.
///
/// Outside production a missing or broken SharePoint configuration is not fatal:
/// the server logs it and falls back to the on-disk store, so an agent can run
/// the whole feature with nothing set up. Production refuses to start instead —
/// silently writing case files to a web server's local disk would be worse than
/// not starting.
pub async fn init() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cfg = SharePointConfig::from_env();
    let production = crate::server::config::is_production();

    let chosen: Box<dyn DocumentStore> = if cfg.wants_local() {
        Box::new(local::LocalStore::new(&cfg)?)
    } else if cfg.is_configured() {
        match graph::GraphStore::connect(&cfg).await {
            Ok(store) => Box::new(store),
            Err(e) if production => return Err(e.into()),
            Err(e) => {
                tracing::warn!(
                    "SharePoint unreachable ({e}); case documents fall back to the on-disk \
                     store. Set SHAREPOINT_BACKEND=local to silence this."
                );
                Box::new(local::LocalStore::new(&cfg)?)
            }
        }
    } else if production {
        return Err("production requires SharePoint settings for case documents".into());
    } else {
        tracing::info!(
            "SharePoint not configured; case documents use the on-disk store (see .env.example)"
        );
        Box::new(local::LocalStore::new(&cfg)?)
    };

    let description = chosen.describe();
    STORE
        .set(chosen)
        .map_err(|_| "case documents already initialized")?;
    tracing::info!("case documents ready ({description})");
    Ok(())
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// The longest a folder or file name may be, matching what the old case-folder
/// tree allowed.
pub const MAX_NAME_LEN: usize = 60;

/// How deep the tree may go below a case root, counting a top-level folder as
/// level 1. A cap keeps the breadcrumb readable and bounds path resolution.
pub const MAX_DEPTH: usize = 6;

/// Check one path segment — a folder or file name — returning it trimmed.
///
/// Names are shown in the UI *and* become path segments against the library, so
/// anything that would change the shape of a path is rejected rather than
/// quietly rewritten: a folder called `a/b` is a mistake worth reporting, not
/// two folders. `..` is rejected for the same reason it always is.
pub fn clean_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("Give the folder a name.".to_string());
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(format!("Names are limited to {MAX_NAME_LEN} characters."));
    }
    if name.contains(['/', '\\']) || name.chars().any(char::is_control) {
        return Err("A name cannot contain slashes.".to_string());
    }
    // SharePoint rejects these outright; catching them here gives a better
    // message than a 400 from Graph.
    if name.contains(['*', ':', '<', '>', '?', '"', '|', '#', '%']) {
        return Err("A name cannot contain * : < > ? \" | # or %.".to_string());
    }
    if name.chars().all(|c| c == '.') {
        return Err("Give it a real name.".to_string());
    }
    Ok(name.to_string())
}

/// Validate a path relative to a case root, returning its segments.
///
/// This is the boundary that keeps a crafted path inside its case. The browser
/// never sends an item id; it sends a path like `Intake/Service Agreement`,
/// which is checked segment by segment here before anything is resolved. An
/// empty path means the case root itself.
pub fn clean_relative_path(raw: &str) -> Result<Vec<String>, String> {
    let mut segments = Vec::new();
    for part in raw.split('/') {
        if part.trim().is_empty() {
            // Tolerate leading, trailing and doubled slashes rather than
            // failing a link that is merely untidy.
            continue;
        }
        segments.push(clean_name(part)?);
    }
    if segments.len() > MAX_DEPTH {
        return Err("That folder is nested too deeply.".to_string());
    }
    Ok(segments)
}

/// The audience of a case-relative path: the audience of the top-level folder
/// it starts in.
///
/// `None` for the case root itself, which belongs to no single audience — which
/// is why listing the root returns only the top-level folders the caller may
/// see, rather than being gated as a whole.
pub fn visibility_of(segments: &[String]) -> Option<Visibility> {
    let first = segments.first()?;
    NEW_CASE_FOLDERS
        .iter()
        .find(|spec| spec.name.eq_ignore_ascii_case(first))
        .map(|spec| spec.visibility)
}

/// The folder name a case is filed under, e.g. `Smith intake (c-12)`.
///
/// The id is part of the name so the folder stays identifiable when two cases
/// share a name, and findable by a human who only has the case id. The case
/// name is sanitized because it is user-supplied.
pub fn case_folder_name(case_id: &str, case_name: &str) -> String {
    person_folder_name(case_id, case_name, "Case")
}

/// A folder name of the form `Display name (id)`, sanitized for the library.
///
/// Shared by case folders and volunteer folders: both want a human-readable
/// name that stays unique and findable when two of them are called the same
/// thing. `fallback` names the folder when the display name sanitizes away.
pub fn person_folder_name(id: &str, display_name: &str, fallback: &str) -> String {
    let cleaned: String = display_name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || "*:<>?\"|#%".contains(c) || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let cleaned: String = cleaned.chars().take(80).collect();
    let cleaned = cleaned.trim_matches('.').trim();
    if cleaned.is_empty() {
        format!("{fallback} ({id})")
    } else {
        format!("{cleaned} ({id})")
    }
}
