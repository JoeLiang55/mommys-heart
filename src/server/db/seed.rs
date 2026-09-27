//! One-time database seed. Ports the in-memory fixtures from [`crate::mockdata`]
//! into PostgreSQL, hashing the demo passwords so the existing "Demo autofill"
//! logins keep working. Runs only when the database is empty.

use crate::server::auth::hash_password;
use crate::server::db::{case_properties, channels, ids, messages, pool, users};
use crate::server_fns::audit::ChangeLogEntry;
use crate::server_fns::channels::{ChannelKind, DEFAULT_CHANNEL_NAME, VOLUNTEER_CHANNEL_NAME};
use crate::server_fns::users::{AccountRole, User};

/// Seed the database from the mock fixtures, but only if there are no users yet.
pub async fn seed_if_empty() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if users::count().await? > 0 {
        tracing::debug!("database already populated; skipping seed");
        return Ok(());
    }
    seed().await?;
    Ok(())
}

/// Force-refresh the demo/test data: wipe every domain table and re-insert the
/// mock fixtures. Invoked by `mommys-heart-app seed`, which is how you put the
/// shared development database back to known fixtures. The normal server
/// startup path does not use this; it only fills an *empty* database via
/// [`seed_if_empty`].
pub async fn reseed() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing::info!("wiping existing application data before reseeding");
    // One statement so FK constraints are satisfied atomically; RESTART IDENTITY
    // resets the audit_log sequence so ids are reproducible across reseeds.
    sqlx::query(
        "TRUNCATE users, sessions, grants, cases, case_properties, case_notes,
                  case_note_addenda, case_note_audit_log, case_document_permissions,
                  case_channels, messages, case_assignments, audit_log,
                  organizations, contacts, contact_properties, organization_properties,
                  contact_category_assignments, contact_communications,
                  case_contacts, funding
         RESTART IDENTITY CASCADE",
    )
    .execute(pool())
    .await?;
    seed().await?;
    Ok(())
}

