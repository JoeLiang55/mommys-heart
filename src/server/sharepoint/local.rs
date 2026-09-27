//! An on-disk stand-in for the SharePoint library, for local development.
//!
//! Case documents are kept as real directories and files under
//! `target/sharepoint/`, and sharing invitations are appended to
//! `permissions.json` beside them. That makes the whole feature — provisioning,
//! browsing, upload, download, delete, grant and revoke — exercisable on a
//! laptop with no Microsoft tenant at all.
//!
//! It is a second implementation of [`DocumentStore`], not a mock: it really
//! stores the bytes and really refuses to serve a path that escapes its root, so
//! what passes here is a genuine exercise of the calling code.
//!
//! An "item id" here is the item's path relative to the store root, so ids are
//! stable across restarts exactly as Graph's are. Every id is re-checked against
//! the root before use — see [`LocalStore::resolve`] — so a crafted id cannot
//! reach outside the store even though ids look like paths.

use std::path::{Component, Path, PathBuf};

use tokio::sync::Mutex;

use super::{DocumentEntry, DocumentRole, DocumentStore, FolderRef, GrantedPermission};
use crate::server::config::SharePointConfig;

/// Where the on-disk store lives, under the build directory so it is disposable
/// and never mistaken for something to back up.
const STORE_DIR: &str = "target/sharepoint";

pub struct LocalStore {
    root: PathBuf,
    root_folder: String,
    volunteer_root_folder: String,
    /// Serializes access to `permissions.json`.
    ///
    /// Granting and revoking are a read-modify-write of one file, and
    /// [`sync_case_access`](super::sync_case_access) fires one task per case, so
    /// several run at once whenever an account changes. Without this they
    /// interleave and the last writer drops the others' edits. The real library
    /// has no such problem — each permission is its own object there — so this
    /// lock exists only to make the on-disk store behave like it.
    permissions_lock: Mutex<()>,
}

impl LocalStore {
    pub fn new(cfg: &SharePointConfig) -> Result<Self, String> {
        let root = PathBuf::from(STORE_DIR);
        std::fs::create_dir_all(&root).map_err(|e| format!("could not create {STORE_DIR}: {e}"))?;
        Ok(Self {
            root: root
                .canonicalize()
                .map_err(|e| format!("could not resolve {STORE_DIR}: {e}"))?,
            root_folder: cfg.root_folder.clone(),
            volunteer_root_folder: cfg.volunteer_root_folder.clone(),
            permissions_lock: Mutex::new(()),
        })
    }

    /// Turn an item id (a store-relative path) into an absolute path, refusing
    /// anything that would leave the store.
    ///
    /// The check is on the *components*, before touching the filesystem, so it
    /// does not depend on the target existing and cannot be defeated by a
    /// path that only becomes absolute once resolved.
    fn resolve(&self, item_id: &str) -> Result<PathBuf, String> {
        let candidate = Path::new(item_id);
        for component in candidate.components() {
            match component {
                Component::Normal(_) => {}
                Component::CurDir => {}
                _ => return Err("that item is outside the document store".to_string()),
            }
        }
        Ok(self.root.join(candidate))
    }

    /// The item id for an absolute path: its location relative to the store root.
    fn id_of(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn permissions_file(&self) -> PathBuf {
        self.root.join("permissions.json")
    }

    /// Every invitation recorded so far.
    fn read_permissions(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.permissions_file())
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<serde_json::Value>>(&raw).ok())
            .unwrap_or_default()
    }

    fn write_permissions(&self, permissions: &[serde_json::Value]) -> Result<(), String> {
        let encoded = serde_json::to_string_pretty(permissions)
            .map_err(|e| format!("could not encode permissions: {e}"))?;
        std::fs::write(self.permissions_file(), encoded)
            .map_err(|e| format!("could not write permissions: {e}"))
    }
}

#[async_trait::async_trait]
impl DocumentStore for LocalStore {
    async fn ensure_folder(&self, parent_id: &str, name: &str) -> Result<FolderRef, String> {
        let path = self.resolve(parent_id)?.join(name);
        std::fs::create_dir_all(&path)
            .map_err(|e| format!("could not create folder '{name}': {e}"))?;
        Ok(FolderRef {
            item_id: self.id_of(&path),
            web_url: format!("file://{}", path.display()),
        })
    }

    async fn ensure_named_root(&self, name: &str) -> Result<FolderRef, String> {
        self.ensure_folder("", name).await
    }

    async fn ensure_root(&self) -> Result<FolderRef, String> {
        self.ensure_named_root(&self.root_folder).await
    }

    async fn ensure_volunteer_root(&self) -> Result<FolderRef, String> {
        self.ensure_named_root(&self.volunteer_root_folder).await
    }

