//! Case documents: the server functions and the download endpoint.
//!
//! A case's files live in a SharePoint document library
//! ([`crate::server::sharepoint`]), not in this database. This module is the
//! whole surface the browser talks to:
//!
//! * [`list_case_documents`] — what is in a folder, with the breadcrumb.
//! * [`upload_case_document`] — a multipart upload, validated by content.
//! * [`create_case_document_folder`] / [`delete_case_document`].
//! * [`provision_case_documents`] — create a case's folder if it is missing.
//! * [`routes`] — a plain Axum **GET** download. A binary download with a
//!   `Content-Disposition` attachment does not map onto a `#[server]` function
//!   (the streaming codec is POST-only and server-fn URLs are hashed), so it
//!   stays a plain route but lives here with the rest of the surface.
//!
//! # How a folder is addressed
//!
//! Never by library id. The browser sends the case id and a **path relative to
//! the case folder** — `Intake/Service Agreement` — which the server validates
//! and resolves against the id it has stored for that case. The first segment
//! names a top-level folder, and that is what decides the audience, so the
//! existing `require_cap` + `require_visibility` gate applies before the library
//! is touched at all. There is no id a caller could substitute to reach another
//! case's files.

use leptos::prelude::*;
use leptos::server_fn::codec::{MultipartData, MultipartFormData};
use serde::{Deserialize, Serialize};

use crate::helpers::visibility::Visibility;

/// One entry in a case folder, as the browser sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaseDocument {
    pub name: String,
    pub is_folder: bool,
    pub size_bytes: i64,
    /// Pre-formatted, because it is only ever displayed.
    pub modified: String,
    pub modified_by: String,
    /// Path relative to the case folder, which is how this entry is addressed
    /// in every follow-up request.
    pub path: String,
    /// Link to the item in SharePoint, for "open in SharePoint".
    pub web_url: String,
}

/// A folder listing: where you are, what is in it, and what you may do here.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CaseDocumentListing {
    /// The path that was listed, relative to the case folder. Empty at the top.
    pub path: String,
    pub entries: Vec<CaseDocument>,
    /// Who can see what is in this folder. `None` at the top level, which shows
    /// only the standing folders the viewer is allowed to see.
    pub visibility: Option<Visibility>,
    /// Whether the case's folder exists yet. `false` means provisioning has not
    /// run or did not succeed.
    pub ready: bool,
    /// Link to this folder in SharePoint. Empty for the on-disk store.
    pub web_url: String,
    /// Whether the viewer may add files and folders here.
    pub can_upload: bool,
    /// Whether the viewer may delete what is here.
    pub can_delete: bool,
}

/// Maximum accepted upload size (25 MB), enforced by an explicit byte check.
pub const MAX_SIZE_BYTES: usize = 25 * 1024 * 1024;

/// Content-sniffed MIME types accepted for uploads: PDFs, common images, and
/// Office documents (modern + legacy). The declared type is ignored; only the
/// bytes' true type (via `infer`) is trusted, which defeats extension and
/// `Content-Type` spoofing.
#[cfg(feature = "ssr")]
const ALLOWED_MIME: &[&str] = &[
    "application/pdf",
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    // Office Open XML (docx / xlsx / pptx)
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    // Legacy Office (doc / xls / ppt)
    "application/msword",
    "application/vnd.ms-excel",
    "application/vnd.ms-powerpoint",
];

