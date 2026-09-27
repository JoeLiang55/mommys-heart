//! A volunteer's own documents: the resume on their profile.
//!
//! Like case documents, the bytes live in the SharePoint library rather than in
//! Postgres — but in the volunteer subtree, which is a sibling of the case tree
//! and carries no sharing invitations at all. Nobody reaches a resume in
//! SharePoint itself; the only ways in are the two server functions here and
//! the download route, each of which checks who is asking.
//!
//! Upload and delete are strictly self-service and always act on the caller's
//! own record, so there is no user id to forge. Downloading is allowed for the
//! owner and for operations admins, matching the volunteer agreement PDF.

use leptos::prelude::*;
use leptos::server_fn::codec::{MultipartData, MultipartFormData};

/// Maximum accepted resume size (10 MB). Smaller than a case document: a resume
/// is a couple of pages.
pub const MAX_RESUME_BYTES: usize = 10 * 1024 * 1024;

/// Content-sniffed types accepted for a resume: PDF and Word. Narrower than the
/// case allowlist on purpose — a resume is a document, not a photo.
#[cfg(feature = "ssr")]
const ALLOWED_RESUME_MIME: &[&str] = &[
    "application/pdf",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/msword",
];

/// What a resume may be, in the words both the validator and the UI use.
pub const RESUME_ACCEPT_LABEL: &str = "PDF or Word documents";

/// The caller's own volunteer record, refused unless their application has been
/// approved.
///
/// This is the gate the whole feature rests on: a pending, declined or revoked
/// applicant has no resume folder and cannot make one.
#[cfg(feature = "ssr")]
async fn require_approved_volunteer() -> Result<crate::server_fns::users::User, ServerFnError> {
    use crate::server::db::volunteers as db;
    use crate::server::permissions::require_user;
    use crate::server_fns::volunteers::VolunteerStatus;

    let user = require_user().await?;
    let record = db::get(&user.id)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new("You are not registered as a volunteer."))?;
    if record.status != VolunteerStatus::Approved {
        return Err(ServerFnError::new(
            "Your volunteer application has to be approved before you can upload a resume.",
        ));
    }
    Ok(user)
}

/// Upload (or replace) the caller's resume.
///
/// The multipart body carries only `file`; whose resume it is comes from the
/// session, never from the request. A previous resume under a different name is
/// deleted only after the new one is safely stored, so a failed upload never
/// loses the file already on file.
#[server(prefix = "/api", input = MultipartFormData)]
pub async fn upload_my_resume(data: MultipartData) -> Result<String, ServerFnError> {
    use crate::server::db::volunteers as db;
    use crate::server::sharepoint;
    use crate::server_fns::documents::{sanitize_filename, validate_upload};

    let user = require_approved_volunteer().await?;

    // On the server side `into_inner()` is always `Some`.
    let mut multipart = data
        .into_inner()
        .ok_or_else(|| ServerFnError::new("Malformed upload."))?;

    let mut filename: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ServerFnError::new(format!("Malformed upload: {e}")))?
    {
        let field_name = field.name().map(str::to_owned);
        let file_name = field.file_name().map(str::to_owned);
        if field_name.as_deref() == Some("file") {
            filename = Some(file_name.unwrap_or_else(|| "resume".to_string()));
            let data = field
                .bytes()
                .await
                .map_err(|e| ServerFnError::new(format!("Could not read file: {e}")))?;
            bytes = Some(data.to_vec());
        } else {
            let _ = field.bytes().await;
        }
    }

    let (filename, bytes) = match (filename, bytes) {
        (Some(f), Some(b)) => (f, b),
        _ => return Err(ServerFnError::new("No file was provided in the upload.")),
    };

    let content_type = validate_upload(
        &bytes,
        MAX_RESUME_BYTES,
        ALLOWED_RESUME_MIME,
        RESUME_ACCEPT_LABEL,
    )
    .map_err(ServerFnError::new)?;
    let filename = sanitize_filename(&filename);
    let size_bytes = bytes.len() as i64;

    let folder = sharepoint::volunteers::ensure_resume_folder(&user.id)
        .await
        .map_err(|e| ServerFnError::new(format!("Could not set up your folder: {e}")))?;
    let store = sharepoint::store().map_err(ServerFnError::new)?;

    let previous = db::resume_item_id(&user.id)
        .await
        .map_err(ServerFnError::new)?;

    store
        .upload(&folder.item_id, &filename, bytes, content_type)
        .await
        .map_err(|e| ServerFnError::new(format!("Could not save your resume: {e}")))?;

    let item_id = store
        .resolve_path(&folder.item_id, &filename)
        .await
        .map_err(|e| ServerFnError::new(format!("Could not save your resume: {e}")))?
        .ok_or_else(|| ServerFnError::new("The uploaded resume could not be found again."))?;

    db::set_resume(&user.id, &filename, &item_id, size_bytes)
        .await
        .map_err(ServerFnError::new)?;

    // Only now that the new file is recorded: a differently-named predecessor
    // would otherwise sit in the folder forever.
    if !previous.is_empty() && previous != item_id {
        if let Err(e) = store.delete(&previous).await {
            tracing::warn!("could not remove the previous resume for {}: {e}", user.id);
        }
    }

    tracing::info!(
        "resume uploaded: user={} file='{filename}' type={content_type} size={size_bytes}",
        user.id
    );
    audit_resume(&user, "", &filename).await;
    Ok(filename)
}

