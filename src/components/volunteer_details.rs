//! The volunteer's own details on their profile: skills, date of birth,
//! contact, emergency contact, and whether an SSN is on file.
//!
//! Shown to the owner and to operations admins; the owner may edit in place.
//! The panel is never sent the SSN, only `has_ssn`; a site admin's Reveal
//! button asks for it explicitly and that request is audited.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::helpers::volunteer_details::{
    format_dob, VolunteerDetails, VolunteerDetailsView, BACKGROUND_CHECK_CONSENT,
};
use crate::server_fns::err_text;
use crate::server_fns::volunteers::{reveal_volunteer_ssn, save_my_volunteer_details};

const INPUT_CLASS: &str = "w-full rounded-lg border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-100 placeholder:text-slate-500 focus:border-primary-500 focus:outline-none focus:ring-2 focus:ring-primary-500/40";
const LABEL_CLASS: &str = "text-xs font-medium text-slate-400";
const SECTION_CLASS: &str = "rounded-xl border border-slate-800 bg-slate-900 p-4";

/// One read-only "label + value" row, with a muted placeholder when the field
/// has not been filled in.
#[component]
fn DetailRow(label: &'static str, #[prop(into)] value: String) -> impl IntoView {
    let empty = value.trim().is_empty();
    let text = if empty {
        "Not provided".to_string()
    } else {
        value
    };
    view! {
        <div class="flex flex-col gap-0.5 border-b border-slate-800 py-2 last:border-b-0 sm:flex-row sm:items-baseline sm:justify-between sm:gap-4">
            <span class=LABEL_CLASS>{label}</span>
            <span class=move || {
                if empty { "text-sm text-slate-500 italic" } else { "text-sm text-slate-200" }
            }>{text}</span>
        </div>
    }
}