/// Resolve a request for a folder within a case: check the caller's capability
/// and audience, then turn the relative path into a library item id.
///
/// This is the single gate every operation in this module goes through, which
/// is why none of them take an item id.
#[cfg(feature = "ssr")]
async fn resolve_folder(
    case_id: &str,
    path: &str,
    cap: crate::server_fns::capabilities::CaseCapability,
) -> Result<(crate::server_fns::users::User, Vec<String>, String), ServerFnError> {
    use crate::server::permissions::{require_cap, require_user, require_visibility};
    use crate::server::sharepoint;

    let user = require_user().await?;
    require_cap(&user, case_id, cap).await?;

    let segments = sharepoint::clean_relative_path(path).map_err(ServerFnError::new)?;
    // The top-level folder decides the audience; the root itself belongs to no
    // single one, and is filtered per entry when listed.
    //
    // A folder the caller's audience does not cover gets the same answer as one
    // that does not exist. Distinguishing them would let a client confirm which
    // volunteer-only folders a case has — the same reason
    // [`require_channel`](crate::server::permissions::require_channel) hides the
    // volunteer-only chat behind "not found".
    match sharepoint::visibility_of(&segments) {
        Some(visibility) if require_visibility(&user, visibility).is_err() => {
            return Err(ServerFnError::new("That folder is not part of this case."));
        }
        Some(_) => {}
        None if !segments.is_empty() => {
            return Err(ServerFnError::new("That folder is not part of this case."));
        }
        None => {}
    }

    // The case's folder, created now if this case has none yet — provisioning
    // may never have reached the library.
    let folder = sharepoint::sync::ensure_folder_ref(case_id)
        .await
        .map_err(ServerFnError::new)?;

    let store = sharepoint::store().map_err(ServerFnError::new)?;
    let relative = segments.join("/");
    let item_id = store
        .resolve_path(&folder.item_id, &relative)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new("That folder no longer exists."))?;

    Ok((user, segments, item_id))
}

/// What is inside a case folder (requires the `ViewEvidence` capability).
///
/// At the top level only the standing folders the caller's audience allows are
/// returned, so a client never learns that the volunteer-only folders exist.
#[server(prefix = "/api")]
pub async fn list_case_documents(
    case_id: String,
    path: String,
) -> Result<CaseDocumentListing, ServerFnError> {
    use crate::server::permissions::{has_volunteer_access, require_cap, require_user};
    use crate::server::sharepoint;
    use crate::server_fns::capabilities::CaseCapability;

    let user = require_user().await?;
    require_cap(&user, &case_id, CaseCapability::ViewEvidence).await?;

    let caps = crate::server::permissions::capabilities_on(&user, &case_id).await?;
    let can_upload = caps.contains(&CaseCapability::UploadEvidence);
    let can_delete = caps.contains(&CaseCapability::DeleteEvidence);

    let segments = sharepoint::clean_relative_path(&path).map_err(ServerFnError::new)?;
    let visibility = sharepoint::visibility_of(&segments);

    // Create the folder tree if this case has never had one, so a case that
    // predates the feature simply works when somebody opens it. If the library
    // cannot be reached the panel says so and offers a retry, rather than
    // failing the whole case page.
    let folder = match sharepoint::sync::ensure_folder_ref(&case_id).await {
        Ok(folder) => folder,
        Err(e) => {
            tracing::warn!("could not prepare the documents folder for {case_id}: {e}");
            return Ok(CaseDocumentListing {
                path: segments.join("/"),
                ready: false,
                can_upload,
                can_delete,
                ..Default::default()
            });
        }
    };

    // Same audience gate as every other case-information read, and the same
    // indistinguishable answer for a folder this account may not see as for one
    // that is not there.
    match sharepoint::visibility_of(&segments) {
        Some(v) if crate::server::permissions::require_visibility(&user, v).is_err() => {
            return Err(ServerFnError::new("That folder is not part of this case."));
        }
        Some(_) => {}
        None if !segments.is_empty() => {
            return Err(ServerFnError::new("That folder is not part of this case."));
        }
        None => {}
    }

    let store = sharepoint::store().map_err(ServerFnError::new)?;
    let relative = segments.join("/");
    let Some(selected_folder) = store
        .resolve_folder(&folder.item_id, &relative)
        .await
        .map_err(ServerFnError::new)?
    else {
        return Err(ServerFnError::new("That folder no longer exists."));
    };

    let raw = store
        .list_children(&selected_folder.item_id)
        .await
        .map_err(ServerFnError::new)?;

    let sees_volunteer_only = has_volunteer_access(&user);
    let entries = raw
        .into_iter()
        .filter(|entry| {
            // At the top level, hide the folders this account's audience does
            // not cover. Deeper down the whole subtree shares one audience,
            // already checked above.
            if !segments.is_empty() {
                return true;
            }
            match sharepoint::visibility_of(&[entry.name.clone()]) {
                Some(v) => !v.is_restricted() || sees_volunteer_only,
                // A folder somebody made directly in SharePoint, outside the
                // standing tree. It names no audience, so it is treated as
                // volunteer-only rather than shown to a client by accident.
                None => sees_volunteer_only,
            }
        })
        .map(|entry| CaseDocument {
            path: if relative.is_empty() {
                entry.name.clone()
            } else {
                format!("{relative}/{}", entry.name)
            },
            name: entry.name,
            is_folder: entry.is_folder,
            size_bytes: entry.size_bytes,
            modified: entry.modified,
            modified_by: entry.modified_by,
            web_url: entry.web_url,
        })
        .collect();

    Ok(CaseDocumentListing {
        path: relative,
        entries,
        visibility,
        ready: true,
        web_url: selected_folder.web_url,
        can_upload,
        can_delete,
    })
}

