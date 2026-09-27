//! Volunteer applications filed by people who do not have an account yet (SSR
//! only).
//!
//! [`super::volunteers`] is keyed by `users(id)`, so it cannot hold one of
//! these: the public volunteer signup stages a fully signed agreement before any
//! account exists, exactly as [`super::pending_registrations`] stages a client
//! signup. The `users` row is created only by [`complete`], when the approved
//! applicant follows their setup link and chooses a password.
//!
//! Like `volunteers`, [`SELECT_COLUMNS`] omits `ssn` and reports only
//! `ssn <> ''`. The number is never read out at all: [`complete`] moves it onto
//! the volunteer record inside the database and clears it here.

use crate::helpers::volunteer_details::{VolunteerDetails, VolunteerDetailsView};
use crate::server::db::{audit, contacts, ids, pool, users};
use crate::server_fns::contacts::{ContactInput, ContactType};
use crate::server_fns::users::AccountRole;
use crate::server_fns::volunteer_applicants::{VolunteerApplicant, VolunteerSetup};

/// How long a setup link stays valid. Generous next to a password reset: this
/// one arrives unprompted, so the applicant may not be watching their inbox.
const SETUP_TTL_DAYS: i64 = 14;

/// How timestamps are rendered for display. These are shown, never compared.
const STAMP: &str = "%Y-%m-%d %H:%M";

/// What can go wrong handling an applicant. The `Display` text is what the admin
/// or applicant reads, so a lost race explains itself rather than leaking a
/// database error.
#[derive(Debug)]
pub enum Error {
    Database(sqlx::Error),
    NotFound,
    AlreadyDecided,
    EmailTaken,
    /// The setup link is unknown, expired, or already used.
    InvalidSetupToken,
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "{error}"),
            Self::NotFound => write!(formatter, "No volunteer application was found."),
            Self::AlreadyDecided => {
                write!(formatter, "This application has already been decided.")
            }
            Self::EmailTaken => write!(
                formatter,
                "Another account already uses that email address."
            ),
            Self::InvalidSetupToken => write!(
                formatter,
                "This setup link is invalid or has expired. Please contact an administrator."
            ),
        }
    }
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        // A race on either unique email index reads as a plain message.
        if let sqlx::Error::Database(database) = &error {
            if database.constraint().is_some_and(|constraint| {
                constraint == "users_email_lower_idx"
                    || constraint == "users_email_key"
                    || constraint == "volunteer_applicants_live_email_idx"
            }) {
                return Self::EmailTaken;
            }
        }
        Self::Database(error)
    }
}

/// Hash a setup token for storage/lookup (SHA-256, hex), as password resets do.
fn hash(raw: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(raw.as_bytes()))
}

fn stamp(at: chrono::DateTime<chrono::Utc>) -> String {
    at.with_timezone(&chrono::Local).format(STAMP).to_string()
}

/// The columns every ordinary read selects. `ssn` is absent by design.
const SELECT_COLUMNS: &str =
    "id, first_name, last_name, email, agreement_version, agreed_at, status, assigned_email,
     decided_by_name, decision_note, decided_at,
     skills_focus, volunteer_role, to_char(date_of_birth, 'YYYY-MM-DD') AS date_of_birth,
     (ssn <> '') AS has_ssn, phone, home_address, emergency_first_name, emergency_last_name,
     emergency_relationship, emergency_phone, legal_name, signature_name,
     signer_is_guardian, guardian_name, guardian_relationship, guardian_email, signed_at";

#[derive(sqlx::FromRow)]
struct ApplicantRow {
    id: String,
    first_name: String,
    last_name: String,
    email: String,
    agreement_version: String,
    agreed_at: chrono::DateTime<chrono::Utc>,
    status: String,
    assigned_email: String,
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
}

impl From<ApplicantRow> for VolunteerApplicant {
    fn from(row: ApplicantRow) -> Self {
        Self {
            id: row.id,
            first_name: row.first_name,
            last_name: row.last_name,
            email: row.email,
            agreement_version: row.agreement_version,
            agreed_at: stamp(row.agreed_at),
            status: row.status,
            assigned_email: row.assigned_email,
            decided_by_name: row.decided_by_name,
            decision_note: row.decision_note,
            decided_at: row.decided_at.map(stamp).unwrap_or_default(),
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
        }
    }
}

