//! Persistence for the volunteer record built on top of a user (SSR only): the
//! agreement they accepted, the details given with it, and the decision on
//! their application. The role is set by [`crate::server::db::users::set_role_in`].
//!
//! [`SELECT_COLUMNS`] omits the `ssn` column and reports only `ssn <> ''`, so no
//! ordinary read can leak it; [`reveal_ssn`] audits the one exception.

use std::fmt;

use crate::helpers::volunteer_details::{VolunteerDetails, VolunteerDetailsView};
use crate::server::db::{audit, contacts, pool, users};
use crate::server_fns::users::AccountRole;
use crate::server_fns::volunteers::{
    Volunteer, VolunteerApplication, VolunteerResume, VolunteerStatus,
};

/// How timestamps are rendered for display. These are shown, never compared.
const STAMP: &str = "%Y-%m-%d %H:%M";

/// What can go wrong deciding an application. The `Display` text is what the
/// admin reads, so a lost race explains itself instead of leaking a db error.
#[derive(Debug)]
pub enum Error {
    Database(sqlx::Error),
    NotFound,
    AlreadyDecided,
    EmailTaken,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "{error}"),
            Self::NotFound => write!(formatter, "No volunteer application was found."),
            Self::AlreadyDecided => {
                write!(formatter, "This application has already been decided.")
            }
            Self::EmailTaken => {
                write!(
                    formatter,
                    "Another account already uses that email address."
                )
            }
        }
    }
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        // A race on the case-insensitive email index reads as a plain message.
        if let sqlx::Error::Database(database) = &error {
            if database.constraint().is_some_and(|constraint| {
                constraint == "users_email_lower_idx" || constraint == "users_email_key"
            }) {
                return Self::EmailTaken;
            }
        }
        Self::Database(error)
    }
}

impl From<users::EmailChangeError> for Error {
    fn from(error: users::EmailChangeError) -> Self {
        match error {
            users::EmailChangeError::Database(error) => Self::Database(error),
            users::EmailChangeError::NotFound => Self::NotFound,
            users::EmailChangeError::Taken => Self::EmailTaken,
        }
    }
}