/// Upload a file into a case folder (requires the `UploadEvidence` capability).
///
/// The multipart body carries `case_id`, `path` (the folder, relative to the
/// case folder) and `file`. Every byte is re-validated regardless of what the
/// client claims: size cap and content-sniffed type allowlist. A file of the
/// same name is replaced, which is what "upload the signed copy" means in
/// practice; SharePoint keeps the version history.
#[server(prefix = "/api", input = MultipartFormData)]
pub async fn upload_case_document(data: MultipartData) -> Result<String, ServerFnError> {
    use crate::server::sharepoint;
    use crate::server_fns::capabilities::CaseCapability;

    // On the server side `into_inner()` is always `Some`.
    let mut multipart = data
        .into_inner()
        .ok_or_else(|| ServerFnError::new("Malformed upload."))?;

    let mut case_id = String::new();
    let mut path = String::new();
    let mut filename: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ServerFnError::new(format!("Malformed upload: {e}")))?
    {
        let field_name = field.name().map(str::to_owned);
        let file_name = field.file_name().map(str::to_owned);
        match field_name.as_deref() {
            Some("file") => {
                filename = Some(file_name.unwrap_or_else(|| "upload".to_string()));
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| ServerFnError::new(format!("Could not read file: {e}")))?;
                bytes = Some(data.to_vec());
            }
            Some("case_id") => case_id = field.text().await.unwrap_or_default(),
            Some("path") => path = field.text().await.unwrap_or_default(),
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let case_id = case_id.trim().to_string();
    if case_id.is_empty() {
        return Err(ServerFnError::new("That upload named no case."));
    }

    let (user, segments, item_id) =
        resolve_folder(&case_id, &path, CaseCapability::UploadEvidence).await?;

    let (filename, bytes) = match (filename, bytes) {
        (Some(f), Some(b)) => (f, b),
        _ => return Err(ServerFnError::new("No file was provided in the upload.")),
    };

    // Content-based validation: size + true (sniffed) type allowlist.
    let content_type = validate_bytes(&bytes).map_err(ServerFnError::new)?;
    let filename = sanitize_filename(&filename);
    let size_bytes = bytes.len();

    let store = sharepoint::store().map_err(ServerFnError::new)?;
    store
        .upload(&item_id, &filename, bytes, content_type)
        .await
        .map_err(|e| ServerFnError::new(format!("Could not save the file: {e}")))?;

    let where_to = if segments.is_empty() {
        "the case folder".to_string()
    } else {
        segments.join(" / ")
    };
    tracing::info!(
        "document uploaded: case={case_id} path='{}' file='{filename}' \
         type={content_type} size={size_bytes}",
        segments.join("/")
    );
    audit_document(&case_id, &user, "document", "", &filename).await;
    notify_documents_changed(
        &case_id,
        &user,
        &segments,
        format!("uploaded \"{filename}\" to {where_to}"),
    );
    Ok(filename)
}

