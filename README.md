# Mommy's Heart management website

A Rust + Leptos (Axum SSR + WASM hydrate) application for managing users,
cases, evidence, messages, and audit logs. Data is persisted in **PostgreSQL**
via SQLx, with server-side authentication (argon2 password hashing + session
cookies).

## Run

```bash
cargo leptos build     # compile (slow the first time)
cargo leptos watch     # run it, rebuilding as you edit
```

`cargo leptos serve` runs without watching. Leave `DATABASE_URL` unset and the
app uses `postgres://postgres:postgres@localhost:5432/postgres`; a fresh
database seeds itself from the mock fixtures on first boot, so the demo logins
work straight away. To reset it to those fixtures, run
`cargo run --no-default-features --features ssr -- seed` — which wipes data
anyone else is using.

### Testing case document links

Without SharePoint configuration, documents use the on-disk store under
`target/sharepoint/`. Folder navigation, uploads, downloads, and permissions
work locally, but this store returns `file://` paths rather than SharePoint
URLs. The Documents panel shows **Open in SharePoint** disabled alongside a
local-storage explanation. A working link requires a real SharePoint URL;
the local store cannot provide one.

To verify SharePoint links end to end, use a development SharePoint library
with the Graph settings described in `.env.example` and `SHAREPOINT_BACKEND=graph`.
Open a case's Documents section, navigate into each permitted folder and a
nested folder, and check that **Open in SharePoint** opens that exact folder.
The link uses the selected folder's URL returned by Graph; it never substitutes
the case root when a folder URL is missing. Also verify that a client account
cannot list or navigate into restricted folders. Use development records and
credentials for this check.

## View email templates

```
cargo run --features ssr -- preview-emails            # writes target/email-preview/
cargo run --features ssr -- preview-emails <out-dir>  # custom output directory
```

## License

MIT — see [LICENSE](LICENSE). The guides under `docs/` are Foundation content,
not software, and are not covered by it.