#[derive(sqlx::FromRow)]
struct VolunteerRow {
    status: String,
    agreement_version: String,
    agreed_at: chrono::DateTime<chrono::Utc>,
    decided_by_name: String,
    decision_note: String,
    decided_at: Option<chrono::DateTime<chrono::Utc>>,
    skills_focus: String,
    volunteer_role: String,
    date_of_birth: Option<String>,
    /// Whether a number is on file. Never the number itself.
    has_ssn: bool,
    phone: String,
    home_address: String,
    emergency_first_name: String,
    emergency_last_name: String,
    emergency_relationship: String,
    emergency_phone: String,
    legal_name: String,
    signature_name: String,
    signer_is_guardian: bool,
    guardian_name: String,
    guardian_relationship: String,
    guardian_email: String,
    signed_at: Option<chrono::DateTime<chrono::Utc>>,
    resume_file_name: String,
    resume_size_bytes: i64,
    resume_uploaded_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn stamp(at: chrono::DateTime<chrono::Utc>) -> String {
    at.with_timezone(&chrono::Local).format(STAMP).to_string()
}

impl From<VolunteerRow> for VolunteerApplication {
    fn from(row: VolunteerRow) -> Self {
        // A recorded name is what says a resume is on file; the bytes are in the
        // document library, not here.
        let resume = (!row.resume_file_name.is_empty()).then(|| VolunteerResume {
            file_name: row.resume_file_name.clone(),
            size_bytes: row.resume_size_bytes,
            uploaded_at: row.resume_uploaded_at.map(stamp).unwrap_or_default(),
        });
        Self {
            // An unparseable status would mean the CHECK constraint was bypassed;
            // treat it as pending so it surfaces in the admin queue for a human.
            status: VolunteerStatus::from_slug(&row.status).unwrap_or(VolunteerStatus::Pending),
            agreement_version: row.agreement_version,
            details: VolunteerDetailsView {
                skills_focus: row.skills_focus,
                volunteer_role: row.volunteer_role,
                date_of_birth: row.date_of_birth.unwrap_or_default(),
                has_ssn: row.has_ssn,
                phone: row.phone,
                home_address: row.home_address,
                emergency_first_name: row.emergency_first_name,
                emergency_last_name: row.emergency_last_name,
                emergency_relationship: row.emergency_relationship,
                emergency_phone: row.emergency_phone,
                legal_name: row.legal_name,
                signature_name: row.signature_name,
                signer_is_guardian: row.signer_is_guardian,
                guardian_name: row.guardian_name,
                guardian_relationship: row.guardian_relationship,
                guardian_email: row.guardian_email,
                signed_at: row.signed_at.map(stamp).unwrap_or_default(),
            },
            agreed_at: stamp(row.agreed_at),
            decided_by_name: row.decided_by_name,
            decision_note: row.decision_note,
            decided_at: row.decided_at.map(stamp).unwrap_or_default(),
            resume,
        }
    }
}

/// The columns every ordinary read selects. `ssn` is absent by design.
const SELECT_COLUMNS: &str =
    "status, agreement_version, agreed_at, decided_by_name, decision_note, decided_at,
     skills_focus, volunteer_role, to_char(date_of_birth, 'YYYY-MM-DD') AS date_of_birth,
     (ssn <> '') AS has_ssn, phone, home_address, emergency_first_name, emergency_last_name,
     emergency_relationship, emergency_phone, legal_name, signature_name,
     signer_is_guardian, guardian_name, guardian_relationship, guardian_email, signed_at,
     resume_file_name, resume_size_bytes, resume_uploaded_at";

/// One person's volunteer record, or `None` if they have never applied.
pub async fn get(user_id: &str) -> Result<Option<VolunteerApplication>, sqlx::Error> {
    let row = sqlx::query_as::<_, VolunteerRow>(&format!(
        "SELECT {SELECT_COLUMNS} FROM volunteers WHERE user_id = $1"
    ))
    .bind(user_id)
    .fetch_optional(pool())
    .await?;
    Ok(row.map(Into::into))
}

/// Record an acceptance of the volunteer agreement and the details submitted
/// with it. `already_a_volunteer` files `approved` rather than `pending`, and
/// the phone is written through to `users.phone` in the same transaction.
pub async fn apply(
    user_id: &str,
    agreement_version: &str,
    details: &VolunteerDetails,
    already_a_volunteer: bool,
    actor: &str,
) -> Result<(), sqlx::Error> {
    let status = if already_a_volunteer {
        VolunteerStatus::Approved.slug()
    } else {
        VolunteerStatus::Pending.slug()
    };

    let mut tx = pool().begin().await?;

    // An existing volunteer keeps their approval; a fresh application clears any
    // previous decision so a re-applicant starts from a clean pending state.
    sqlx::query(
        "INSERT INTO volunteers (
             user_id, status, agreement_version, skills_focus, date_of_birth, ssn,
             phone, emergency_first_name, emergency_last_name, emergency_relationship,
             emergency_phone, volunteer_role, legal_name, signature_name,
             signer_is_guardian, guardian_name, guardian_relationship, guardian_email,
             electronic_consent, signed_at, home_address
         )
         VALUES ($1, $2, $3, $4, NULLIF($5, '')::date, $6, $7, $8, $9, $10, $11,
                 $12, $13, $14, $15, $16, $17, $18, $19, now(), $20)
         ON CONFLICT (user_id) DO UPDATE SET
             status = EXCLUDED.status,
             agreement_version = EXCLUDED.agreement_version,
             agreed_at = now(),
             skills_focus = EXCLUDED.skills_focus,
             date_of_birth = EXCLUDED.date_of_birth,
             -- A blank submission keeps whatever is already on file, matching
             -- the edit form: the browser is never sent the number, so it
             -- cannot echo it back and an empty box must not erase it.
             ssn = CASE WHEN EXCLUDED.ssn = '' THEN volunteers.ssn ELSE EXCLUDED.ssn END,
             phone = EXCLUDED.phone,
             home_address = EXCLUDED.home_address,
             emergency_first_name = EXCLUDED.emergency_first_name,
             emergency_last_name = EXCLUDED.emergency_last_name,
             emergency_relationship = EXCLUDED.emergency_relationship,
             emergency_phone = EXCLUDED.emergency_phone,
             volunteer_role = EXCLUDED.volunteer_role,
             legal_name = EXCLUDED.legal_name,
             signature_name = EXCLUDED.signature_name,
             signer_is_guardian = EXCLUDED.signer_is_guardian,
             guardian_name = EXCLUDED.guardian_name,
             guardian_relationship = EXCLUDED.guardian_relationship,
             guardian_email = EXCLUDED.guardian_email,
             electronic_consent = EXCLUDED.electronic_consent,
             signed_at = now(),
             decided_by = CASE WHEN EXCLUDED.status = 'pending' THEN NULL ELSE volunteers.decided_by END,
             decided_by_name = CASE WHEN EXCLUDED.status = 'pending' THEN '' ELSE volunteers.decided_by_name END,
             decision_note = CASE WHEN EXCLUDED.status = 'pending' THEN '' ELSE volunteers.decision_note END,
             decided_at = CASE WHEN EXCLUDED.status = 'pending' THEN NULL ELSE volunteers.decided_at END",
    )
    .bind(user_id)
    .bind(status)
    .bind(agreement_version)
    .bind(&details.skills_focus)
    .bind(&details.date_of_birth)
    .bind(&details.ssn)
    .bind(&details.phone)
    .bind(&details.emergency_first_name)
    .bind(&details.emergency_last_name)
    .bind(&details.emergency_relationship)
    .bind(&details.emergency_phone)
    .bind(&details.volunteer_role)
    .bind(&details.legal_name)
    .bind(&details.signature_name)
    .bind(details.signer_is_guardian)
    .bind(&details.guardian_name)
    .bind(&details.guardian_relationship)
    .bind(&details.guardian_email)
    .bind(details.electronic_consent)
    .bind(&details.home_address)
    .execute(&mut *tx)
    .await?;