/// Create a sub-folder inside a case folder (requires `UploadEvidence`).
///
/// A folder can only be made somewhere the caller can already see, and it
/// inherits that place's audience: there is no way to create a folder whose
/// audience disagrees with where it sits.
#[server(prefix = "/api")]
pub async fn create_case_document_folder(
    case_id: String,
    path: String,
    name: String,
) -> Result<(), ServerFnError> {
    use crate::server::sharepoint;
    use crate::server_fns::capabilities::CaseCapability;

    let (user, segments, item_id) =
        resolve_folder(&case_id, &path, CaseCapability::UploadEvidence).await?;

    if segments.len() >= sharepoint::MAX_DEPTH {
        return Err(ServerFnError::new(
            "Folders cannot be nested any deeper here.",
        ));
    }
    let name = sharepoint::clean_name(&name).map_err(ServerFnError::new)?;

    let store = sharepoint::store().map_err(ServerFnError::new)?;
    store
        .ensure_folder(&item_id, &name)
        .await
        .map_err(|e| ServerFnError::new(format!("Could not create the folder: {e}")))?;

    audit_document(&case_id, &user, "folder", "", &name).await;
    notify_documents_changed(
        &case_id,
        &user,
        &segments,
        format!("added the folder \"{name}\""),
    );
    Ok(())
}

/// Delete a file or folder from a case (requires the `DeleteEvidence`
/// capability).
///
/// Two things are refused. The standing folders every case is created with are
/// part of the filing scheme rather than something somebody added. And a folder
/// with anything in it has to be emptied first: deleting a whole tree in one
/// click is how people lose paperwork they meant to keep, and the files inside
/// have their own delete.
#[server(prefix = "/api")]
pub async fn delete_case_document(case_id: String, path: String) -> Result<(), ServerFnError> {
    use crate::server::sharepoint;
    use crate::server_fns::capabilities::CaseCapability;

    let (user, segments, item_id) =
        resolve_folder(&case_id, &path, CaseCapability::DeleteEvidence).await?;

    let Some(name) = segments.last().cloned() else {
        return Err(ServerFnError::new(
            "The case folder itself cannot be deleted.",
        ));
    };
    if segments.len() == 1 && sharepoint::visibility_of(&segments).is_some() {
        return Err(ServerFnError::new(
            "The standing folders are part of every case and cannot be deleted.",
        ));
    }

    // A filed case note record is refused for the same reason a case note has no
    // delete path at all: the library is where that record lives now, so
    // deleting the file here would be deleting the note's official document.
    // Corrections are made by appending an addendum, which re-files it.
    if segments.len() == 2
        && segments[0].eq_ignore_ascii_case(sharepoint::CASE_NOTES_FOLDER)
        && crate::server::db::case_note_documents::is_filed_record(&case_id, &name)
            .await
            .unwrap_or(false)
    {
        return Err(ServerFnError::new(
            "This is a case note's filed record and cannot be deleted. \
             Correct a note by adding an addendum to it.",
        ));
    }

    let store = sharepoint::store().map_err(ServerFnError::new)?;

    // Whether this is a folder is read from the parent's listing rather than
    // guessed from the name, so a file called "Notes" is never mistaken for one.
    let parent_path = segments[..segments.len() - 1].join("/");
    let folder = crate::server::db::case_documents::folder_ref(&case_id)
        .await
        .map_err(ServerFnError::new)?;
    let parent_id = store
        .resolve_path(&folder.item_id, &parent_path)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new("That folder no longer exists."))?;
    let is_folder = store
        .list_children(&parent_id)
        .await
        .map_err(ServerFnError::new)?
        .into_iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.is_folder)
        .unwrap_or(false);

    if is_folder {
        let contents = store
            .list_children(&item_id)
            .await
            .map_err(ServerFnError::new)?;
        if !contents.is_empty() {
            return Err(ServerFnError::new("Empty the folder before deleting it."));
        }
    }

    store
        .delete(&item_id)
        .await
        .map_err(|e| ServerFnError::new(format!("Could not delete it: {e}")))?;

    audit_document(&case_id, &user, "document", &name, "").await;
    notify_documents_changed(&case_id, &user, &segments, format!("removed \"{name}\""));
    Ok(())
}