#[component]
pub fn VolunteerDetailsPanel(
    initial_details: VolunteerDetailsView,
    /// Whose profile this is. Needed to ask for their SSN.
    #[prop(into)]
    user_id: String,
    /// Whether the viewer owns this profile, which is what allows editing.
    is_self: bool,
    /// Only site admins may reveal a Social Security Number.
    is_site_admin: bool,
) -> impl IntoView {
    let details = RwSignal::new(initial_details);
    let user_id = StoredValue::new(user_id);
    let editing = RwSignal::new(false);
    let draft = RwSignal::new(VolunteerDetails::default());
    let save_error = RwSignal::new(None::<String>);
    let saving = RwSignal::new(false);
    let saved = RwSignal::new(false);

    // Held only as long as this page view.
    let revealed_ssn = RwSignal::new(None::<String>);
    let revealing = RwSignal::new(false);
    let reveal_error = RwSignal::new(None::<String>);

    let begin_edit = move |_| {
        draft.set(details.get_untracked().to_edit());
        save_error.set(None);
        saved.set(false);
        editing.set(true);
    };

    let cancel_edit = move |_| {
        save_error.set(None);
        editing.set(false);
    };

    let save = move |_| {
        if saving.get_untracked() {
            return;
        }
        let edit = draft.get_untracked();
        // Checked here too so the message appears without a round trip; both
        // sides run the same validator. A blank SSN keeps the one on file.
        if let Err(message) = edit
            .normalized()
            .validate_with(details.get_untracked().has_ssn)
        {
            save_error.set(Some(message));
            return;
        }
        saving.set(true);
        save_error.set(None);
        spawn_local(async move {
            match save_my_volunteer_details(edit).await {
                Ok(updated) => {
                    details.set(updated);
                    // A changed number invalidates anything already revealed.
                    revealed_ssn.set(None);
                    editing.set(false);
                    saved.set(true);
                }
                Err(error) => save_error.set(Some(err_text(error))),
            }
            saving.set(false);
        });
    };

    let reveal = move |_| {
        if revealing.get_untracked() {
            return;
        }
        revealing.set(true);
        reveal_error.set(None);
        let target = user_id.get_value();
        spawn_local(async move {
            match reveal_volunteer_ssn(target).await {
                Ok(ssn) => revealed_ssn.set(Some(ssn)),
                Err(error) => reveal_error.set(Some(err_text(error))),
            }
            revealing.set(false);
        });
    };

    // The SSN row: presence only, until a site admin explicitly asks.
    let ssn_row = move || {
        let has_ssn = details.get().has_ssn;
        let revealed = revealed_ssn.get();
        let value = match (&revealed, has_ssn) {
            (Some(ssn), _) => ssn.clone(),
            (None, true) => "On file".to_string(),
            (None, false) => "Not provided".to_string(),
        };
        let show_button = is_site_admin && has_ssn && revealed.is_none();
        view! {
            <div class="border-b border-slate-800 py-2 last:border-b-0">
                <div class="flex flex-col gap-0.5 sm:flex-row sm:items-baseline sm:justify-between sm:gap-4">
                    <span class=LABEL_CLASS>"Social Security Number"</span>
                    <span class="flex items-center gap-2">
                        <span class=move || {
                            if has_ssn { "text-sm text-slate-200" } else { "text-sm text-slate-500 italic" }
                        }>{value}</span>
                        <Show when=move || show_button>
                            <button
                                on:click=reveal
                                prop:disabled=move || revealing.get()
                                class="rounded-lg border border-slate-700 px-2 py-1 text-xs font-medium text-slate-300 hover:bg-slate-800 disabled:opacity-50"
                            >
                                {move || if revealing.get() { "Revealing\u{2026}" } else { "Reveal" }}
                            </button>
                        </Show>
                    </span>
                </div>
                <Show when=move || revealed_ssn.get().is_some()>
                    <p class="mt-1 text-xs text-amber-300">
                        "This disclosure has been recorded in the change log."
                    </p>
                </Show>
                <Show when=move || reveal_error.get().is_some()>
                    <p class="mt-1 text-xs text-rose-300">
                        {move || reveal_error.get().unwrap_or_default()}
                    </p>
                </Show>
            </div>
        }
    };

    let read_only = move || {
        let d = details.get();
        let signed_by_guardian = d.signer_is_guardian;
        view! {
            <div class="mt-2">
                <DetailRow label="Skills and area of focus" value=d.skills_focus.clone() />
                <DetailRow label="Volunteer role" value=d.volunteer_role.clone() />
                <DetailRow label="Date of birth" value=format_dob(&d.date_of_birth) />
                {ssn_row()}
                <DetailRow label="Phone" value=d.phone.clone() />
                <DetailRow label="Home address" value=d.home_address.clone() />
                <DetailRow label="Emergency contact" value=d.emergency_full_name() />
                <DetailRow
                    label="Relationship to volunteer"
                    value=d.emergency_relationship.clone()
                />
                <DetailRow label="Emergency contact phone" value=d.emergency_phone.clone() />
                <DetailRow label="Full legal name" value=d.legal_name.clone() />
                <DetailRow label="Electronic signature" value=d.signature_name.clone() />
                <Show when=move || signed_by_guardian>
                    <DetailRow label="Signed by guardian" value=d.guardian_name.clone() />
                    <DetailRow
                        label="Guardian's relationship"
                        value=d.guardian_relationship.clone()
                    />
                    <DetailRow label="Guardian's email" value=d.guardian_email.clone() />
                </Show>
                <DetailRow label="Signed on" value=d.signed_at.clone() />
            </div>
        }
        .into_any()
    };

    let edit_form = move || {
        let has_ssn = details.get().has_ssn;
        view! {
            <div class="mt-4 space-y-4">
                <div>
                    <label class=LABEL_CLASS>"Skills and area of focus"</label>
                    <textarea
                        class=INPUT_CLASS
                        rows="3"
                        prop:value=move || draft.get().skills_focus
                        on:input=move |event| {
                            let value = event_target_value(&event);
                            draft.update(|d| d.skills_focus = value);
                        }
                    />
                </div>

                <div>
                    <label class=LABEL_CLASS>"Volunteer role " <span class="text-rose-400" aria-hidden="true">"*"</span></label>
                    <input
                        class=INPUT_CLASS
                        prop:value=move || draft.get().volunteer_role
                        on:input=move |event| {
                            let value = event_target_value(&event);
                            draft.update(|d| d.volunteer_role = value);
                        }
                    />
                </div>

                <p class="text-xs text-slate-500">{BACKGROUND_CHECK_CONSENT}</p>

                <div class="grid gap-4 sm:grid-cols-2">
                    <div>
                        <label class=LABEL_CLASS>"Date of birth"</label>
                        <input
                            class=INPUT_CLASS
                            type="date"
                            prop:value=move || draft.get().date_of_birth
                            on:input=move |event| {
                                let value = event_target_value(&event);
                                draft.update(|d| d.date_of_birth = value);
                            }
                        />
                    </div>
                    <div>
                        <label class=LABEL_CLASS>"Phone"</label>
                        <input
                            class=INPUT_CLASS
                            placeholder="(555) 123-4567"
                            prop:value=move || draft.get().phone
                            on:input=move |event| {
                                let value = event_target_value(&event);
                                draft.update(|d| d.phone = value);
                            }
                        />
                    </div>
                </div>

                <div>
                    <label class=LABEL_CLASS>"Home address"</label>
                    <input
                        class=INPUT_CLASS
                        autocomplete="street-address"
                        placeholder="Street, city, state, ZIP"
                        prop:value=move || draft.get().home_address
                        on:input=move |event| {
                            let value = event_target_value(&event);
                            draft.update(|d| d.home_address = value);
                        }
                    />
                </div>

                <div>
                    <label class=LABEL_CLASS>
                        {move || {
                            if has_ssn {
                                "Social Security Number (leave blank to keep the number on file)"
                            } else {
                                "Social Security Number (required)"
                            }
                        }}
                    </label>
                    <input
                        class=INPUT_CLASS
                        placeholder=move || {
                            if has_ssn {
                                "Leave blank to keep the number on file"
                            } else {
                                "000-00-0000"
                            }
                        }
                        prop:value=move || draft.get().ssn
                        on:input=move |event| {
                            let value = event_target_value(&event);
                            draft.update(|d| d.ssn = value);
                        }
                    />
                </div>

                <div class="grid gap-4 sm:grid-cols-2">
                    <div>
                        <label class=LABEL_CLASS>"Emergency contact first name"</label>
                        <input
                            class=INPUT_CLASS
                            prop:value=move || draft.get().emergency_first_name
                            on:input=move |event| {
                                let value = event_target_value(&event);
                                draft.update(|d| d.emergency_first_name = value);
                            }
                        />
                    </div>
                    <div>
                        <label class=LABEL_CLASS>"Emergency contact last name"</label>
                        <input
                            class=INPUT_CLASS
                            prop:value=move || draft.get().emergency_last_name
                            on:input=move |event| {
                                let value = event_target_value(&event);
                                draft.update(|d| d.emergency_last_name = value);
                            }
                        />
                    </div>
                    <div>
                        <label class=LABEL_CLASS>"Relationship to volunteer"</label>
                        <input
                            class=INPUT_CLASS
                            prop:value=move || draft.get().emergency_relationship
                            on:input=move |event| {
                                let value = event_target_value(&event);
                                draft.update(|d| d.emergency_relationship = value);
                            }
                        />
                    </div>
                    <div>
                        <label class=LABEL_CLASS>"Emergency contact phone"</label>
                        <input
                            class=INPUT_CLASS
                            placeholder="(555) 123-4567"
                            prop:value=move || draft.get().emergency_phone
                            on:input=move |event| {
                                let value = event_target_value(&event);
                                draft.update(|d| d.emergency_phone = value);
                            }
                        />
                    </div>
                </div>

                <Show when=move || save_error.get().is_some()>
                    <p class="rounded-lg border border-rose-500/40 bg-rose-500/10 px-3 py-2 text-sm text-rose-300">
                        {move || save_error.get().unwrap_or_default()}
                    </p>
                </Show>

                <p class="text-xs text-slate-500">
                    "Your phone number here is also used as your account phone number."
                </p>
            </div>
        }
        .into_any()
    };

    view! {
        <div class=SECTION_CLASS>
            <div class="flex flex-wrap items-center justify-between gap-2">
                <h3 class="text-sm font-semibold text-slate-200">"Volunteer information"</h3>
                <Show when=move || is_self && !editing.get()>
                    <button
                        on:click=begin_edit
                        class="shrink-0 rounded-lg border border-slate-700 px-3 py-1.5 text-sm font-medium text-slate-200 hover:bg-slate-800"
                    >
                        "Edit"
                    </button>
                </Show>
                <Show when=move || editing.get()>
                    <div class="flex items-center gap-2">
                        <button
                            on:click=save
                            prop:disabled=move || saving.get()
                            class="shrink-0 rounded-lg bg-primary-500 px-3 py-1.5 text-sm font-semibold text-white hover:bg-primary-600 disabled:opacity-60"
                        >
                            {move || if saving.get() { "Saving\u{2026}" } else { "Save" }}
                        </button>
                        <button
                            on:click=cancel_edit
                            class="shrink-0 rounded-lg border border-slate-700 px-3 py-1.5 text-sm font-medium text-slate-300 hover:bg-slate-800"
                        >
                            "Cancel"
                        </button>
                    </div>
                </Show>
            </div>

            <Show when=move || saved.get()>
                <p class="mt-2 text-sm text-emerald-300">"Your volunteer information has been saved."</p>
            </Show>

            {move || if editing.get() { edit_form() } else { read_only() }}
        </div>
    }
}
