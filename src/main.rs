// Deep `view!` trees monomorphise into very large types; without
// `--cfg erase_components` the default limit overflows during layout.
#![recursion_limit = "512"]

#[cfg(feature = "ssr")]
#[tokio::main]
async fn main() {
    use axum::{middleware, Router};
    use leptos::prelude::*;
    use leptos_axum::{generate_route_list, LeptosRoutes};
    use mommys_heart_app::app::{shell, App};
    use mommys_heart_app::server::{api, rag, telemetry};
    use mommys_heart_app::server_fns;
    use tower_http::trace::TraceLayer;

    let _ = dotenvy::dotenv();

    // Start structured logging first so startup (incl. DB migrations/seeding) is
    // captured. Verbosity is controlled by `RUST_LOG`.
    telemetry::init();

    // Production never starts with MFA or origin validation disabled. Secure
    // cookies and transport headers follow the same production-mode decision.
    if let Err(error) = mommys_heart_app::server::config::validate_production() {
        panic!("unsafe production configuration: {error}");
    }

    if std::env::args().nth(1).as_deref() == Some("seed") {
        if let Err(e) = mommys_heart_app::server::db::init().await {
            panic!("failed to initialize database: {e}");
        }
        if let Err(e) = mommys_heart_app::server::db::seed::reseed().await {
            panic!("failed to seed database: {e}");
        }
        tracing::info!("demo data loaded");
        return;
    }

    // `mommys-heart-app preview-emails [out-dir]` — render every notification
    // email template with placeholder data to standalone HTML files (default
    // `target/email-preview/`) and exit, without a database, email service, or
    // web server. Open the printed `index.html` in a browser to iterate on the
    // look. See `EMAIL_DRY_RUN` in .env.example for the runtime dry-run flag.
    if std::env::args().nth(1).as_deref() == Some("preview-emails") {
        use std::path::PathBuf;
        let out_dir = std::env::args()
            .nth(2)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target/email-preview"));
        match mommys_heart_app::server::email::preview::write_gallery(&out_dir) {
            Ok(index) => tracing::info!("email preview written — open {}", index.display()),
            Err(e) => panic!("failed to write email preview: {e}"),
        }
        return;
    }

    // Connect to PostgreSQL, run migrations, and seed on first run. Fail fast if
    // the database is unreachable — the app cannot function without it.
    if let Err(e) = mommys_heart_app::server::db::init().await {
        panic!("failed to initialize database: {e}");
    }

    // Configure the SharePoint document library that holds case files. Outside
    // production a missing or unreachable library is not fatal: the store falls
    // back to an on-disk directory so the feature still works locally.
    // Production fails fast, where the real library is required.
    if let Err(e) = mommys_heart_app::server::sharepoint::init().await {
        if mommys_heart_app::server::config::is_production() {
            panic!("failed to initialize case documents: {e}");
        }
        tracing::warn!("case documents unavailable: {e}");
    }

    // Give cases that have no document folder yet one, in the background. Covers
    // cases created while the library was unreachable, and any that predate it.
    mommys_heart_app::server::sharepoint::sync::start_provisioning_backfill();

    // File the case notes whose document is missing or out of date — notes
    // finalized while the library was unreachable, and notes that predate the
    // filing of note records at all.
    mommys_heart_app::server::case_note_records::start_filing_backfill();

    // Kick off document ingestion in the background so the server starts
    // serving immediately; the RAG store fills in once embeddings complete.
    rag::start_background_ingest();

    // Prune audit-log entries older than the retention window, on startup and
    // then every 30 days.
    mommys_heart_app::server::db::audit::start_retention_task();

    // Same for the durable email-delivery-failure log surfaced in the admin
    // dashboard.
    mommys_heart_app::server::db::email_failures::start_retention_task();

    // Initialize the process-local contact-mail task service and close any
    // history row left active by an earlier process stop.
    if let Err(error) = mommys_heart_app::server::contact_mail::initialize().await {
        panic!("failed to initialize contact mail tasks: {error}");
    }

    // Same for spreadsheet imports, which also start the staged-upload purge.
    if let Err(error) = mommys_heart_app::server::crm_import::initialize().await {
        panic!("failed to initialize import tasks: {error}");
    }

    // Prune case-chat notifications that were never read within the retention
    // window (unread ones a user never opened), on startup and every 30 days.
    mommys_heart_app::server::db::channel_notifications::start_retention_task();

    // Mail administrators a daily digest of the activity the audit logs record.
    // The feed itself needs no task: it is a read over those logs.
    mommys_heart_app::server::notifications::start_admin_activity_digest_task();

    let conf = get_configuration(None).unwrap();
    let leptos_options = conf.leptos_options;
    let addr = leptos_options.site_addr;
    let routes = generate_route_list(App);

    let app = Router::new()
        .leptos_routes(&leptos_options, routes, {
            let leptos_options = leptos_options.clone();
            move || shell(leptos_options.clone())
        })
        // The dedicated JSON API (+ CORS for the cross-origin Squarespace widget).
        .merge(api::router::<LeptosOptions>().layer(api::cors_layer()));

    // Wire the case-documents HTTP surface
    let app = server_fns::documents::install(app);
    let app = server_fns::volunteer_documents::install(app);
    let app = api::volunteer_agreement::install(app);
    let app = api::message_transcripts::install(app)
        .fallback(leptos_axum::file_and_error_handler(shell))
        // The security layer is outermost so it covers SSR, RPC, and REST responses.
        .layer(middleware::from_fn(
            mommys_heart_app::server::security::protect,
        ))
        // Log every incoming request (method, path, status, latency).
        .layer(TraceLayer::new_for_http())
        .with_state(leptos_options);

    tracing::info!("listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app.into_make_service())
        .await
        .unwrap();
}

#[cfg(not(feature = "ssr"))]
fn main() {
    // The client-side (wasm) entry point is `lib::hydrate`; this binary target
    // only runs under the `ssr` feature.
}