/// Create a case's documents folder if it does not exist yet (requires
/// `UploadEvidence`).
///
/// Provisioning normally happens when the case is created; this is the retry for
/// when the library was unreachable then. It is idempotent, so pressing it twice
/// is harmless.
#[server(prefix = "/api")]
pub async fn provision_case_documents(case_id: String) -> Result<(), ServerFnError> {
    use crate::server::permissions::{require_cap, require_user};
    use crate::server::sharepoint::sync;
    use crate::server_fns::capabilities::CaseCapability;

    let user = require_user().await?;
    require_cap(&user, &case_id, CaseCapability::UploadEvidence).await?;

    sync::ensure_case_folder(&case_id)
        .await
        .map_err(|e| ServerFnError::new(format!("Could not set up the folder: {e}")))?;
    sync::sync_case_access(case_id);
    Ok(())
}

/// Record a document change in the case's history, so the Change Log and the
/// admin activity feed see it exactly as they saw evidence changes before.
#[cfg(feature = "ssr")]
async fn audit_document(
    case_id: &str,
    user: &crate::server_fns::users::User,
    field: &str,
    old_value: &str,
    new_value: &str,
) {
    use crate::server::db::{audit, pool};

    if let Err(e) = audit::record(
        pool(),
        audit::Entity::Case,
        case_id,
        &user.full_name(),
        field,
        old_value,
        new_value,
    )
    .await
    {
        tracing::warn!("could not record a document change for case {case_id}: {e}");
    }
}

/// Tell the case's people that its documents changed, honoring each recipient's
/// notification settings. The audience follows the folder, so a change in a
/// volunteer-only folder never emails the client.
#[cfg(feature = "ssr")]
fn notify_documents_changed(
    case_id: &str,
    user: &crate::server_fns::users::User,
    segments: &[String],
    detail: String,
) {
    use crate::server::sharepoint;

    let visibility = sharepoint::visibility_of(segments).unwrap_or(Visibility::VolunteerOnly);
    crate::server::notifications::notify_case(
        case_id.to_string(),
        user.id.clone(),
        user.full_name(),
        crate::server_fns::settings::NotificationKind::EvidenceChanged,
        detail,
        crate::server::notifications::audience_for(visibility),
    );
}

/// Validate size and content-sniffed type, returning the canonical MIME type to
/// store (derived from the bytes, never from the client's declared type).
///
/// The cap and the allowlist are arguments because other features file into the
/// same library under narrower rules (a volunteer resume is PDF or Word, and
/// smaller), and only one place should decide whether bytes are acceptable.
#[cfg(feature = "ssr")]
pub(crate) fn validate_upload(
    bytes: &[u8],
    max_bytes: usize,
    allowed: &[&str],
    described: &str,
) -> Result<&'static str, String> {
    if bytes.is_empty() {
        return Err("The uploaded file is empty.".to_string());
    }
    if bytes.len() > max_bytes {
        return Err(format!(
            "File is too large ({:.1} MB); the limit is {} MB.",
            bytes.len() as f64 / (1024.0 * 1024.0),
            max_bytes / (1024 * 1024)
        ));
    }
    let kind = infer::get(bytes)
        .ok_or_else(|| format!("Unrecognized file type. Allowed: {described}."))?;
    let mime = kind.mime_type();
    if allowed.contains(&mime) {
        Ok(mime)
    } else {
        Err(format!(
            "File type '{mime}' is not allowed. Allowed: {described}."
        ))
    }
}

/// Validate an upload against the case-document rules.
#[cfg(feature = "ssr")]
fn validate_bytes(bytes: &[u8]) -> Result<&'static str, String> {
    validate_upload(
        bytes,
        MAX_SIZE_BYTES,
        ALLOWED_MIME,
        "PDF, images, and Office documents",
    )
}

/// Reduce a client-supplied filename to a safe name: strip any path components
/// and control characters, drop what SharePoint rejects, and cap the length.
#[cfg(feature = "ssr")]
pub(crate) fn sanitize_filename(raw: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw)
        .trim()
        .replace(['\r', '\n', '\0'], "");
    let base: String = base
        .chars()
        .map(|c| if "*:<>?\"|#%".contains(c) { '_' } else { c })
        .collect();
    let base = base.trim_matches('.').trim().to_string();
    let cleaned = if base.is_empty() {
        "upload".to_string()
    } else {
        base
    };
    cleaned.chars().take(200).collect()
}

