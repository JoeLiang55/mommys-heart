//! The application's server functions, grouped by concept.
//!
//! Each `#[server]` function is written **once**: Leptos generates the server
//! endpoint *and* the browser-side call, so there is no hand-written API client
//! and no separate request/response DTO layer. The client just calls these
//! functions like normal `async fn`s; under the hood they become HTTP calls to
//! auto-registered endpoints under `/api/rpc/*`.
//!
//! On the server, each function resolves the caller from the session cookie
//! (via [`crate::server::permissions`]), enforces authorization, and delegates
//! persistence to the repositories in [`crate::server::db`]. On the client, only
//! the function signature is compiled — the body never ships to the browser.
//!
//! The only surface that stays a plain REST endpoint is the chat widget
//! (`/api/chat`), which is called cross-origin from the Squarespace site and so
//! cannot use same-origin server functions.

pub mod admin_activity;
pub mod admin_requests;
pub mod audit;
pub mod auth;
pub mod bulk_properties;
pub mod capabilities;
pub mod case_contacts;
pub mod case_notes;
pub mod case_properties;
pub mod cases;
pub mod channel_notifications;
pub mod channels;
pub mod contact_directory;
pub mod contact_mail;
pub mod contact_properties;
pub mod contact_rules;
pub mod contacts;
pub mod crm;
pub mod crm_import;
pub mod documents;
pub mod email_failures;
pub mod funding;
pub mod grants;
pub mod message;
pub mod organization_properties;
pub mod organizations;
pub mod pagination;
pub mod profile;
pub mod property_filters;
pub mod reports;
pub mod settings;
pub mod users;
pub mod volunteer_applicants;
pub mod volunteer_documents;
pub mod volunteer_hours;
pub mod volunteers;

use leptos::prelude::*;

/// Flatten a [`ServerFnError`] into the plain, user-facing message we wrote
/// server-side. Domain errors are raised as `ServerFnError::ServerError(msg)`;
/// we surface `msg` directly (dropping the library's "error running server
/// function:" prefix) so the UI shows exactly the message we wrote. Shared by
/// every page/component that calls a server function, so the mapping lives in
/// one place.
pub fn err_text(e: ServerFnError) -> String {
    match e {
        ServerFnError::ServerError(m) => m,
        other => other.to_string(),
    }
}
