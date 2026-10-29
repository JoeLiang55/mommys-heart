//! The real SharePoint document library, reached through Microsoft Graph.
//!
//! Talks to Graph directly over REST with `reqwest` and no SDK, matching
//! [`crate::server::email::communicationservice`] and
//! [`crate::server::rag::azure`]. Authentication is **app-only**: a client
//! credentials token for the tenant, so the app acts as itself rather than on
//! behalf of whoever is signed in. That is what lets the case page render a
//! folder for a volunteer who has not personally been invited to it yet.
//!
//! Item ids from Graph are opaque and already URL-safe, but every *path* the app
//! builds is percent-encoded here, because path segments carry user-supplied
//! folder and file names.

use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::RwLock;

use super::{DocumentEntry, DocumentRole, DocumentStore, FolderRef, GrantedPermission};
use crate::server::config::SharePointConfig;

const GRAPH: &str = "https://graph.microsoft.com/v1.0";

/// Refresh a token this long before it actually expires, so a request never
/// races the expiry.
const TOKEN_SKEW: Duration = Duration::from_secs(120);

/// A cached app-only access token and when it stops being usable.
struct CachedToken {
    value: String,
    expires_at: Instant,
}

pub struct GraphStore {
    http: reqwest::Client,
    tenant_id: String,
    client_id: String,
    client_secret: String,
    /// The library, resolved from the site URL once at startup.
    drive_id: String,
    /// Where in the library the app's own subtree begins.
    root_folder: String,
    /// Where volunteers' own folders begin, a sibling of `root_folder`.
    volunteer_root_folder: String,
    site_url: String,
    token: RwLock<Option<CachedToken>>,
}

impl GraphStore {
    /// Authenticate and resolve the configured site + library to a drive id.
    ///
    /// Doing the lookup once at startup means every later call is a direct
    /// `/drives/{id}` request, and a wrong site URL or library name is reported
    /// while the server is booting rather than the first time somebody opens a
    /// case.
    pub async fn connect(cfg: &SharePointConfig) -> Result<Self, String> {
        let store = Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .map_err(|e| format!("could not build HTTP client: {e}"))?,
            tenant_id: cfg.tenant_id.clone(),
            client_id: cfg.client_id.clone(),
            client_secret: cfg.client_secret.clone(),
            drive_id: String::new(),
            root_folder: cfg.root_folder.clone(),
            volunteer_root_folder: cfg.volunteer_root_folder.clone(),
            site_url: cfg.site_url.clone(),
            token: RwLock::new(None),
        };