/// File a new application. Returns its id. The unique partial index on the
/// address is what makes a second live application for the same email an
/// [`Error::EmailTaken`] rather than a duplicate row.
pub async fn create(
    first_name: &str,
    last_name: &str,
    email: &str,
    agreement_version: &str,
    details: &VolunteerDetails,
) -> Result<String, Error> {
    let id = ids::opaque("va");
    sqlx::query(
        "INSERT INTO volunteer_applicants (
             id, first_name, last_name, email, agreement_version, status,
             skills_focus, volunteer_role, date_of_birth, ssn, phone,
             emergency_first_name, emergency_last_name, emergency_relationship,
             emergency_phone, legal_name, signature_name, signer_is_guardian,
             guardian_name, guardian_relationship, guardian_email,
             electronic_consent, signed_at, home_address
         )
         VALUES ($1, $2, $3, $4, $5, 'pending', $6, $7, NULLIF($8, '')::date, $9, $10,
                 $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, now(), $22)",
    )
    .bind(&id)
    .bind(first_name)
    .bind(last_name)
    .bind(email)
    .bind(agreement_version)
    .bind(&details.skills_focus)
    .bind(&details.volunteer_role)
    .bind(&details.date_of_birth)
    .bind(&details.ssn)
    .bind(&details.phone)
    .bind(&details.emergency_first_name)
    .bind(&details.emergency_last_name)
    .bind(&details.emergency_relationship)
    .bind(&details.emergency_phone)
    .bind(&details.legal_name)
    .bind(&details.signature_name)
    .bind(details.signer_is_guardian)
    .bind(&details.guardian_name)
    .bind(&details.guardian_relationship)
    .bind(&details.guardian_email)
    .bind(details.electronic_consent)
    .bind(&details.home_address)
    .execute(pool())
    .await?;
    Ok(id)
}

/// Whether a live (pending or approved) application already exists for `email`.
pub async fn live_email_exists(email: &str) -> Result<bool, sqlx::Error> {
    let exists: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM volunteer_applicants
         WHERE lower(email) = lower($1) AND status IN ('pending', 'approved') LIMIT 1",
    )
    .bind(email)
    .fetch_optional(pool())
    .await?;
    Ok(exists.is_some())
}