// ---------------------------------------------------------------------------
// Download route (plain Axum GET — see the module docs for why it is not a
// `#[server]` function).
// ---------------------------------------------------------------------------

#[cfg(feature = "ssr")]
mod download {
    use axum::{
        body::Body,
        extract::{DefaultBodyLimit, Path, Query},
        http::{header, StatusCode},
        response::{IntoResponse, Response},
        routing::get,
        Router,
    };
    use serde::Deserialize;

    use crate::server::auth::AuthUser;
    use crate::server::permissions::{require_cap, require_visibility};
    use crate::server::sharepoint;
    use crate::server_fns::capabilities::CaseCapability;

    #[derive(Deserialize)]
    struct DownloadQuery {
        /// Path to the file, relative to the case folder.
        path: String,
    }

    /// Wire the whole document HTTP surface into the app router: the binary
    /// download **GET** route, and a body limit that lets a 25 MB multipart
    /// upload through (the handler still enforces the real cap on the bytes).
    ///
    /// Keeping this here means `main.rs` needs to know neither the route paths
    /// nor the size limits.
    pub fn install<S>(app: Router<S>) -> Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        app.merge(routes())
            .layer(DefaultBodyLimit::max(super::MAX_SIZE_BYTES + 1024 * 1024))
    }

    /// The document download route, ready to be merged into the app router.
    pub fn routes<S>() -> Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        Router::new().route("/api/cases/{case_id}/documents/download", get(download))
    }

    /// `GET /api/cases/{case_id}/documents/download?path=...` — stream a stored
    /// file back with a download disposition (requires `ViewEvidence`).
    async fn download(
        AuthUser(user): AuthUser,
        Path(case_id): Path<String>,
        Query(query): Query<DownloadQuery>,
    ) -> Response {
        if let Err(e) = require_cap(&user, &case_id, CaseCapability::ViewEvidence).await {
            return (StatusCode::FORBIDDEN, e.to_string()).into_response();
        }

        // Same path gate as every other document operation: validate, then let
        // the top-level folder decide the audience. A file this account's
        // audience does not cover answers exactly as a missing one does, so the
        // response never confirms that a volunteer-only folder exists.
        let segments = match sharepoint::clean_relative_path(&query.path) {
            Ok(segments) if !segments.is_empty() => segments,
            Ok(_) => return (StatusCode::BAD_REQUEST, "No file was named.").into_response(),
            Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
        };
        let permitted = sharepoint::visibility_of(&segments)
            .is_some_and(|visibility| require_visibility(&user, visibility).is_ok());
        if !permitted {
            return (StatusCode::NOT_FOUND, "That file is not part of this case.").into_response();
        }

        let folder = match sharepoint::sync::ensure_folder_ref(&case_id).await {
            Ok(folder) => folder,
            Err(e) => return (StatusCode::BAD_GATEWAY, e).into_response(),
        };

        let store = match sharepoint::store() {
            Ok(store) => store,
            Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e).into_response(),
        };

        let relative = segments.join("/");
        let item_id = match store.resolve_path(&folder.item_id, &relative).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                return (StatusCode::NOT_FOUND, "That file no longer exists.").into_response()
            }
            Err(e) => return (StatusCode::BAD_GATEWAY, e).into_response(),
        };

        match store.download(&item_id).await {
            Ok((bytes, content_type)) => {
                let filename = segments.last().cloned().unwrap_or_default();
                let disposition = format!("attachment; filename=\"{}\"", filename.replace('"', ""));
                (
                    [
                        (header::CONTENT_TYPE, content_type),
                        (header::CONTENT_DISPOSITION, disposition),
                    ],
                    Body::from(bytes),
                )
                    .into_response()
            }
            Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
        }
    }
}

#[cfg(feature = "ssr")]
pub use download::install;
