//! Volunteers as an object built on top of a user: the agreement they accepted,
//! the details they gave with it, their application to become one, and the admin
//! decision on it.
//!
//! One record per person, doubling as their application, so a decision updates it
//! in place and a declined applicant may accept again to re-apply.

use leptos::prelude::*;
use serde::{Deserialize, Serialize};

use crate::helpers::volunteer_details::{VolunteerDetails, VolunteerDetailsView};

/// Where a volunteer application stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolunteerStatus {
    Pending,
    Approved,
    Denied,
    /// Was approved, then lost the volunteer role. The accepted agreement is
    /// kept rather than deleted because it is a real historical fact; a revoked
    /// person may accept again and re-apply, exactly like a declined one.
    Revoked,
}

impl VolunteerStatus {
    pub fn slug(self) -> &'static str {
        match self {
            VolunteerStatus::Pending => "pending",
            VolunteerStatus::Approved => "approved",
            VolunteerStatus::Denied => "denied",
            VolunteerStatus::Revoked => "revoked",
        }
    }

    pub fn from_slug(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(VolunteerStatus::Pending),
            "approved" => Some(VolunteerStatus::Approved),
            "denied" => Some(VolunteerStatus::Denied),
            "revoked" => Some(VolunteerStatus::Revoked),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            VolunteerStatus::Pending => "Pending review",
            VolunteerStatus::Approved => "Approved",
            VolunteerStatus::Denied => "Declined",
            VolunteerStatus::Revoked => "Revoked",
        }
    }

    pub fn badge_classes(self) -> &'static str {
        match self {
            VolunteerStatus::Pending => "bg-amber-500/15 text-amber-300 ring-1 ring-amber-500/30",
            VolunteerStatus::Approved => {
                "bg-emerald-500/15 text-emerald-300 ring-1 ring-emerald-500/30"
            }
            VolunteerStatus::Denied | VolunteerStatus::Revoked => {
                "bg-rose-500/15 text-rose-300 ring-1 ring-rose-500/30"
            }
        }
    }
}

/// The resume a volunteer has on file, as the profile shows it. The bytes live
/// in the document library; this is only what is needed to describe and fetch
/// the file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VolunteerResume {
    pub file_name: String,
    pub size_bytes: i64,
    /// Pre-formatted for display; only ever shown, never compared.
    pub uploaded_at: String,
}

impl VolunteerResume {
    /// The size as a person would write it, e.g. `1.4 MB`.
    pub fn size_label(&self) -> String {
        let bytes = self.size_bytes.max(0) as f64;
        if bytes >= 1024.0 * 1024.0 {
            format!("{:.1} MB", bytes / (1024.0 * 1024.0))
        } else {
            format!("{:.0} KB", (bytes / 1024.0).ceil())
        }
    }
}

/// One person's volunteer record: the agreement they accepted, the details they
/// gave with it, and the state of their application.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VolunteerApplication {
    pub status: VolunteerStatus,
    /// The agreement wording they accepted. Empty for volunteers who predate the
    /// agreement (backfilled by migration), which is why it is not an `Option`:
    /// "no version" is a real, meaningful state rather than missing data.
    pub agreement_version: String,
    /// What they told us about themselves. Carries `has_ssn`, never the number.
    #[serde(default)]
    pub details: VolunteerDetailsView,
    /// Pre-formatted for display; these are only ever shown, never compared.
    pub agreed_at: String,
    pub decided_by_name: String,
    pub decision_note: String,
    pub decided_at: String,
    /// Their resume, when one has been uploaded. Only approved volunteers can
    /// have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<VolunteerResume>,
}

impl VolunteerApplication {
    /// Whether this record carries the wording of a specific agreement version.
    /// False for the migration backfill, whose volunteers never saw one.
    pub fn has_agreement(&self) -> bool {
        !self.agreement_version.is_empty()
    }