async fn seed() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let pool = pool();
    // Only the first volunteer gets a signed agreement and filled-in details.
    let mut seeded_a_signed_volunteer = false;

    // 1. Users (hash the plaintext demo passwords) + their per-user audit log.
    for (u, password) in crate::mockdata::users() {
        let password_hash = hash_password(&password)?;
        sqlx::query(
            "INSERT INTO users (
                 id, first_name, last_name, email, phone, home_address, password_hash, role,
                 information_management_access
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&u.id)
        .bind(&u.first_name)
        .bind(&u.last_name)
        .bind(&u.email)
        .bind(&u.phone)
        .bind(&u.home_address)
        .bind(&password_hash)
        .bind(u.role.slug())
        .bind(u.information_management_access)
        .execute(pool)
        .await?;

        // The subtype record each role implies, so a fresh database satisfies
        // the same invariant `users::set_role_in` maintains at runtime.
        match u.role {
            AccountRole::Volunteer => {
                // The first seeded volunteer has signed the current agreement
                // and filled in the details form, including an SSN, so the
                // panel and the audited reveal are demoable. The rest stay
                // backfill-shaped: approved but predating the agreement.
                let signed = !seeded_a_signed_volunteer;
                seeded_a_signed_volunteer = true;
                sqlx::query(
                    "INSERT INTO volunteers (
                         user_id, status, agreement_version, decided_by_name, skills_focus,
                         date_of_birth, ssn, phone, emergency_first_name, emergency_last_name,
                         emergency_relationship, emergency_phone, volunteer_role, legal_name,
                         signature_name, electronic_consent, signed_at, home_address
                     )
                     VALUES ($1, 'approved', $2, 'Seed', $3, NULLIF($4, '')::date, $5, $6, $7, $8, $9, $10,
                             $11, $12, $13, $14, CASE WHEN $14 THEN now() ELSE NULL END, $15)",
                )
                .bind(&u.id)
                .bind(if signed {
                    crate::helpers::volunteer_terms::VOLUNTEER_AGREEMENT_VERSION
                } else {
                    ""
                })
                .bind(if signed {
                    "Family law research, client intake, and grant writing."
                } else {
                    ""
                })
                .bind(if signed { "1988-04-02" } else { "" })
                .bind(if signed { "123456789" } else { "" })
                .bind(if signed { u.phone.as_str() } else { "" })
                .bind(if signed { "Priya" } else { "" })
                .bind(if signed { "Patel" } else { "" })
                .bind(if signed { "Sister" } else { "" })
                .bind(if signed { "(555) 204-1188" } else { "" })
                .bind(if signed { "Case intake volunteer" } else { "" })
                .bind(if signed {
                    format!("{} {}", u.first_name, u.last_name)
                } else {
                    String::new()
                })
                .bind(if signed {
                    format!("{} {}", u.first_name, u.last_name)
                } else {
                    String::new()
                })
                .bind(signed)
                .bind(if signed {
                    u.home_address.as_str()
                } else {
                    ""
                })
                .execute(pool)
                .await?;
            }
            AccountRole::Client => {
                sqlx::query("INSERT INTO clients (user_id) VALUES ($1)")
                    .bind(&u.id)
                    .execute(pool)
                    .await?;
            }
            AccountRole::Deactivated => {
                // The subtype row this role implies, exactly as the two arms
                // above do for theirs. `previous_role` is what a reactivation
                // restores, so the schema's completeness CHECK is satisfied.
                let deactivation = u
                    .deactivation
                    .as_ref()
                    .expect("a seeded deactivated user carries its deactivation record");
                sqlx::query(
                    "INSERT INTO account_deactivations
                         (user_id, previous_role, reason, deactivated_by_name)
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(&u.id)
                .bind(deactivation.previous_role.slug())
                .bind(&deactivation.reason)
                .bind(&deactivation.by)
                .execute(pool)
                .await?;
            }
            _ => {}
        }

        insert_audit(&u.id, "user", &Vec::<ChangeLogEntry>::new()).await?;
    }

    // 2. Cases + their default chat channels, notes, evidence, properties, and
    //    case audit log.
    for c in crate::mockdata::cases() {
        // A withdrawn case must carry its withdrawal metadata and the status to
        // restore it to, or the schema's CHECK rejects the row.
        let withdrawal = c.withdrawal.clone();
        let previous_status = if withdrawal.is_some() {
            crate::mockdata::WITHDRAWN_CASE_PREVIOUS_STATUS.slug()
        } else {
            ""
        };
        sqlx::query(
            "INSERT INTO cases (id, name, status, review_reason, owner_id,
                                withdrawn_by, withdrawn_at, withdrawal_reason,
                                status_before_withdrawal)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&c.id)
        .bind(&c.name)
        .bind(c.status.slug())
        .bind(&c.review_reason)
        .bind(&c.owner_id)
        .bind(withdrawal.as_ref().map(|w| w.by.as_str()).unwrap_or(""))
        .bind(withdrawal.as_ref().map(|w| w.at.as_str()).unwrap_or(""))
        .bind(withdrawal.as_ref().map(|w| w.reason.as_str()).unwrap_or(""))
        .bind(previous_status)
        .execute(pool)
        .await?;

        // Same two channels every runtime-created case gets: the permanent
        // volunteer-only back-channel plus "General".
        let mut conn = pool.acquire().await?;
        channels::create_defaults(&mut conn, &c.id).await?;
        drop(conn);

        // Seeded cases carry the same intake/outtake fields a case created
        // through the app gets, so the demo data shows what a real case
        // actually looks like rather than a simplified version of one.
        //
        // Their document folders are not created here: those live in the
        // SharePoint library, and seeding must not depend on — or write to — an
        // external service. The startup backfill provisions them once the
        // server is running.
        let mut tx = pool.begin().await?;
        case_properties::add_for_new_case(&mut tx, &c.id, c.properties.iter().cloned()).await?;
        tx.commit().await?;

        for n in &c.notes {
            sqlx::query(
                "INSERT INTO case_notes (id, case_id, author, body, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $5)",
            )
            .bind(&n.id)
            .bind(&c.id)
            .bind(&n.author)
            .bind(&n.body)
            .bind(&n.created_at)
            .execute(pool)
            .await?;
        }

        insert_audit(&c.id, "case", &Vec::<ChangeLogEntry>::new()).await?;
    }

    // 3. Case assignments (after both users and cases exist).
    for (u, _) in crate::mockdata::users() {
        insert_assignments(&u).await?;
    }

    // 4. Case chat messages, routed into the channel each one belongs to.
    let mut channel_ids: std::collections::HashMap<(String, &'static str), String> =
        std::collections::HashMap::new();
    for sm in crate::mockdata::messages() {
        let m = &sm.message;
        let name = match sm.channel {
            ChannelKind::Standard => DEFAULT_CHANNEL_NAME,
            ChannelKind::VolunteerOnly => VOLUNTEER_CHANNEL_NAME,
        };
        let key = (m.case_id.clone(), name);
        let channel_id = match channel_ids.get(&key) {
            Some(id) => id.clone(),
            None => {
                let id = channels::id_for(&m.case_id, name)
                    .await?
                    .ok_or_else(|| format!("seed: case {} has no \"{name}\" channel", m.case_id))?;
                channel_ids.insert(key, id.clone());
                id
            }
        };
        messages::insert(
            &m.id,
            &m.case_id,
            &channel_id,
            &m.author_id,
            &m.author,
            &m.body,
            &m.sent_at,
        )
        .await?;
    }

    // 5. CRM fixtures: organizations, people who are not accounts, the custom
    //    properties on them, who is on which case, and the money.
    seed_crm_fixtures().await?;

    // 6. A handful of real audit-log entries, date-spread across the last ~6
    //    weeks, so the change-log views (which fetch the audit log as their own
    //    paginated, date-filtered data source) have data to show and page
    //    through in the demo. Real edits made in the running app append more.
    seed_audit_fixtures().await?;

    // 7. Advance the shared id sequence past every seeded id. Seed ids are
    //    `prefix-<n>` numbered per prefix from 1 (e.g. `m-1`..`m-13000`), and
    //    those counts can exceed the sequence's START value. Since `ids::next`
    //    hands out `<prefix>-<nextval>` from this one global sequence, leaving it
    //    below the largest seeded suffix makes the first newly-created record
    //    collide with seed data (e.g. `m-5001`) and violate the primary key.
    //    Bump it above the max suffix across every table that receives new ids.
    advance_id_sequence().await?;

    tracing::info!("database seeded from mock fixtures");
    Ok(())
}