    sync_user_phone(&mut tx, user_id, &details.phone, actor).await?;
    sync_user_home_address(&mut tx, user_id, &details.home_address, actor).await?;

    tx.commit().await?;
    Ok(())
}

/// Update one volunteer's own details, auditing each field that changed. A blank
/// `details.ssn` keeps the stored number, which can be replaced but not cleared:
/// the agreement requires one.
pub async fn save_details(
    user_id: &str,
    details: &VolunteerDetails,
    actor: &str,
) -> Result<(), sqlx::Error> {
    type DetailsRow = (
        String,
        String,
        Option<String>,
        bool,
        String,
        String,
        String,
        String,
        String,
        String,
    );

    let mut tx = pool().begin().await?;

    let current = sqlx::query_as::<_, DetailsRow>(
        "SELECT skills_focus, volunteer_role, to_char(date_of_birth, 'YYYY-MM-DD'), (ssn <> ''),
                phone, emergency_first_name, emergency_last_name, emergency_relationship,
                emergency_phone, home_address
         FROM volunteers WHERE user_id = $1 FOR UPDATE",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((
        skills_focus,
        volunteer_role,
        date_of_birth,
        had_ssn,
        phone,
        emergency_first_name,
        emergency_last_name,
        emergency_relationship,
        emergency_phone,
        home_address,
    )) = current
    else {
        return Ok(());
    };

    sqlx::query(
        "UPDATE volunteers SET
             skills_focus = $2,
             date_of_birth = NULLIF($3, '')::date,
             ssn = CASE WHEN $4 = '' THEN ssn ELSE $4 END,
             phone = $5,
             emergency_first_name = $6,
             emergency_last_name = $7,
             emergency_relationship = $8,
             emergency_phone = $9,
             volunteer_role = $10,
             home_address = $11
         WHERE user_id = $1",
    )
    .bind(user_id)
    .bind(&details.skills_focus)
    .bind(&details.date_of_birth)
    .bind(&details.ssn)
    .bind(&details.phone)
    .bind(&details.emergency_first_name)
    .bind(&details.emergency_last_name)
    .bind(&details.emergency_relationship)
    .bind(&details.emergency_phone)
    .bind(&details.volunteer_role)
    .bind(&details.home_address)
    .execute(&mut *tx)
    .await?;

    let changes = [
        ("volunteer skills", skills_focus, &details.skills_focus),
        ("volunteer role", volunteer_role, &details.volunteer_role),
        (
            "volunteer date of birth",
            date_of_birth.unwrap_or_default(),
            &details.date_of_birth,
        ),
        ("volunteer phone", phone, &details.phone),
        (
            "volunteer home address",
            home_address,
            &details.home_address,
        ),
        (
            "volunteer emergency contact first name",
            emergency_first_name,
            &details.emergency_first_name,
        ),
        (
            "volunteer emergency contact last name",
            emergency_last_name,
            &details.emergency_last_name,
        ),
        (
            "volunteer emergency contact relationship",
            emergency_relationship,
            &details.emergency_relationship,
        ),
        (
            "volunteer emergency contact phone",
            emergency_phone,
            &details.emergency_phone,
        ),
    ];
    for (field, old, new) in changes {
        if &old != new {
            audit::record_in_transaction(
                &mut tx,
                audit::Entity::User,
                user_id,
                actor,
                field,
                &old,
                new,
            )
            .await?;
        }
    }

    // Presence only. The number never appears in the log.
    let has_ssn = had_ssn || !details.ssn.is_empty();
    if has_ssn != had_ssn {
        audit::record_in_transaction(
            &mut tx,
            audit::Entity::User,
            user_id,
            actor,
            "volunteer SSN",
            "",
            "on file",
        )
        .await?;
    }

    sync_user_phone(&mut tx, user_id, &details.phone, actor).await?;
    sync_user_home_address(&mut tx, user_id, &details.home_address, actor).await?;

    tx.commit().await?;
    Ok(())
}