/// One applicant by id, whatever their status.
pub async fn get(id: &str) -> Result<Option<VolunteerApplicant>, sqlx::Error> {
    let row = sqlx::query_as::<_, ApplicantRow>(&format!(
        "SELECT {SELECT_COLUMNS} FROM volunteer_applicants WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool())
    .await?;
    Ok(row.map(Into::into))
}

/// Applications still awaiting a decision, oldest first, alongside those already
/// approved but not yet completed — an approved applicant has no account yet, so
/// they would otherwise vanish from every list until they follow their link.
pub async fn list_open() -> Result<Vec<VolunteerApplicant>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ApplicantRow>(&format!(
        "SELECT {SELECT_COLUMNS} FROM volunteer_applicants
         WHERE status IN ('pending', 'approved')
         ORDER BY agreed_at"
    ))
    .fetch_all(pool())
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// How many applications await a decision, for the admin badge.
pub async fn pending_count() -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM volunteer_applicants WHERE status = 'pending'")
        .fetch_one(pool())
        .await
}

/// Approve or decline an application. On approval `assigned_email` may carry the
/// official address to sign in with, and `setup_token` is stored hashed so the
/// emailed link can be redeemed once. Deciding one that is no longer pending
/// errors rather than overwriting.
///
/// The assigned address is checked against `users` here so the admin is told
/// immediately, rather than at completion when it is the applicant who would
/// hit it.
pub async fn decide(
    id: &str,
    approve: bool,
    actor_id: &str,
    actor_name: &str,
    note: &str,
    assigned_email: Option<&str>,
    setup_token: &str,
) -> Result<(), Error> {
    let mut tx = pool().begin().await?;

    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM volunteer_applicants WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    match status.as_deref() {
        Some("pending") => {}
        Some(_) => return Err(Error::AlreadyDecided),
        None => return Err(Error::NotFound),
    }

    if let Some(email) = assigned_email.filter(|_| approve) {
        let taken: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM users WHERE lower(email) = lower($1) LIMIT 1")
                .bind(email)
                .fetch_optional(&mut *tx)
                .await?;
        if taken.is_some() {
            return Err(Error::EmailTaken);
        }
    }

    // A decline carries no link and no expiry, so nothing is left redeemable.
    let (token_hash, expires_at) = if approve {
        (
            Some(hash(setup_token)),
            Some(chrono::Utc::now() + chrono::Duration::days(SETUP_TTL_DAYS)),
        )
    } else {
        (None, None)
    };

    sqlx::query(
        "UPDATE volunteer_applicants SET
             status = $2,
             decided_by = $3,
             decided_by_name = $4,
             decision_note = $5,
             decided_at = now(),
             assigned_email = $6,
             setup_token_hash = $7,
             setup_expires_at = $8
         WHERE id = $1",
    )
    .bind(id)
    .bind(if approve { "approved" } else { "denied" })
    .bind(actor_id)
    .bind(actor_name)
    .bind(note)
    .bind(assigned_email.filter(|_| approve).unwrap_or_default())
    .bind(token_hash)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

/// What the setup page shows for a live token: who it is for and which address
/// will sign them in. Does not consume the token.
pub async fn setup_for_token(token: &str) -> Result<Option<VolunteerSetup>, sqlx::Error> {
    let row: Option<(String, String, String, String)> = sqlx::query_as(
        "SELECT first_name, last_name, email, assigned_email FROM volunteer_applicants
         WHERE setup_token_hash = $1 AND status = 'approved' AND setup_expires_at > now()",
    )
    .bind(hash(token))
    .fetch_optional(pool())
    .await?;
    Ok(row.map(
        |(first_name, last_name, original_email, assigned_email)| {
            let email_changed =
                !assigned_email.is_empty() && !assigned_email.eq_ignore_ascii_case(&original_email);
            let sign_in_email = if assigned_email.is_empty() {
                original_email.clone()
            } else {
                assigned_email
            };
            VolunteerSetup {
                first_name,
                last_name,
                sign_in_email,
                original_email,
                email_changed,
            }
        },
    ))
}

/// Redeem a setup token and create the real account, all in one transaction:
/// the `users` row (role Volunteer), the `volunteers` row carrying the agreement
/// and details across verbatim, and the CRM contact record. Returns the new
/// user's id and the address they now sign in with.
///
/// The `UPDATE … RETURNING` on the token is what makes this single-use even
/// under concurrent requests, exactly as password reset does it.
pub async fn complete(token: &str, password_hash: &str) -> Result<(String, String), Error> {
    let mut tx = pool().begin().await?;
    // Role changes take this lock before any user/volunteer row lock.
    users::lock_role_changes_in(&mut tx).await?;

    // Claim the token. Only the fields the `users` row needs come back; every
    // other column is copied across in SQL below, so the SSN is never read out.
    let claimed: Option<(String, String, String, String, String, String, String)> = sqlx::query_as(
        "UPDATE volunteer_applicants SET status = 'completed', setup_token_hash = NULL
         WHERE setup_token_hash = $1 AND status = 'approved' AND setup_expires_at > now()
         RETURNING id, first_name, last_name, email, assigned_email, phone, home_address",
    )
    .bind(hash(token))
    .fetch_optional(&mut *tx)
    .await?;

    let Some((
        applicant_id,
        first_name,
        last_name,
        original_email,
        assigned_email,
        phone,
        home_address,
    )) = claimed
    else {
        return Err(Error::InvalidSetupToken);
    };

    // The admin's address wins when they issued one; otherwise they keep the
    // one they applied with.
    let sign_in_email = if assigned_email.is_empty() {
        original_email
    } else {
        assigned_email
    };

    // The address may have been claimed between approval and completion.
    let taken: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM users WHERE lower(email) = lower($1) LIMIT 1")
            .bind(&sign_in_email)
            .fetch_optional(&mut *tx)
            .await?;
    if taken.is_some() {
        return Err(Error::EmailTaken);
    }

    let user_id = users::next_id();
    users::insert_in(
        &mut tx,
        &user_id,
        &first_name,
        &last_name,
        &sign_in_email,
        &phone,
        &home_address,
        password_hash,
        AccountRole::Volunteer,
    )
    .await?;

    // Audit rows written below inherit the new account's stable identity.
    audit::set_actor_in_transaction(&mut tx, &user_id).await?;

    let full_name = format!("{first_name} {last_name}").trim().to_string();

    // `users.role = 'volunteer'` requires an approved `volunteers` row, so it is
    // written here in the same transaction. The decision that granted it comes
    // across too, so the record of who approved them is not lost.
    sqlx::query(
        "INSERT INTO volunteers (
             user_id, status, agreement_version, agreed_at, decided_by, decided_by_name,
             decision_note, decided_at, skills_focus, date_of_birth, ssn, phone,
             emergency_first_name, emergency_last_name, emergency_relationship,
             emergency_phone, volunteer_role, legal_name, signature_name,
             signer_is_guardian, guardian_name, guardian_relationship, guardian_email,
             electronic_consent, signed_at, home_address
         )
         SELECT $1, 'approved', a.agreement_version, a.agreed_at, a.decided_by,
                a.decided_by_name, a.decision_note, a.decided_at, a.skills_focus,
                a.date_of_birth, a.ssn, a.phone, a.emergency_first_name,
                a.emergency_last_name, a.emergency_relationship, a.emergency_phone,
                a.volunteer_role, a.legal_name, a.signature_name, a.signer_is_guardian,
                a.guardian_name, a.guardian_relationship, a.guardian_email,
                a.electronic_consent, a.signed_at, a.home_address
         FROM volunteer_applicants a WHERE a.id = $2",
    )
    .bind(&user_id)
    .bind(&applicant_id)
    .execute(&mut *tx)
    .await?;

    // The number now lives on the volunteer record, so it is cleared here rather
    // than kept in two places.
    sqlx::query("UPDATE volunteer_applicants SET ssn = '', completed_user_id = $2 WHERE id = $1")
        .bind(&applicant_id)
        .bind(&user_id)
        .execute(&mut *tx)
        .await?;

    // A volunteer belongs in the contact directory, like every other approval.
    contacts::create_linked_in(
        &mut tx,
        &ContactInput {
            first_name,
            last_name,
            email: sign_in_email.clone(),
            phone,
            address: home_address,
            types: vec![ContactType::Volunteer],
            source: "Volunteer signup".to_string(),
            ..Default::default()
        },
        &user_id,
        &full_name,
    )
    .await?;

    tx.commit().await?;
    Ok((user_id, sign_in_email))
}