    /// Whether they accepted the wording currently in force. False for someone
    /// who never signed and for one on a superseded version, who must re-accept.
    pub fn is_current_agreement(&self) -> bool {
        crate::helpers::volunteer_terms::is_current(&self.agreement_version)
    }
}

/// One row of the admin's pending-application queue: who applied, when, and what
/// they say they can do.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Volunteer {
    pub id: String,
    pub first_name: String,
    pub last_name: String,
    pub email: String,
    #[serde(default)]
    pub skills_focus: String,
    /// Pre-formatted for display; only ever shown, never compared.
    pub agreed_at: String,
}

impl Volunteer {
    pub fn full_name(&self) -> String {
        format!("{} {}", self.first_name, self.last_name)
            .trim()
            .to_string()
    }
}

/// Accept the volunteer agreement and submit the details that go with it.
///
/// Volunteers only: this is how an existing volunteer records their acceptance
/// of a new version of the wording. It is deliberately *not* a way to become
/// one — that is the public signup at
/// [`crate::server_fns::volunteer_applicants::apply_as_volunteer`], which an
/// admin approves. Clients and administrators are refused.
#[server(prefix = "/api")]
pub async fn apply_to_volunteer(
    agreement_version: String,
    details: VolunteerDetails,
) -> Result<(), ServerFnError> {
    use crate::server::db::volunteers;
    use crate::server::permissions::require_user;
    use crate::server_fns::users::AccountRole;

    let user = require_user().await?;
    if user.role != AccountRole::Volunteer {
        return Err(ServerFnError::new(
            "Only volunteers can accept the volunteer agreement here.",
        ));
    }
    // An old tab holding a stale version never saw the wording it claims to
    // accept, so make it re-read rather than record consent it cannot evidence.
    if !crate::helpers::volunteer_terms::is_current(agreement_version.trim()) {
        return Err(ServerFnError::new(
            "Please read and accept the current volunteer agreement.",
        ));
    }
    // Normalize first so what is validated is exactly what gets stored. Signing
    // requires the SSN outright, plus the consent and typed signature.
    let details = details.normalized();
    details.validate_with(false).map_err(ServerFnError::new)?;
    details.validate_signature().map_err(ServerFnError::new)?;

    let existing = volunteers::get(&user.id)
        .await
        .map_err(ServerFnError::new)?;
    if existing
        .as_ref()
        .is_some_and(|application| application.status == VolunteerStatus::Pending)
    {
        return Err(ServerFnError::new(
            "Your volunteer application is already awaiting review.",
        ));
    }
    // Only the current wording counts as already signed, so a volunteer on a
    // superseded version is not turned away.
    if existing
        .as_ref()
        .is_some_and(VolunteerApplication::is_current_agreement)
    {
        return Err(ServerFnError::new(
            "You have already accepted the current volunteer agreement.",
        ));
    }
    // Always `true`: the caller is a volunteer already, so this records an
    // acceptance rather than filing anything for review.
    volunteers::apply(
        &user.id,
        agreement_version.trim(),
        &details,
        true,
        &user.full_name(),
    )
    .await
    .map_err(ServerFnError::new)?;
    Ok(())
}

/// Update the caller's own volunteer details, always scoped to the signed-in
/// user. A blank `ssn` keeps the number on file.
#[server(prefix = "/api")]
pub async fn save_my_volunteer_details(
    details: VolunteerDetails,
) -> Result<VolunteerDetailsView, ServerFnError> {
    use crate::server::db::volunteers;
    use crate::server::permissions::require_user;

    let user = require_user().await?;
    let details = details.normalized();

    let existing = volunteers::get(&user.id)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new("You do not have a volunteer record to edit."))?;

    // The browser is never sent the stored number, so a blank box means "keep it".
    details
        .validate_with(existing.details.has_ssn)
        .map_err(ServerFnError::new)?;

    volunteers::save_details(&user.id, &details, &user.full_name())
        .await
        .map_err(ServerFnError::new)?;

    Ok(VolunteerDetailsView {
        skills_focus: details.skills_focus,
        volunteer_role: details.volunteer_role,
        date_of_birth: details.date_of_birth,
        has_ssn: !details.ssn.is_empty() || existing.details.has_ssn,
        phone: details.phone,
        home_address: details.home_address,
        emergency_first_name: details.emergency_first_name,
        emergency_last_name: details.emergency_last_name,
        emergency_relationship: details.emergency_relationship,
        emergency_phone: details.emergency_phone,
        // The signature block is not editable here; it stays as signed.
        ..existing.details
    })
}