    async fn list_children(&self, item_id: &str) -> Result<Vec<DocumentEntry>, String> {
        let path = self.resolve(item_id)?;
        let listing = match std::fs::read_dir(&path) {
            Ok(listing) => listing,
            // A folder that is not there yet lists as empty, matching how a
            // freshly provisioned case behaves before anything is filed.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("could not read that folder: {e}")),
        };

        let mut entries = Vec::new();
        for item in listing.flatten() {
            let name = item.file_name().to_string_lossy().to_string();
            // The bookkeeping file is part of the store, not case content.
            if name == "permissions.json" {
                continue;
            }
            let meta = match item.metadata() {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            let item_path = item.path();
            entries.push(DocumentEntry {
                name,
                is_folder: meta.is_dir(),
                size_bytes: if meta.is_dir() { 0 } else { meta.len() as i64 },
                modified: format_modified(&meta),
                modified_by: String::new(),
                web_url: format!("file://{}", item_path.display()),
            });
        }
        super::graph::sort_entries(&mut entries);
        Ok(entries)
    }

    async fn resolve_path(&self, root_id: &str, path: &str) -> Result<Option<String>, String> {
        if path.is_empty() {
            return Ok(Some(root_id.to_string()));
        }
        let root = self.resolve(root_id)?;
        let mut current = root;
        // Walk a segment at a time so a crafted segment cannot jump upwards.
        for segment in path.split('/').filter(|s| !s.is_empty()) {
            if segment == "." || segment == ".." || segment.contains('\\') {
                return Err("that path is not allowed".to_string());
            }
            current = current.join(segment);
        }
        if current.exists() {
            Ok(Some(self.id_of(&current)))
        } else {
            Ok(None)
        }
    }

    async fn upload(
        &self,
        parent_id: &str,
        filename: &str,
        bytes: Vec<u8>,
        _content_type: &str,
    ) -> Result<(), String> {
        let parent = self.resolve(parent_id)?;
        std::fs::create_dir_all(&parent)
            .map_err(|e| format!("could not prepare that folder: {e}"))?;
        std::fs::write(parent.join(filename), bytes)
            .map_err(|e| format!("could not write '{filename}': {e}"))
    }

    async fn download(&self, item_id: &str) -> Result<(Vec<u8>, String), String> {
        let path = self.resolve(item_id)?;
        let bytes = std::fs::read(&path).map_err(|e| format!("could not read that file: {e}"))?;
        // The real store reports what SharePoint recorded; here the bytes are
        // all there is, so sniff them the same way uploads are checked.
        let content_type = infer::get(&bytes)
            .map(|kind| kind.mime_type().to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        Ok((bytes, content_type))
    }

    async fn delete(&self, item_id: &str) -> Result<(), String> {
        let path = self.resolve(item_id)?;
        let outcome = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match outcome {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("could not delete that item: {e}")),
        }
    }

    async fn invite(
        &self,
        item_id: &str,
        email: &str,
        role: DocumentRole,
    ) -> Result<GrantedPermission, String> {
        // Held across the read and the write, so a concurrent grant or revoke
        // cannot overwrite this one.
        let _guard = self.permissions_lock.lock().await;
        let mut permissions = self.read_permissions();
        // Ids must stay unique even after entries in the middle are removed, so
        // they come from the highest id seen rather than from the file's length.
        let next = permissions
            .iter()
            .filter_map(|p| p["id"].as_str())
            .filter_map(|id| id.strip_prefix("local-"))
            .filter_map(|n| n.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        let permission_id = format!("local-{next}");
        permissions.push(serde_json::json!({
            "id": permission_id,
            "item_id": item_id,
            "email": email,
            "role": role.slug(),
        }));
        self.write_permissions(&permissions)?;
        tracing::info!(
            "[local documents] invited {email} to '{item_id}' as {}",
            role.slug()
        );
        Ok(GrantedPermission { permission_id })
    }

    async fn revoke(&self, item_id: &str, permission_id: &str) -> Result<(), String> {
        let _guard = self.permissions_lock.lock().await;
        let mut permissions = self.read_permissions();
        permissions.retain(|p| p["id"].as_str() != Some(permission_id));
        self.write_permissions(&permissions)?;
        tracing::info!("[local documents] revoked {permission_id} on '{item_id}'");
        Ok(())
    }

    fn describe(&self) -> String {
        format!("on-disk store at {}/{}", STORE_DIR, self.root_folder)
    }
}

/// A file's mtime as the `YYYY-MM-DD HH:MM` the rest of the app displays.
fn format_modified(meta: &std::fs::Metadata) -> String {
    let Ok(modified) = meta.modified() else {
        return String::new();
    };
    let stamp: chrono::DateTime<chrono::Local> = modified.into();
    stamp.format("%Y-%m-%d %H:%M").to_string()
}
