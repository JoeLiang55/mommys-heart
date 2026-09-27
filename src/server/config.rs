//! Azure OpenAI + RAG configuration, read from the environment (SSR only).
//!
//! Everything is optional so the server boots and degrades gracefully when
//! Azure OpenAI is not configured (e.g. local development without credentials).

/// Azure OpenAI connection + deployment settings.
#[derive(Clone, Debug)]
pub struct AzureConfig {
    pub endpoint: String,
    pub api_key: String,
    pub chat_deployment: String,
    /// Deployment used by the AI reports agent. Kept separate from
    /// [`Self::chat_deployment`] because writing SQL against an unfamiliar
    /// schema wants a stronger, slower model than the document chatbot does.
    pub report_deployment: String,
    pub embedding_deployment: String,
    pub api_version: String,
    pub docs_dir: String,
}

impl AzureConfig {
    /// Read configuration from the environment, falling back to the defaults
    /// below.
    pub fn from_env() -> Self {
        Self {
            endpoint: env("AZURE_OPENAI_ENDPOINT", ""),
            api_key: env("AZURE_OPENAI_API_KEY", ""),
            chat_deployment: env("AZURE_OPENAI_CHAT_DEPLOYMENT", "gpt-4o"),
            report_deployment: env("AZURE_OPENAI_REPORT_DEPLOYMENT", "gpt-5.6-sol"),
            embedding_deployment: env(
                "AZURE_OPENAI_EMBEDDING_DEPLOYMENT",
                "text-embedding-ada-002",
            ),
            api_version: env("AZURE_OPENAI_API_VERSION", "2024-12-01-preview"),
            docs_dir: env("DOCS_DIR", "docs"),
        }
    }

    /// Whether Azure OpenAI credentials are present. When `false`, the RAG
    /// pipeline is disabled and `/api/chat` returns a graceful fallback.
    pub fn is_configured(&self) -> bool {
        !self.endpoint.is_empty() && !self.api_key.is_empty()
    }
}

/// Azure Communication Services (ACS) Email connection settings. Read from the
/// environment, all optional so the server boots and email delivery degrades to
/// a graceful no-op when ACS is not configured (e.g. local development).
#[derive(Clone, Debug)]
pub struct EmailConfig {
    /// Resource endpoint, e.g. `https://your-resource.communication.azure.com`.
    pub endpoint: String,
    /// The resource's base64 access key (the `accesskey=` value of the ACS
    /// connection string), used to sign requests with HMAC-SHA256.
    pub access_key: String,
    /// A verified sender address, e.g. `DoNotReply@your-domain.azurecomm.net`.
    pub sender_address: String,
    /// When `true`, notification emails are *not* actually sent: the dispatcher
    /// logs what it would have sent instead. Used to keep local/test runs from
    /// emailing real recipients. Controlled by `EMAIL_DRY_RUN`.
    pub dry_run: bool,
}

impl EmailConfig {
    /// Read ACS Email configuration from the environment.
    pub fn from_env() -> Self {
        Self {
            endpoint: env("ACS_EMAIL_ENDPOINT", ""),
            access_key: env("ACS_EMAIL_ACCESS_KEY", ""),
            sender_address: env("ACS_EMAIL_SENDER_ADDRESS", ""),
            dry_run: env_bool("EMAIL_DRY_RUN", false),
        }
    }

    /// Whether all ACS Email settings are present. When `false`, email delivery
    /// is skipped (a graceful no-op) rather than erroring.
    pub fn is_configured(&self) -> bool {
        !self.endpoint.is_empty() && !self.access_key.is_empty() && !self.sender_address.is_empty()
    }
}

/// SharePoint document-library settings, read from the environment (SSR only).
///
/// Case documents live in a SharePoint document library reached through
/// Microsoft Graph with an app-only (client credentials) token. Everything is
/// optional so the server boots without it: an unconfigured deployment falls
/// back to the on-disk store below, which is what makes the whole feature
/// exercisable locally with no tenant at all.
#[derive(Clone, Debug)]
pub struct SharePointConfig {
    pub tenant_id: String,
    pub client_id: String,
    pub client_secret: String,
    /// The site holding the library, e.g. `https://contoso.sharepoint.com/sites/CaseFiles`.
    pub site_url: String,
    /// Display name of the document library within that site.
    pub library: String,
    /// Folder inside the library under which every case folder is created.
    pub root_folder: String,
    /// Folder inside the library under which each volunteer's own folder is
    /// created. A sibling of `root_folder`, never inside it, so a volunteer's
    /// paperwork cannot be reached through a case path.
    pub volunteer_root_folder: String,
    /// Which store backs case documents: `graph` (the real library) or `local`
    /// (a directory tree, for development).
    pub backend: String,
}