/// Copy the volunteer's phone onto their user record when it differs, auditing
/// the change.
async fn sync_user_phone(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: &str,
    phone: &str,
    actor: &str,
) -> Result<(), sqlx::Error> {
    let current: Option<String> = sqlx::query_scalar("SELECT phone FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(&mut **tx)
        .await?;
    let Some(current) = current else {
        return Ok(());
    };
    if current == phone {
        return Ok(());
    }
    sqlx::query("UPDATE users SET phone = $1 WHERE id = $2")
        .bind(phone)
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    audit::record_in_transaction(
        tx,
        audit::Entity::User,
        user_id,
        actor,
        "phone",
        &current,
        phone,
    )
    .await
}

/// Copy the volunteer's home address onto their user record when it differs,
/// auditing the change. This is what makes it visible on the profile page, whose
/// contact block reads `users.home_address`. A blank address never overwrites
/// one already on file.
async fn sync_user_home_address(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: &str,
    home_address: &str,
    actor: &str,
) -> Result<(), sqlx::Error> {
    if home_address.is_empty() {
        return Ok(());
    }
    let current: Option<String> = sqlx::query_scalar("SELECT home_address FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(&mut **tx)
        .await?;
    let Some(current) = current else {
        return Ok(());
    };
    if current == home_address {
        return Ok(());
    }
    sqlx::query("UPDATE users SET home_address = $1 WHERE id = $2")
        .bind(home_address)
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    audit::record_in_transaction(
        tx,
        audit::Entity::User,
        user_id,
        actor,
        "home_address",
        &current,
        home_address,
    )
    .await
}

/// One volunteer's Social Security Number, auditing the disclosure first. The
/// only query that selects the `ssn` column.
pub async fn reveal_ssn(user_id: &str, actor: &str) -> Result<String, sqlx::Error> {
    let mut tx = pool().begin().await?;
    let ssn: Option<String> = sqlx::query_scalar("SELECT ssn FROM volunteers WHERE user_id = $1")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    let ssn = ssn.unwrap_or_default();
    if !ssn.is_empty() {
        audit::record_in_transaction(
            &mut tx,
            audit::Entity::User,
            user_id,
            actor,
            "volunteer SSN",
            "on file",
            "revealed",
        )
        .await?;
    }
    tx.commit().await?;
    Ok(ssn)
}

/// Every application waiting on a decision, oldest first so the longest wait is
/// dealt with first. One join: the queue shows a name and an email, so it does
/// not pay for the full user record and its case assignments.
pub async fn list_pending() -> Result<Vec<Volunteer>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct PendingRow {
        user_id: String,
        first_name: String,
        last_name: String,
        email: String,
        skills_focus: String,
        agreed_at: chrono::DateTime<chrono::Utc>,
    }

    let rows = sqlx::query_as::<_, PendingRow>(
        "SELECT v.user_id, u.first_name, u.last_name, u.email, v.skills_focus, v.agreed_at
         FROM volunteers v
         JOIN users u ON u.id = v.user_id
         WHERE v.status = 'pending'
           AND u.role <> 'deactivated'
         ORDER BY v.agreed_at",
    )
    .fetch_all(pool())
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| Volunteer {
            id: row.user_id,
            first_name: row.first_name,
            last_name: row.last_name,
            email: row.email,
            skills_focus: row.skills_focus,
            agreed_at: stamp(row.agreed_at),
        })
        .collect())
}

