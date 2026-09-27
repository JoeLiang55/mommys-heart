//! The resume a volunteer has on file, on their profile.
//!
//! Shown to the owner and to operations admins; only the owner may upload or
//! remove, which matches the server — [`upload_my_resume`] and
//! [`delete_my_resume`] always act on the caller's own record. The file itself
//! is in the document library, so the download is an ordinary link to the
//! server's resume route rather than anything this component fetches.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::server_fns::err_text;
use crate::server_fns::volunteer_documents::{delete_my_resume, my_resume, RESUME_ACCEPT_LABEL};
use crate::server_fns::volunteers::VolunteerResume;

const SECTION_CLASS: &str = "rounded-xl border border-slate-800 bg-slate-900 p-4";

/// Send the chosen file to the server. The pre-check here is only for quick
/// feedback; the server re-validates every byte.
#[cfg(feature = "hydrate")]
async fn upload_resume_file(file_ref: NodeRef<leptos::html::Input>) -> Result<bool, String> {
    use crate::server_fns::volunteer_documents::{upload_my_resume, MAX_RESUME_BYTES};
    use leptos::server_fn::codec::MultipartData;

    let Some(input) = file_ref.get_untracked() else {
        return Ok(false);
    };
    let Some(file) = input.files().and_then(|files| files.get(0)) else {
        return Ok(false);
    };
    if file.size() > MAX_RESUME_BYTES as f64 {
        return Err(format!(
            "File is too large; the limit is {} MB.",
            MAX_RESUME_BYTES / (1024 * 1024)
        ));
    }

    let filename = file.name();
    let form = web_sys::FormData::new().map_err(|_| "Could not prepare the upload.".to_string())?;
    form.append_with_blob_and_filename("file", file.as_ref(), &filename)
        .map_err(|_| "Could not attach the file.".to_string())?;

    upload_my_resume(MultipartData::from(form))
        .await
        .map(|_name| true)
        .map_err(err_text)
}

/// Server-side rendering never uploads; the browser does it after hydration.
#[cfg(not(feature = "hydrate"))]
async fn upload_resume_file(_file_ref: NodeRef<leptos::html::Input>) -> Result<bool, String> {
    Ok(false)
}

#[component]
pub fn VolunteerResumePanel(
    /// The resume currently on file, if any.
    initial_resume: Option<VolunteerResume>,
    /// Whose profile this is, which is what the download link addresses.
    #[prop(into)]
    user_id: String,
    /// Whether the viewer owns this profile, which is what allows uploading.
    is_self: bool,
) -> impl IntoView {
    let resume = RwSignal::new(initial_resume);
    let user_id = StoredValue::new(user_id);
    let file_ref = NodeRef::<leptos::html::Input>::new();
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let notice = RwSignal::new(None::<String>);

    // A fresh read after an upload, so what is shown is what the server
    // recorded — the stored name and stamp, not the locally chosen file.
    let upload = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        notice.set(None);
        spawn_local(async move {
            match upload_resume_file(file_ref).await {
                Ok(true) => {
                    notice.set(Some("Your resume has been uploaded.".to_string()));
                    if let Some(input) = file_ref.get_untracked() {
                        input.set_value("");
                    }
                    reload_resume(resume, error).await;
                }
                Ok(false) => error.set(Some("Choose a file first.".to_string())),
                Err(message) => error.set(Some(message)),
            }
            busy.set(false);
        });
    };

    let remove = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        notice.set(None);
        spawn_local(async move {
            match delete_my_resume().await {
                Ok(()) => {
                    resume.set(None);
                    notice.set(Some("Your resume has been removed.".to_string()));
                }
                Err(e) => error.set(Some(err_text(e))),
            }
            busy.set(false);
        });
    };

    let current = move || {
        let Some(on_file) = resume.get() else {
            return view! {
                <p class="mt-2 text-sm text-slate-500 italic">"No resume on file."</p>
            }
            .into_any();
        };
        let href = format!("/api/volunteers/{}/resume", user_id.get_value());
        let detail = if on_file.uploaded_at.is_empty() {
            on_file.size_label()
        } else {
            format!(
                "{} \u{b7} uploaded {}",
                on_file.size_label(),
                on_file.uploaded_at
            )
        };
        view! {
            <div class="mt-2 flex flex-wrap items-center justify-between gap-2 rounded-lg border border-slate-800 bg-slate-950 px-3 py-2">
                <div class="min-w-0">
                    <p class="truncate text-sm text-slate-200">{on_file.file_name.clone()}</p>
                    <p class="text-xs text-slate-500">{detail}</p>
                </div>
                <div class="flex items-center gap-2">
                    <a
                        href=href
                        class="rounded-lg border border-slate-700 px-3 py-1.5 text-sm font-medium text-slate-200 hover:bg-slate-800"
                    >
                        "Download"
                    </a>
                    <Show when=move || is_self>
                        <button
                            on:click=remove
                            prop:disabled=move || busy.get()
                            class="rounded-lg border border-slate-700 px-3 py-1.5 text-sm font-medium text-rose-300 hover:bg-slate-800 disabled:opacity-60"
                        >
                            "Remove"
                        </button>
                    </Show>
                </div>
            </div>
        }
        .into_any()
    };

    view! {
        <div class=SECTION_CLASS>
            <h3 class="text-sm font-semibold text-slate-200">"Resume"</h3>
            {current}

            <Show when=move || notice.get().is_some()>
                <p class="mt-2 text-sm text-emerald-300">{move || notice.get().unwrap_or_default()}</p>
            </Show>
            <Show when=move || error.get().is_some()>
                <p class="mt-2 text-sm text-rose-300">{move || error.get().unwrap_or_default()}</p>
            </Show>

            <Show when=move || is_self>
                <div class="mt-4 space-y-2 border-t border-slate-800 pt-4">
                    <div class="flex flex-col gap-2 sm:flex-row">
                        <input
                            node_ref=file_ref
                            type="file"
                            accept=".pdf,.doc,.docx"
                            class="block w-full text-sm text-slate-300 file:mr-3 file:rounded-lg file:border-0 file:bg-slate-800 file:px-3 file:py-2 file:text-sm file:font-medium file:text-slate-200 hover:file:bg-slate-700"
                        />
                        <button
                            on:click=upload
                            prop:disabled=move || busy.get()
                            class="shrink-0 rounded-lg bg-primary-500 px-3 py-2 text-sm font-semibold text-white hover:bg-primary-600 disabled:opacity-60"
                        >
                            {move || {
                                if busy.get() {
                                    "Uploading\u{2026}".to_string()
                                } else if resume.get().is_some() {
                                    "Replace".to_string()
                                } else {
                                    "Upload".to_string()
                                }
                            }}
                        </button>
                    </div>
                    <p class="text-xs text-slate-500">{RESUME_ACCEPT_LABEL}" up to 10 MB."</p>
                </div>
            </Show>
        </div>
    }
}

/// Re-read the record after an upload, so the panel shows the stored name, size
/// and stamp the server actually recorded rather than what was chosen locally.
async fn reload_resume(resume: RwSignal<Option<VolunteerResume>>, error: RwSignal<Option<String>>) {
    match my_resume().await {
        Ok(updated) => resume.set(updated),
        Err(e) => error.set(Some(err_text(e))),
    }
}