/// Set `app_id_seq` above the largest numeric suffix of any seeded id so that
/// `ids::next` never reissues an id that already exists.
///
/// `users` is deliberately absent: user ids are random hex from `ids::opaque`,
/// not sequence-derived, so their suffix is not a number to compare against
/// (and casting one to `bigint` would error).
async fn advance_id_sequence() -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT setval('app_id_seq', GREATEST(
             (SELECT COALESCE(max(split_part(id, '-', 2)::bigint), 0) FROM grants),
             (SELECT COALESCE(max(split_part(id, '-', 2)::bigint), 0) FROM cases),
             (SELECT COALESCE(max(split_part(id, '-', 2)::bigint), 0) FROM case_notes),
             (SELECT COALESCE(max(split_part(id, '-', 2)::bigint), 0) FROM case_note_addenda),
             (SELECT COALESCE(max(split_part(id, '-', 2)::bigint), 0)
                FROM case_note_audit_log
                WHERE split_part(id, '-', 2) ~ '^[0-9]+$'),
             (SELECT COALESCE(max(split_part(id, '-', 2)::bigint), 0) FROM case_channels),
             (SELECT COALESCE(max(split_part(id, '-', 2)::bigint), 0) FROM messages),
             (SELECT COALESCE(max(split_part(id, '-', 2)::bigint), 0) FROM audit_log),
             (SELECT COALESCE(max(split_part(id, '-', 2)::bigint), 0) FROM organizations),
             (SELECT COALESCE(max(split_part(id, '-', 2)::bigint), 0) FROM case_contacts),
             (SELECT COALESCE(max(split_part(id, '-', 2)::bigint), 0) FROM funding),
             -- `contacts` is deliberately absent: the migration backfills ids as
             -- `ct-u-<hex>`, whose second segment is not a number.
             (SELECT COALESCE(max(split_part(id, '-', 2)::bigint), 0)
                FROM contacts WHERE split_part(id, '-', 2) ~ '^[0-9]+$')
         ) + 1, false)",
    )
    .execute(pool())
    .await?;
    Ok(())
}