        let site_id = store.resolve_site(&cfg.site_url).await?;
        let drive_id = store.resolve_drive(&site_id, &cfg.library).await?;
        Ok(Self { drive_id, ..store })
    }

    /// `https://contoso.sharepoint.com/sites/CaseFiles` -> Graph's site id.
    async fn resolve_site(&self, site_url: &str) -> Result<String, String> {
        let rest = site_url
            .strip_prefix("https://")
            .or_else(|| site_url.strip_prefix("http://"))
            .ok_or_else(|| format!("SHAREPOINT_SITE_URL must start with https:// (got {site_url})"))?;
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        // Graph addresses a site as {hostname}:/{server-relative-path}.
        let addressed = if path.is_empty() {
            host.to_string()
        } else {
            format!("{host}:/{path}")
        };
        let body = self
            .get_json(&format!("{GRAPH}/sites/{addressed}"))
            .await
            .map_err(|e| format!("could not resolve SharePoint site {site_url}: {e}"))?;
        body["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("SharePoint site {site_url} returned no id"))
    }

    /// The document library with this display name on the site.
    async fn resolve_drive(&self, site_id: &str, library: &str) -> Result<String, String> {
        let body = self
            .get_json(&format!("{GRAPH}/sites/{site_id}/drives"))
            .await
            .map_err(|e| format!("could not list document libraries: {e}"))?;
        let drives = body["value"]
            .as_array()
            .ok_or_else(|| "document library list was not an array".to_string())?;
        let found = drives.iter().find(|d| {
            d["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(library))
        });
        match found.and_then(|d| d["id"].as_str()) {
            Some(id) => Ok(id.to_string()),
            None => {
                let names: Vec<&str> = drives.iter().filter_map(|d| d["name"].as_str()).collect();
                Err(format!(
                    "no document library named '{library}' on that site (found: {})",
                    names.join(", ")
                ))
            }
        }
    }

    /// A valid app-only access token, minting a new one when the cached one is
    /// missing or close to expiry.
    async fn token(&self) -> Result<String, String> {
        if let Some(cached) = self.token.read().await.as_ref() {
            if Instant::now() < cached.expires_at {
                return Ok(cached.value.clone());
            }
        }

        let mut guard = self.token.write().await;
        // Another task may have refreshed it while this one waited for the lock.
        if let Some(cached) = guard.as_ref() {
            if Instant::now() < cached.expires_at {
                return Ok(cached.value.clone());
            }
        }

        let url = format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            self.tenant_id
        );
        let response = self
            .http
            .post(&url)
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("scope", "https://graph.microsoft.com/.default"),
                ("grant_type", "client_credentials"),
            ])
            .send()
            .await
            .map_err(|e| format!("token request failed: {e}"))?;

        let status = response.status();
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| format!("token response was not JSON: {e}"))?;
        if !status.is_success() {
            // Entra puts the useful part in `error_description`.
            let detail = body["error_description"]
                .as_str()
                .or_else(|| body["error"].as_str())
                .unwrap_or("unknown error");
            return Err(format!("token request rejected ({status}): {detail}"));
        }

        let value = body["access_token"]
            .as_str()
            .ok_or_else(|| "token response had no access_token".to_string())?
            .to_string();
        let lifetime = body["expires_in"].as_u64().unwrap_or(3600);
        *guard = Some(CachedToken {
            value: value.clone(),
            expires_at: Instant::now() + Duration::from_secs(lifetime).saturating_sub(TOKEN_SKEW),
        });
        Ok(value)
    }

    /// The `/drives/{id}` prefix every item request hangs off.
    fn drive(&self) -> String {
        format!("{GRAPH}/drives/{}", self.drive_id)
    }

    async fn get_json(&self, url: &str) -> Result<serde_json::Value, String> {
        let token = self.token().await?;
        let response = self
            .http
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        json_or_error(response).await
    }

    async fn post_json(
        &self,
        url: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let token = self.token().await?;
        let response = self
            .http
            .post(url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        json_or_error(response).await
    }

    async fn resolve_item_in_drive(
        &self,
        drive: &str,
        root_id: &str,
        path: &str,
    ) -> Result<Option<FolderRef>, String> {
        let url = if path.is_empty() {
            format!("{drive}/items/{root_id}")
        } else {
            let encoded = path
                .split('/')
                .map(encode_path_segment)
                .collect::<Vec<_>>()
                .join("/");
            format!("{drive}/items/{root_id}:/{encoded}")
        };
        let token = self.token().await?;
        let response = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let item = json_or_error(response).await?;
        Ok(item["id"].as_str().map(|_| folder_ref(&item)))
    }

    /// A folder's children, or `None` when the folder itself is missing.
    async fn child_named(
        &self,
        parent_id: &str,
        name: &str,
    ) -> Result<Option<serde_json::Value>, String> {
        let url = format!(
            "{}/items/{parent_id}:/{}",
            self.drive(),
            encode_path_segment(name)
        );
        let token = self.token().await?;
        let response = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        json_or_error(response).await.map(Some)
    }
}

#[async_trait::async_trait]
impl DocumentStore for GraphStore {
    async fn ensure_folder(&self, parent_id: &str, name: &str) -> Result<FolderRef, String> {
        if let Some(existing) = self.child_named(parent_id, name).await? {
            return Ok(folder_ref(&existing));
        }
        // `fail` rather than `replace`: if something raced us to the name we
        // want the existing item, which the retry below fetches.
        let created = self
            .post_json(
                &format!("{}/items/{parent_id}/children", self.drive()),
                json!({
                    "name": name,
                    "folder": {},
                    "@microsoft.graph.conflictBehavior": "fail",
                }),
            )
            .await;
        match created {
            Ok(item) => Ok(folder_ref(&item)),
            Err(e) => match self.child_named(parent_id, name).await? {
                Some(existing) => Ok(folder_ref(&existing)),
                None => Err(format!("could not create folder '{name}': {e}")),
            },
        }
    }

    async fn ensure_named_root(&self, name: &str) -> Result<FolderRef, String> {
        let library_root = self
            .get_json(&format!("{}/root", self.drive()))
            .await
            .map_err(|e| format!("could not read the library root: {e}"))?;
        let root_id = library_root["id"]
            .as_str()
            .ok_or_else(|| "library root had no id".to_string())?;
        self.ensure_folder(root_id, name).await
    }

    async fn ensure_root(&self) -> Result<FolderRef, String> {
        self.ensure_named_root(&self.root_folder).await
    }

    async fn ensure_volunteer_root(&self) -> Result<FolderRef, String> {
        self.ensure_named_root(&self.volunteer_root_folder).await
    }

    async fn list_children(&self, item_id: &str) -> Result<Vec<DocumentEntry>, String> {
        let mut entries = Vec::new();
        // Graph pages large folders; follow @odata.nextLink until it stops.
        let mut next = Some(format!(
            "{}/items/{item_id}/children?$top=200&$select=name,size,folder,file,webUrl,\
             lastModifiedDateTime,lastModifiedBy",
            self.drive()
        ));
        while let Some(url) = next {
            let body = self.get_json(&url).await?;
            if let Some(items) = body["value"].as_array() {
                for item in items {
                    entries.push(entry_from(item));
                }
            }
            next = body["@odata.nextLink"].as_str().map(str::to_string);
        }
        sort_entries(&mut entries);
        Ok(entries)
    }

    async fn resolve_item(&self, root_id: &str, path: &str) -> Result<Option<FolderRef>, String> {
        self.resolve_item_in_drive(&self.drive(), root_id, path)
            .await
    }