/// The library item id of the resume on file, empty when there is none.
///
/// Kept out of [`VolunteerApplication`] deliberately: the id is how the server
/// reaches the file, and nothing in the browser has any use for it.
pub async fn resume_item_id(user_id: &str) -> Result<String, sqlx::Error> {
    Ok(
        sqlx::query_scalar("SELECT resume_item_id FROM volunteers WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(pool())
            .await?
            .unwrap_or_default(),
    )
}

/// Record the resume just filed in the library, replacing any previous one.
pub async fn set_resume(
    user_id: &str,
    file_name: &str,
    item_id: &str,
    size_bytes: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE volunteers SET
             resume_file_name = $2,
             resume_item_id = $3,
             resume_size_bytes = $4,
             resume_uploaded_at = now()
         WHERE user_id = $1",
    )
    .bind(user_id)
    .bind(file_name)
    .bind(item_id)
    .bind(size_bytes)
    .execute(pool())
    .await?;
    Ok(())
}

/// Forget the resume on file. The file itself is deleted by the caller; this is
/// only the pointer.
pub async fn clear_resume(user_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE volunteers SET
             resume_file_name = '',
             resume_item_id = '',
             resume_size_bytes = 0,
             resume_uploaded_at = NULL
         WHERE user_id = $1",
    )
    .bind(user_id)
    .execute(pool())
    .await?;
    Ok(())
}

/// How many applications are waiting on a decision.
pub async fn pending_count() -> Result<i64, sqlx::Error> {
    // Matches `list_pending`: deactivating a pending applicant takes them out of
    // the queue, so the badge cannot count someone the list will not show.
    sqlx::query_scalar(
        "SELECT count(*) FROM volunteers v
         JOIN users u ON u.id = v.user_id
         WHERE v.status = 'pending' AND u.role <> 'deactivated'",
    )
    .fetch_one(pool())
    .await
}

/// Approve or decline a pending application. Approving delegates to
/// [`users::set_role_in`], which grants the role and audits it atomically.
/// Deciding one that is no longer pending errors rather than overwriting.
///
/// On approval `new_email` may carry the official volunteer address to make the
/// account's primary email; it is applied in the same transaction, before the
/// contact record is built from the account. Returns the previous email when it
/// was actually changed, so the caller can also notify the old address.
pub async fn decide(
    user_id: &str,
    approve: bool,
    actor_id: &str,
    actor_name: &str,
    note: &str,
    new_email: Option<&str>,
) -> Result<Option<String>, Error> {
    let mut tx = pool().begin().await?;
    // CRM audit rows written below inherit the deciding admin's stable identity.
    audit::set_actor_in_transaction(&mut tx, actor_id).await?;
    // Role changes take this lock before any user/volunteer row lock.
    users::lock_role_changes_in(&mut tx).await?;

    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM volunteers WHERE user_id = $1 FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    match status.as_deref() {
        Some("pending") => {}
        Some(_) => return Err(Error::AlreadyDecided),
        None => return Err(Error::NotFound),
    }

    let mut previous_email = None;
    if approve {
        // Ahead of the role and contact work, so the contact record created
        // below is built from the account's new address.
        if let Some(new_email) = new_email {
            previous_email = users::set_email_in(&mut tx, user_id, new_email, actor_name).await?;
        }
        // Sets the role *and* flips this record to approved, together.
        users::set_role_in(&mut tx, user_id, AccountRole::Volunteer, actor_name).await?;
        // `set_role_in` returns early when the account already holds the role,
        // so the contact record is ensured here too. It is idempotent.
        contacts::ensure_volunteer_in(&mut tx, user_id, actor_name).await?;
    }

    // Record the decision itself. On approval the status is already 'approved';
    // this adds who decided and why, and is what marks a decline.
    sqlx::query(
        "UPDATE volunteers SET
             status = $2,
             decided_by = $3,
             decided_by_name = $4,
             decision_note = $5,
             decided_at = now()
         WHERE user_id = $1",
    )
    .bind(user_id)
    .bind(if approve {
        VolunteerStatus::Approved.slug()
    } else {
        VolunteerStatus::Denied.slug()
    })
    .bind(actor_id)
    .bind(actor_name)
    .bind(note)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(previous_email)
}