/// Seed the CRM: partner and funder organizations, people who have no account,
/// custom properties, case involvement, and grants with their funding.
///
/// The account-linked contacts already exist — migration 0019 backfills one per
/// user — so this only adds what an account cannot represent.
async fn seed_crm_fixtures() -> Result<(), sqlx::Error> {
    let pool = pool();

    // One contact per seeded account. Migration 0019 does this for a database
    // that already had users, but on a fresh database the migration runs before
    // any user exists — and `reseed` truncates contacts — so the seed has to do
    // it too, with the same `ct-<user id>` scheme.
    sqlx::query(
        "INSERT INTO contacts
             (id, first_name, last_name, email, phone, address, user_id, types, source)
         SELECT 'ct-' || u.id, u.first_name, u.last_name, u.email, u.phone,
                u.home_address, u.id,
                CASE u.role
                    WHEN 'client' THEN ARRAY['client']
                    WHEN 'volunteer' THEN ARRAY['volunteer']
                    ELSE ARRAY['staff']
                END,
                'Account'
         FROM users u
         WHERE btrim(u.last_name) <> ''
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await?;

    // (id, name, kind, website, email, phone, description)
    let organizations = [
        (
            "org-1",
            "Harbor Community Foundation",
            "funder",
            "harborcf.org",
            "grants@harborcf.org",
            "(555) 010-2200",
            "Local family foundation; funds our family-court advocacy work.",
        ),
        (
            "org-2",
            "State Office for Victims of Crime",
            "government",
            "ovc.state.gov",
            "vocagrants@state.gov",
            "(555) 010-4400",
            "Administers the VOCA formula grant.",
        ),
        (
            "org-3",
            "Riverside Legal Aid",
            "partner",
            "riversidelegal.org",
            "intake@riversidelegal.org",
            "(555) 010-6600",
            "Pro bono family-law representation for referred clients.",
        ),
        (
            "org-4",
            "Bayside Counseling Center",
            "service_provider",
            "baysidecounseling.org",
            "referrals@baysidecounseling.org",
            "(555) 010-7700",
            "Trauma-informed counseling; accepts sliding-scale referrals.",
        ),
        (
            "org-5",
            "County Family Court",
            "court",
            "",
            "clerk@countyfamilycourt.gov",
            "(555) 010-8800",
            "Family division; docket clerk handles our filings.",
        ),
        (
            "org-6",
            "Meridian Tech",
            "employer",
            "meridiantech.example",
            "",
            "",
            "Corporate donor; matches employee giving.",
        ),
    ];
    for (id, name, kind, website, email, phone, description) in organizations {
        sqlx::query(
            "INSERT INTO organizations (id, name, kind, website, email, phone, description)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(name)
        .bind(kind)
        .bind(website)
        .bind(email)
        .bind(phone)
        .bind(description)
        .execute(pool)
        .await?;
    }

    // People with no login: the reason a CRM needs contacts separate from users.
    // (id, first, last, title, org, types, email, phone, source, description)
    let contacts = [
        (
            "ct-101",
            "Miriam",
            "Alvarez",
            "Program Officer",
            Some("org-1"),
            vec!["funder_contact"],
            "malvarez@harborcf.org",
            "(555) 010-2201",
            "Grant application 2026",
            "Primary contact for the Harbor family-advocacy grant.",
        ),
        (
            "ct-102",
            "Dennis",
            "Whitfield",
            "Grants Administrator",
            Some("org-2"),
            vec!["funder_contact", "government_agency"],
            "dwhitfield@state.gov",
            "(555) 010-4401",
            "VOCA award",
            "Handles VOCA reporting and reimbursement questions.",
        ),
        (
            "ct-103",
            "Sandra",
            "Oyelaran",
            "Staff Attorney",
            Some("org-3"),
            vec!["attorney", "partner"],
            "soyelaran@riversidelegal.org",
            "(555) 010-6601",
            "Partner referral agreement",
            "Takes our custody referrals; prefers email intake.",
        ),
        (
            "ct-104",
            "Peter",
            "Grady",
            "Licensed Counselor",
            Some("org-4"),
            vec!["service_provider"],
            "pgrady@baysidecounseling.org",
            "(555) 010-7701",
            "Provider outreach",
            "Six sliding-scale slots reserved for our clients each month.",
        ),
        (
            "ct-105",
            "Yvonne",
            "Marsh",
            "Docket Clerk",
            Some("org-5"),
            vec!["court_professional"],
            "ymarsh@countyfamilycourt.gov",
            "(555) 010-8801",
            "Court liaison",
            "Confirms hearing dates and filing receipts.",
        ),
        (
            "ct-106",
            "Aaron",
            "Feldman",
            "",
            Some("org-6"),
            vec!["donor"],
            "aaron.feldman@meridiantech.example",
            "(555) 010-9900",
            "Annual appeal 2026",
            "Recurring individual donor; employer matches gifts.",
        ),
        (
            "ct-107",
            "Grace",
            "Adeyemi",
            "Board Chair",
            None,
            vec!["board_member", "donor"],
            "grace.adeyemi@example.com",
            "(555) 010-1100",
            "Founding board",
            "Chairs the finance committee.",
        ),
        (
            "ct-108",
            "Teresa",
            "Nguyen",
            "",
            None,
            vec!["emergency_contact"],
            "",
            "(555) 010-3300",
            "Client intake",
            "Sister of a client; listed as their emergency contact.",
        ),
    ];
    for (id, first, last, title, org, types, email, phone, source, description) in contacts {
        sqlx::query(
            "INSERT INTO contacts
                 (id, first_name, last_name, job_title, organization_id, types,
                  email, phone, source, description)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(id)
        .bind(first)
        .bind(last)
        .bind(title)
        .bind(org)
        .bind(types.iter().map(|t| t.to_string()).collect::<Vec<_>>())
        .bind(email)
        .bind(phone)
        .bind(source)
        .bind(description)
        .execute(pool)
        .await?;
    }

    // Custom properties, showing the same section grouping cases use.
    //
    // "Location" is deliberately shared across several people with repeated
    // values, so the property filter has a real facet to demonstrate on a fresh
    // database.
    let properties = [
        ("ct-101", 0, "Preferred contact", "Email", "Relationship"),
        (
            "ct-101",
            1,
            "Reporting portal",
            "harborcf.org/grantee",
            "Relationship",
        ),
        ("ct-101", 2, "Site visit", "", "Relationship"),
        ("ct-103", 0, "Bar number", "SB-448120", "Professional"),
        (
            "ct-103",
            1,
            "Practice areas",
            "Custody, protective orders",
            "Professional",
        ),
        (
            "ct-103",
            2,
            "Referral capacity",
            "3 active matters",
            "Professional",
        ),
        (
            "ct-104",
            0,
            "Languages",
            "English, Portuguese",
            "Professional",
        ),
        (
            "ct-104",
            1,
            "Sliding scale",
            "Yes \u{2014} 6 slots per month",
            "Professional",
        ),
        ("ct-107", 0, "Board term ends", "2027-06-30", "Governance"),
        ("ct-107", 1, "Committee", "Finance (chair)", "Governance"),
        (
            "ct-107",
            2,
            "Conflict of interest form",
            "Signed 2026-01-14",
            "Governance",
        ),
        ("ct-101", 3, "Location", "Harbor City", "Relationship"),
        ("ct-102", 0, "Location", "Harbor City", "Relationship"),
        ("ct-103", 3, "Location", "Riverside", "Professional"),
        ("ct-104", 2, "Location", "Riverside", "Professional"),
        ("ct-105", 0, "Location", "Harbor City", "Relationship"),
        ("ct-106", 0, "Location", "Remote", "Relationship"),
        // Named but unfilled, so the "not filled in" facet value has a member.
        ("ct-107", 3, "Location", "", "Relationship"),
    ];
    for (contact_id, ord, key, value, section) in properties {
        sqlx::query(
            "INSERT INTO contact_properties (contact_id, ord, key, value, section)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(contact_id)
        .bind(ord as i32)
        .bind(key)
        .bind(value)
        .bind(section)
        .execute(pool)
        .await?;
    }

    // Organization properties mirroring the person-level Location facet, so the
    // Organizations directory has something to filter by too.
    let organization_properties = [
        ("org-1", 0, "Location", "Harbor City", "Relationship"),
        ("org-2", 0, "Location", "Harbor City", "Relationship"),
        ("org-3", 0, "Location", "Riverside", "Relationship"),
        ("org-4", 0, "Location", "Riverside", "Relationship"),
        ("org-5", 0, "Location", "Harbor City", "Relationship"),
        ("org-6", 0, "Location", "Remote", "Relationship"),
    ];
    for (organization_id, ord, key, value, section) in organization_properties {
        sqlx::query(
            "INSERT INTO organization_properties (organization_id, ord, key, value, section)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(organization_id)
        .bind(ord as i32)
        .bind(key)
        .bind(value)
        .bind(section)
        .execute(pool)
        .await?;
    }

    // Every seeded person has the same missing defaults a runtime create receives.
    for contact_id in sqlx::query_scalar::<_, String>("SELECT id FROM contacts")
        .fetch_all(pool)
        .await?
    {
        let mut tx = pool.begin().await?;
        crate::server::db::contact_properties::ensure_defaults_in_transaction(&mut tx, &contact_id)
            .await?;
        tx.commit().await?;
    }

    for organization_id in sqlx::query_scalar::<_, String>("SELECT id FROM organizations")
        .fetch_all(pool)
        .await?
    {
        let mut tx = pool.begin().await?;
        crate::server::db::organization_properties::ensure_defaults_in_transaction(
            &mut tx,
            &organization_id,
        )
        .await?;
        tx.commit().await?;
    }

    // Mirror runtime signup: each client owner is the primary person on their case.
    sqlx::query(
        "INSERT INTO case_contacts
             (id, case_id, contact_id, role, note, is_primary, added_by)
         SELECT 'cc-' || nextval('app_id_seq'), ca.id, ct.id, 'client', '',
                NOT EXISTS (SELECT 1 FROM case_contacts existing
                            WHERE existing.case_id = ca.id AND existing.is_primary),
                'Seed'
         FROM cases ca
         JOIN users u ON u.id = ca.owner_id AND u.role = 'client'
         JOIN contacts ct ON ct.user_id = u.id AND NOT ct.archived
         WHERE NOT EXISTS (
             SELECT 1 FROM case_contacts existing
             WHERE existing.case_id = ca.id AND existing.contact_id = ct.id
               AND existing.role = 'client'
         )",
    )
    .execute(pool)
    .await?;

    // Who is involved in the first two seeded cases.
    let case_contacts = [
        (
            "cc-9001",
            "c-1",
            "ct-103",
            "attorney",
            "Representing the client in the custody matter.",
            false,
        ),
        (
            "cc-9002",
            "c-1",
            "ct-108",
            "emergency_contact",
            "Call only outside work hours.",
            false,
        ),
        (
            "cc-9003",
            "c-1",
            "ct-105",
            "court_professional",
            "Confirms hearing dates.",
            false,
        ),
        (
            "cc-9004",
            "c-2",
            "ct-104",
            "provider_contact",
            "Counseling referral accepted.",
            false,
        ),
    ];
    for (id, case_id, contact_id, role, note, is_primary) in case_contacts {
        sqlx::query(
            "INSERT INTO case_contacts (id, case_id, contact_id, role, note, is_primary, added_by)
             VALUES ($1, $2, $3, $4, $5, $6, 'Seed')",
        )
        .bind(id)
        .bind(case_id)
        .bind(contact_id)
        .bind(role)
        .bind(note)
        .bind(is_primary)
        .execute(pool)
        .await?;
    }

    // Grants across the lifecycle, so every status is demoable. Amounts are
    // cents. (id, name, status, funder, officer, requested, awarded, applied,
    //  decided, start, end, cadence, purpose)
    let grants: [(
        &str,
        &str,
        &str,
        &str,
        Option<&str>,
        Option<i64>,
        Option<i64>,
        Option<&str>,
        Option<&str>,
        Option<&str>,
        Option<&str>,
        &str,
        &str,
    ); 6] = [
        (
            "gr-1",
            "Harbor Family Advocacy Grant",
            "active",
            "org-1",
            Some("ct-101"),
            Some(7_500_000),
            Some(6_000_000),
            Some("2025-09-15"),
            Some("2025-11-01"),
            Some("2026-01-01"),
            Some("2026-12-31"),
            "quarterly",
            "Funds two part-time family-court advocates.",
        ),
        (
            "gr-2",
            "VOCA Victim Services Formula Grant",
            "reporting",
            "org-2",
            Some("ct-102"),
            Some(12_000_000),
            Some(9_500_000),
            Some("2025-06-02"),
            Some("2025-08-20"),
            Some("2025-10-01"),
            Some("2026-09-30"),
            "semiannual",
            "Direct victim services, including safety planning and court accompaniment.",
        ),
        (
            "gr-3",
            "Harbor Capacity Building",
            "applied",
            "org-1",
            Some("ct-101"),
            Some(2_500_000),
            None,
            Some("2026-01-20"),
            None,
            None,
            None,
            "final_only",
            "Case-management software and staff training.",
        ),
        (
            "gr-4",
            "Community Resilience Fund",
            "prospect",
            "org-1",
            None,
            Some(4_000_000),
            None,
            None,
            None,
            None,
            None,
            "none",
            "Prospective renewal for outreach in the north county.",
        ),
        (
            "gr-5",
            "Emergency Housing Supplement",
            "closed",
            "org-2",
            Some("ct-102"),
            Some(3_000_000),
            Some(3_000_000),
            Some("2024-05-01"),
            Some("2024-07-15"),
            Some("2024-09-01"),
            Some("2025-08-31"),
            "annual",
            "Short-term hotel placement for clients fleeing unsafe homes.",
        ),
        (
            "gr-6",
            "Statewide Legal Access Initiative",
            "declined",
            "org-2",
            None,
            Some(8_000_000),
            None,
            Some("2025-03-10"),
            Some("2025-05-30"),
            None,
            None,
            "none",
            "Not funded; reapply in the next cycle.",
        ),
    ];
    for (
        id,
        name,
        status,
        funder,
        officer,
        requested,
        awarded,
        applied,
        decided,
        start,
        end,
        cadence,
        purpose,
    ) in grants
    {
        sqlx::query(
            "INSERT INTO grants
                 (id, name, status, funder_organization_id, program_officer_contact_id,
                  amount_requested_cents, amount_awarded_cents, application_date,
                  decision_date, period_start, period_end, reporting_cadence, purpose)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8::date, $9::date, $10::date, $11::date, $12, $13)",
        )
        .bind(id)
        .bind(name)
        .bind(status)
        .bind(funder)
        .bind(officer)
        .bind(requested)
        .bind(awarded)
        .bind(applied)
        .bind(decided)
        .bind(start)
        .bind(end)
        .bind(cadence)
        .bind(purpose)
        .execute(pool)
        .await?;
    }

    // Money in, including one voided record so the correction path is visible.
    let funding: [(
        &str,
        &str,
        i64,
        &str,
        Option<&str>,
        Option<&str>,
        Option<&str>,
        &str,
        bool,
        &str,
    ); 8] = [
        (
            "fn-1",
            "grant_payment",
            1_500_000,
            "2026-01-15",
            Some("gr-1"),
            None,
            None,
            "ACH 88213",
            false,
            "",
        ),
        (
            "fn-2",
            "grant_payment",
            1_500_000,
            "2026-04-15",
            Some("gr-1"),
            None,
            None,
            "ACH 90114",
            false,
            "",
        ),
        (
            "fn-3",
            "grant_payment",
            4_750_000,
            "2025-10-10",
            Some("gr-2"),
            None,
            None,
            "Wire 5521",
            false,
            "",
        ),
        (
            "fn-4",
            "grant_payment",
            3_000_000,
            "2024-09-20",
            Some("gr-5"),
            None,
            None,
            "Cheque 3021",
            false,
            "",
        ),
        (
            "fn-5",
            "donation",
            250_000,
            "2026-02-02",
            None,
            Some("org-6"),
            None,
            "Matching gift",
            false,
            "",
        ),
        (
            "fn-6",
            "donation",
            100_000,
            "2026-02-11",
            None,
            None,
            Some("ct-107"),
            "Annual appeal",
            false,
            "",
        ),
        (
            "fn-7",
            "in_kind",
            75_000,
            "2026-01-30",
            None,
            Some("org-3"),
            None,
            "Pro bono hours",
            false,
            "",
        ),
        (
            "fn-8",
            "grant_payment",
            1_500_000,
            "2026-04-15",
            Some("gr-1"),
            None,
            None,
            "ACH 90114",
            true,
            "Duplicate of ACH 90114; entered twice.",
        ),
    ];
    for (id, kind, amount, received, grant, org, contact, reference, voided, reason) in funding {
        sqlx::query(
            "INSERT INTO funding
                 (id, kind, amount_cents, received_on, grant_id, source_organization_id,
                  source_contact_id, reference, recorded_by, voided, void_reason,
                  voided_by, voided_at)
             VALUES ($1, $2, $3, $4::date, $5, $6, $7, $8, 'Seed', $9, $10,
                     CASE WHEN $9 THEN 'Seed' ELSE '' END,
                     CASE WHEN $9 THEN now() ELSE NULL END)",
        )
        .bind(id)
        .bind(kind)
        .bind(amount)
        .bind(received)
        .bind(grant)
        .bind(org)
        .bind(contact)
        .bind(reference)
        .bind(voided)
        .bind(reason)
        .execute(pool)
        .await?;
    }

    Ok(())
}

/// A single seeded audit entry: how many days before "now" it occurred plus the
/// change it records. Kept as fixtures so the demo change logs have real,
/// date-spread data to page through and filter.
struct SeedAudit {
    entity_type: &'static str,
    entity_id: &'static str,
    days_ago: i64,
    actor: &'static str,
    field: &'static str,
    old_value: &'static str,
    new_value: &'static str,
}

/// Insert a small, realistic set of audit entries against a few fixture users
/// and cases, dated across the last ~6 weeks so the default "last 2 weeks" view
/// shows some and widening the range reveals the rest. Ordered oldest-first so
/// the newest change gets the highest `seq` and sorts to the top (matching how
/// live edits accumulate).
async fn seed_audit_fixtures() -> Result<(), sqlx::Error> {
    let pool = pool();
    let mut fixtures = [
        SeedAudit {
            entity_type: "case",
            entity_id: "c-1",
            days_ago: 41,
            actor: "Maria Nguyen",
            field: "status",
            old_value: "Open",
            new_value: "Monitor",
        },
        SeedAudit {
            entity_type: "user",
            entity_id: "u-2",
            days_ago: 33,
            actor: "Maria Nguyen",
            field: "role",
            old_value: "Client",
            new_value: "Volunteer",
        },
        SeedAudit {
            entity_type: "case",
            entity_id: "c-2",
            days_ago: 28,
            actor: "Dana Patel",
            field: "owner",
            old_value: "James Garcia",
            new_value: "Dana Patel",
        },
        SeedAudit {
            entity_type: "user",
            entity_id: "u-1",
            days_ago: 20,
            actor: "Maria Nguyen",
            field: "permissions on c-3",
            old_value: "Viewer",
            new_value: "Manager",
        },
        SeedAudit {
            entity_type: "case",
            entity_id: "c-1",
            days_ago: 12,
            actor: "Dana Patel",
            field: "name",
            old_value: "Maria-Nguyen custody matter",
            new_value: "Nguyen custody matter",
        },
        SeedAudit {
            entity_type: "case",
            entity_id: "c-3",
            days_ago: 9,
            actor: "Maria Nguyen",
            field: "status",
            old_value: "Monitor",
            new_value: "Closed",
        },
        SeedAudit {
            entity_type: "user",
            entity_id: "u-2",
            days_ago: 5,
            actor: "Maria Nguyen",
            field: "permissions on c-1",
            old_value: "none",
            new_value: "Contributor",
        },
        SeedAudit {
            entity_type: "case",
            entity_id: "c-2",
            days_ago: 2,
            actor: "Dana Patel",
            field: "Docket",
            old_value: "FC-2026-0002",
            new_value: "FC-2026-0002-A",
        },
        SeedAudit {
            entity_type: "user",
            entity_id: "u-1",
            days_ago: 1,
            actor: "Maria Nguyen",
            field: "role",
            old_value: "Volunteer",
            new_value: "Site admin",
        },
        SeedAudit {
            entity_type: "case",
            entity_id: "c-1",
            days_ago: 0,
            actor: "Maria Nguyen",
            field: "status",
            old_value: "Monitor",
            new_value: "Open",
        },
    ];
    // Oldest first so the newest change ends up with the highest `seq`.
    fixtures.sort_by_key(|f| std::cmp::Reverse(f.days_ago));

    for f in fixtures {
        let id = ids::next(pool, "cl").await?;
        // User fixtures name their subject positionally ("u-2" = the second
        // seeded user); resolve that to the generated opaque id.
        let entity_id = match f
            .entity_id
            .strip_prefix("u-")
            .and_then(|n| n.parse::<usize>().ok())
        {
            Some(n) if f.entity_type == "user" => crate::mockdata::user_id(n - 1),
            _ => f.entity_id.to_string(),
        };
        let at = (chrono::Local::now() - chrono::Duration::days(f.days_ago))
            .format("%Y-%m-%d %H:%M")
            .to_string();
        sqlx::query(
            "INSERT INTO audit_log
                (id, entity_type, entity_id, actor, field, old_value, new_value, at, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now() - make_interval(days => $9))",
        )
        .bind(&id)
        .bind(f.entity_type)
        .bind(&entity_id)
        .bind(f.actor)
        .bind(f.field)
        .bind(f.old_value)
        .bind(f.new_value)
        .bind(&at)
        .bind(f.days_ago as i32)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Insert audit entries preserving their ids/timestamps. Entries are stored
/// newest-first in the fixtures; insert in reverse so `seq DESC` reproduces that
/// order.
async fn insert_audit(
    entity_id: &str,
    entity_type: &str,
    entries: &[ChangeLogEntry],
) -> Result<(), sqlx::Error> {
    for e in entries.iter().rev() {
        sqlx::query(
            "INSERT INTO audit_log (id, entity_type, entity_id, actor, field, old_value, new_value, at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&e.id)
        .bind(entity_type)
        .bind(entity_id)
        .bind(&e.actor)
        .bind(&e.field)
        .bind(&e.old_value)
        .bind(&e.new_value)
        .bind(&e.at)
        .execute(pool())
        .await?;
    }
    Ok(())
}

async fn insert_assignments(u: &User) -> Result<(), sqlx::Error> {
    for a in &u.assigned_cases {
        for cap in &a.capabilities {
            sqlx::query(
                "INSERT INTO case_assignments (user_id, case_id, capability) VALUES ($1, $2, $3)
                 ON CONFLICT DO NOTHING",
            )
            .bind(&u.id)
            .bind(&a.case_id)
            .bind(cap.slug())
            .execute(pool())
            .await?;
        }
    }
    Ok(())
}