impl SharePointConfig {
    pub fn from_env() -> Self {
        Self {
            tenant_id: env("GRAPH_TENANT_ID", ""),
            client_id: env("GRAPH_CLIENT_ID", ""),
            client_secret: env("GRAPH_CLIENT_SECRET", ""),
            site_url: env("SHAREPOINT_SITE_URL", "")
                .trim_end_matches('/')
                .to_string(),
            library: env("SHAREPOINT_LIBRARY", "Documents"),
            root_folder: env("SHAREPOINT_ROOT_FOLDER", "Cases"),
            volunteer_root_folder: env("SHAREPOINT_VOLUNTEER_ROOT_FOLDER", "Volunteers"),
            backend: env("SHAREPOINT_BACKEND", "graph").to_ascii_lowercase(),
        }
    }

    /// Whether the on-disk development store was asked for explicitly.
    pub fn wants_local(&self) -> bool {
        self.backend == "local"
    }

    /// Whether every credential Graph needs is present.
    pub fn is_configured(&self) -> bool {
        !self.tenant_id.is_empty()
            && !self.client_id.is_empty()
            && !self.client_secret.is_empty()
            && !self.site_url.is_empty()
    }
}

/// Branding + linking values shared by every email template, so the product
/// name and the links in a message are defined once and stay consistent across
/// all notifications. Read from the environment, with sensible defaults so
/// emails render correctly even when nothing is configured.
#[derive(Clone, Debug)]
pub struct Brand {
    /// Short product name shown in headers and signatures, e.g. `Mommy's Heart`.
    pub name: String,
    /// The public base URL of the app (no trailing slash), used to build the
    /// call-to-action and settings links in emails. Empty when unset, in which
    /// case templates omit the buttons and render plain guidance instead.
    pub app_url: String,
    /// Support / "reply to" address surfaced in the footer so recipients know
    /// who to contact. Empty when unset.
    pub support_email: String,
}

impl Brand {
    /// Read branding from the environment. `APP_URL` and `SUPPORT_EMAIL` are
    /// optional; `BRAND_NAME` defaults to the product name.
    pub fn from_env() -> Self {
        Self {
            name: env("BRAND_NAME", "Mommy's Heart"),
            app_url: env("APP_URL", "https://app.example.org")
                .trim_end_matches('/')
                .to_string(),
            support_email: env("SUPPORT_EMAIL", "support@example.org"),
        }
    }
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Read a boolean environment flag. Truthy values (case-insensitive) are
/// `true`, `1`, `yes`, and `on`; anything else falls back to `default`.
fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        ),
        Err(_) => default,
    }
}

/// Whether production-only fail-closed checks apply.
pub fn is_production() -> bool {
    std::env::var("APP_ENV")
        .map(|value| value.eq_ignore_ascii_case("production"))
        .unwrap_or(!cfg!(debug_assertions))
}

/// Reject unsafe production configuration before the first request is served.
pub fn validate_production() -> Result<(), String> {
    if !is_production() {
        return Ok(());
    }
    if !EmailConfig::from_env().is_configured() {
        return Err("production requires configured ACS email so MFA cannot fail open".into());
    }
    if std::env::var("APP_URL")
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        return Err("production requires APP_URL for same-origin request validation".into());
    }
    // The on-disk document store is a development aid: in production it would
    // put case files on the web server's local disk instead of SharePoint.
    let documents = SharePointConfig::from_env();
    if documents.wants_local() {
        return Err("production cannot use SHAREPOINT_BACKEND=local for case documents".into());
    }
    if !documents.is_configured() {
        return Err("production requires SharePoint settings for case documents".into());
    }
    Ok(())
}