/// Reveal one volunteer's Social Security Number, formatted `000-00-0000`. Site
/// admins only, and the disclosure is audited before the number is returned.
#[server(prefix = "/api")]
pub async fn reveal_volunteer_ssn(user_id: String) -> Result<String, ServerFnError> {
    use crate::server::db::volunteers;
    use crate::server::permissions::{require_site_admin, require_user};

    let actor = require_user().await?;
    require_site_admin(&actor)?;
    let user_id = user_id.trim().to_string();
    if user_id.is_empty() {
        return Err(ServerFnError::new("No volunteer was chosen."));
    }
    let ssn = volunteers::reveal_ssn(&user_id, &actor.full_name())
        .await
        .map_err(ServerFnError::new)?;
    if ssn.is_empty() {
        return Err(ServerFnError::new(
            "This volunteer has no Social Security Number on file.",
        ));
    }
    Ok(crate::helpers::volunteer_details::format_ssn(&ssn))
}

/// Every volunteer application waiting on a decision, oldest first. Operations
/// admins may inspect the queue; only site admins may decide an application.
#[server(prefix = "/api")]
pub async fn list_pending_volunteer_applications() -> Result<Vec<Volunteer>, ServerFnError> {
    use crate::server::db::volunteers;
    use crate::server::permissions::{require_operations_admin, require_user};

    let actor = require_user().await?;
    require_operations_admin(&actor)?;
    volunteers::list_pending().await.map_err(ServerFnError::new)
}

/// Approve or decline a volunteer application. Site admins only, because
/// approving grants a role. Either outcome emails the applicant; a decline has
/// no other visible effect and they may apply again.
///
/// On approval the admin may also set the volunteer's official email address as
/// their primary (sign-in) address. It must be typed twice, and both copies are
/// sent so the confirmation is checked on the server too. Empty leaves the
/// address alone.
#[server(prefix = "/api")]
pub async fn decide_volunteer_application(
    user_id: String,
    approve: bool,
    note: String,
    new_email: String,
    confirm_new_email: String,
) -> Result<(), ServerFnError> {
    use crate::server::db::volunteers;
    use crate::server::permissions::{require_site_admin, require_user};
    use crate::server_fns::auth::validate_confirmed_email;

    let actor = require_user().await?;
    require_site_admin(&actor)?;
    let user_id = user_id.trim().to_string();
    if user_id.is_empty() {
        return Err(ServerFnError::new("No application was chosen."));
    }
    if note.chars().count() > 1_000 {
        return Err(ServerFnError::new(
            "Notes must be 1,000 characters or fewer.",
        ));
    }
    let note = note.trim().to_string();
    // Only an approval may carry an address change, and only a non-empty one
    // asks for it at all.
    let new_email = if approve && !new_email.trim().is_empty() {
        Some(validate_confirmed_email(&new_email, &confirm_new_email)?)
    } else {
        None
    };
    let previous_email = volunteers::decide(
        &user_id,
        approve,
        &actor.id,
        &actor.full_name(),
        &note,
        new_email.as_deref(),
    )
    .await
    .map_err(|error| ServerFnError::new(error.to_string()))?;
    crate::server::notifications::notify_volunteer_decision(user_id, approve, note, previous_email);
    Ok(())
}
