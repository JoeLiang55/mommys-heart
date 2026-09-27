// Deep `view!` trees monomorphise into very large types; without
// `--cfg erase_components` the default limit overflows during layout.
#![recursion_limit = "512"]

pub mod app;
pub mod helpers;
pub mod mockdata;
pub mod server_fns;
pub mod state;

pub mod components {
    pub mod admin_activity;
    pub mod admin_case_access;
    pub mod admin_case_requests;
    pub mod admin_cases;
    pub mod admin_manage_cases;
    pub mod admin_manage_users;
    pub mod admin_request_center;
    pub mod admin_requests;
    pub mod admin_user_card;
    pub mod admin_volunteers;
    pub mod case_contacts;
    pub mod case_questionnaires;
    pub mod category_picker;
    pub mod change_log;
    pub mod chart;
    pub mod contact_cases;
    pub mod contact_form;
    pub mod contact_properties;
    pub mod email_failures;
    pub mod guard;
    pub mod layout;
    pub mod loading;
    pub mod organization_properties;
    pub mod profile_link;
    pub mod property_filters;
    pub mod property_rows;
    pub mod volunteer_details;
    pub mod volunteer_hours;
    pub mod volunteer_resume;
}

pub mod pages {
    pub mod admin;
    pub mod bulk_properties;
    pub mod case_intake;
    pub mod case_notes;
    pub mod case_signup;
    pub mod cases;
    pub mod contact_categories;
    pub mod contact_mail;
    pub mod contacts;
    pub mod crm_import;
    pub mod forgot_password;
    pub mod funding;
    pub mod inbox;
    pub mod login;
    pub mod mfa;
    pub mod organizations;
    pub mod people;
    pub mod profile;
    pub mod reports;
    pub mod reset_password;
    pub mod settings;
    pub mod verify_email;
    pub mod volunteer_agreement;
    pub mod volunteer_setup;
}

#[cfg(feature = "ssr")]
pub mod server;

#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    use crate::app::App;
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_body(App);
}