/// The caller's own resume as it now stands, for refreshing the panel after an
/// upload without reloading the whole profile.
#[server(prefix = "/api")]
pub async fn my_resume(
) -> Result<Option<crate::server_fns::volunteers::VolunteerResume>, ServerFnError> {
    use crate::server::db::volunteers as db;
    use crate::server::permissions::require_user;

    let user = require_user().await?;
    Ok(db::get(&user.id)
        .await
        .map_err(ServerFnError::new)?
        .and_then(|record| record.resume))
}

/// Remove the caller's resume, from the library and from their record.
#[server(prefix = "/api")]
pub async fn delete_my_resume() -> Result<(), ServerFnError> {
    use crate::server::db::volunteers as db;
    use crate::server::permissions::require_user;
    use crate::server::sharepoint;

    // Deliberately not `require_approved_volunteer`: someone whose approval was
    // revoked must still be able to take their own document down.
    let user = require_user().await?;

    let record = db::get(&user.id)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new("You are not registered as a volunteer."))?;
    let Some(resume) = record.resume else {
        return Ok(());
    };

    let item_id = db::resume_item_id(&user.id)
        .await
        .map_err(ServerFnError::new)?;
    if !item_id.is_empty() {
        let store = sharepoint::store().map_err(ServerFnError::new)?;
        store
            .delete(&item_id)
            .await
            .map_err(|e| ServerFnError::new(format!("Could not remove your resume: {e}")))?;
    }

    db::clear_resume(&user.id)
        .await
        .map_err(ServerFnError::new)?;
    audit_resume(&user, &resume.file_name, "").await;
    Ok(())
}

/// Record a resume change against the account, so it appears in the same
/// history every other account change does.
#[cfg(feature = "ssr")]
async fn audit_resume(user: &crate::server_fns::users::User, old_value: &str, new_value: &str) {
    use crate::server::db::{audit, pool};

    if let Err(e) = audit::record(
        pool(),
        audit::Entity::User,
        &user.id,
        &user.full_name(),
        "resume",
        old_value,
        new_value,
    )
    .await
    {
        tracing::warn!("could not record a resume change for {}: {e}", user.id);
    }
}

// ---------------------------------------------------------------------------
// Download route (plain Axum GET, for the same reason case documents use one:
// a binary attachment does not map onto a `#[server]` function).
// ---------------------------------------------------------------------------

#[cfg(feature = "ssr")]
mod download {
    use axum::{
        body::Body,
        extract::Path,
        http::{header, StatusCode},
        response::{IntoResponse, Response},
        routing::get,
        Router,
    };

    use crate::server::auth::AuthUser;
    use crate::server::db::volunteers;
    use crate::server::permissions::require_operations_admin;
    use crate::server::sharepoint;

    pub fn install<S>(router: Router<S>) -> Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        router.route("/api/volunteers/{user_id}/resume", get(download))
    }

    /// `GET /api/volunteers/{user_id}/resume` — the volunteer's resume, for
    /// themselves and for operations admins. A viewer who may not see it gets
    /// the same answer as one asking for a resume that is not there.
    async fn download(AuthUser(viewer): AuthUser, Path(user_id): Path<String>) -> Response {
        let missing = || (StatusCode::NOT_FOUND, "No resume on file.").into_response();

        if viewer.id != user_id && require_operations_admin(&viewer).is_err() {
            return missing();
        }

        let Ok(Some(record)) = volunteers::get(&user_id).await else {
            return missing();
        };
        let Some(resume) = record.resume else {
            return missing();
        };
        let Ok(item_id) = volunteers::resume_item_id(&user_id).await else {
            return missing();
        };
        if item_id.is_empty() {
            return missing();
        }

        let store = match sharepoint::store() {
            Ok(store) => store,
            Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e).into_response(),
        };
        match store.download(&item_id).await {
            Ok((bytes, content_type)) => {
                let disposition = format!(
                    "attachment; filename=\"{}\"",
                    resume.file_name.replace('"', "")
                );
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
