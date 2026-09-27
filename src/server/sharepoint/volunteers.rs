//! Each volunteer's own folder in the document library, and the resume filed
//! inside it.
//!
//! Volunteer paperwork lives beside the case tree, never inside it: the folders
//! here hang off [`ensure_volunteer_root`](super::DocumentStore::ensure_volunteer_root),
//! a sibling of the case root. That separation is the access control. No
//! sharing invitation is ever issued on this subtree, so a resume is reachable
//! only through the app, which checks who is asking.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use tokio::sync::Mutex;

use crate::server::db::users;
use crate::server::sharepoint::{person_folder_name, store, FolderRef, RESUME_FOLDER};

/// One lock per volunteer, so two provisioning runs for the same person cannot
/// both walk the creation path. Same reasoning as the per-case lock in
/// [`super::sync`].
static USER_LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();

async fn lock_for(user_id: &str) -> Arc<Mutex<()>> {
    let locks = USER_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = locks.lock().await;
    map.entry(user_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// The volunteer's `Resume` folder, creating their folder and it if needed.
///
/// Idempotent, and it does not record anything: the folder is found again by
/// name, so there is no pointer to keep in step. The resume's own item id *is*
/// recorded, because that is what a download and a replace need.
pub async fn ensure_resume_folder(user_id: &str) -> Result<FolderRef, String> {
    let user_lock = lock_for(user_id).await;
    let _guard = user_lock.lock().await;

    let store = store()?;

    let user = users::get(user_id)
        .await
        .map_err(|e| format!("database error: {e}"))?
        .ok_or_else(|| "that account no longer exists".to_string())?;

    let root = store.ensure_volunteer_root().await?;
    let folder_name = person_folder_name(user_id, &user.full_name(), "Volunteer");
    let personal = store.ensure_folder(&root.item_id, &folder_name).await?;
    store.ensure_folder(&personal.item_id, RESUME_FOLDER).await
}