    async fn upload(
        &self,
        parent_id: &str,
        filename: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<(), String> {
        // A simple PUT covers everything up to 250 MB, comfortably above the
        // app's own 25 MB cap, so there is no need for an upload session.
        let url = format!(
            "{}/items/{parent_id}:/{}:/content",
            self.drive(),
            encode_path_segment(filename)
        );
        let token = self.token().await?;
        let response = self
            .http
            .put(&url)
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(bytes)
            .send()
            .await
            .map_err(|e| format!("upload failed: {e}"))?;
        json_or_error(response).await.map(|_| ())
    }

    async fn download(&self, item_id: &str) -> Result<(Vec<u8>, String), String> {
        let token = self.token().await?;
        let response = self
            .http
            .get(&format!("{}/items/{item_id}/content", self.drive()))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("download failed: {e}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let detail = response.text().await.unwrap_or_default();
            return Err(format!("download failed ({status}): {}", trim_detail(&detail)));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("reading the file failed: {e}"))?;
        Ok((bytes.to_vec(), content_type))
    }

    async fn delete(&self, item_id: &str) -> Result<(), String> {
        let token = self.token().await?;
        let response = self
            .http
            .delete(&format!("{}/items/{item_id}", self.drive()))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("delete failed: {e}"))?;
        // Already gone is the outcome the caller wanted.
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        Err(format!("delete failed ({status}): {}", trim_detail(&detail)))
    }

    async fn invite(
        &self,
        item_id: &str,
        email: &str,
        role: DocumentRole,
    ) -> Result<GrantedPermission, String> {
        // `sendInvitation: false` because the app sends its own notification
        // email, and `requireSignIn: true` so the grant is to the person rather
        // than to anyone holding a link.
        let body = self
            .post_json(
                &format!("{}/items/{item_id}/invite", self.drive()),
                json!({
                    "recipients": [{ "email": email }],
                    "requireSignIn": true,
                    "sendInvitation": false,
                    "roles": [role.slug()],
                }),
            )
            .await?;
        let permission_id = body["value"][0]["id"]
            .as_str()
            .ok_or_else(|| format!("invitation for {email} returned no permission id"))?;
        Ok(GrantedPermission {
            permission_id: permission_id.to_string(),
        })
    }

    async fn revoke(&self, item_id: &str, permission_id: &str) -> Result<(), String> {
        let token = self.token().await?;
        let response = self
            .http
            .delete(&format!(
                "{}/items/{item_id}/permissions/{permission_id}",
                self.drive()
            ))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("revoke failed: {e}"))?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        Err(format!(
            "revoke failed ({status}): {}",
            trim_detail(&detail)
        ))
    }

    fn describe(&self) -> String {
        format!("SharePoint {}/{}", self.site_url, self.root_folder)
    }
}

/// Parse a Graph response, turning a non-2xx into the message Graph put in it
/// rather than a bare status code.
async fn json_or_error(response: reqwest::Response) -> Result<serde_json::Value, String> {
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("reading the response failed: {e}"))?;
    if status.is_success() {
        if text.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        return serde_json::from_str(&text).map_err(|e| format!("response was not JSON: {e}"));
    }
    let detail = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| trim_detail(&text));
    Err(format!("Graph returned {status}: {detail}"))
}

/// Keep an error body short enough to log without flooding.
fn trim_detail(raw: &str) -> String {
    raw.chars().take(300).collect()
}

fn folder_ref(item: &serde_json::Value) -> FolderRef {
    FolderRef {
        item_id: item["id"].as_str().unwrap_or_default().to_string(),
        web_url: item["webUrl"].as_str().unwrap_or_default().to_string(),
    }
}

fn entry_from(item: &serde_json::Value) -> DocumentEntry {
    DocumentEntry {
        name: item["name"].as_str().unwrap_or_default().to_string(),
        is_folder: !item["folder"].is_null(),
        size_bytes: item["size"].as_i64().unwrap_or(0),
        modified: format_stamp(item["lastModifiedDateTime"].as_str().unwrap_or_default()),
        modified_by: item["lastModifiedBy"]["user"]["displayName"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        web_url: item["webUrl"].as_str().unwrap_or_default().to_string(),
    }
}

/// Graph's ISO-8601 stamp reduced to the `YYYY-MM-DD HH:MM` the rest of the app
/// displays.
fn format_stamp(raw: &str) -> String {
    match raw.split_once('T') {
        Some((date, time)) => format!("{date} {}", time.chars().take(5).collect::<String>()),
        None => raw.to_string(),
    }
}

/// Folders before files, then alphabetical within each — the order a file
/// browser is expected to show.
pub(super) fn sort_entries(entries: &mut [DocumentEntry]) {
    entries.sort_by(|a, b| {
        b.is_folder
            .cmp(&a.is_folder)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// Percent-encode one path segment. Segment names are user-supplied, so
/// anything outside the unreserved set is escaped rather than trusted.
fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
