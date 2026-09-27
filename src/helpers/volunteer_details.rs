//! The volunteer's own details: skills, role, date of birth, contact, emergency
//! contact, the required Social Security Number, and the electronic signature.
//!
//! Pure functions, so one implementation serves both the browser form and the
//! server function. The SSN is write-only from the browser's side: the read
//! type [`VolunteerDetailsView`] carries a `has_ssn` flag instead.

use serde::{Deserialize, Serialize};

use crate::helpers::dates;

/// The earliest date of birth accepted. Anything before this is a typo rather
/// than a person.
const EARLIEST_BIRTH_YEAR: i32 = 1900;

/// How many digits a US phone number has, and an SSN.
const PHONE_DIGITS: usize = 10;
const SSN_DIGITS: usize = 9;

/// The consent the volunteer gives by filling the contact block in, shown
/// directly above those fields wherever they are edited.
pub const BACKGROUND_CHECK_CONSENT: &str = "By providing the information below, Volunteer consents to the Foundation performing a background check.";

/// What a volunteer submits about themselves. Every field is a string because
/// this is form input; [`VolunteerDetails::normalized`] is what turns it into
/// storable values.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VolunteerDetails {
    pub skills_focus: String,
    /// The role the volunteer is applying for, beside their skills.
    #[serde(default)]
    pub volunteer_role: String,
    /// ISO `YYYY-MM-DD`, as an `<input type="date">` produces. Displayed as
    /// `MM-DD-YYYY` by [`format_dob`].
    pub date_of_birth: String,
    /// Required when signing the agreement. Digits only after normalization; on
    /// the profile edit form an empty value means "keep the number on file".
    pub ssn: String,
    pub phone: String,
    pub emergency_first_name: String,
    pub emergency_last_name: String,
    pub emergency_relationship: String,
    pub emergency_phone: String,
    /// The volunteer's full legal name, as printed on the agreement.
    #[serde(default)]
    pub legal_name: String,
    /// The full legal name typed into the electronic-signature field.
    #[serde(default)]
    pub signature_name: String,
    /// Whether a parent or legal guardian is signing for a minor.
    #[serde(default)]
    pub signer_is_guardian: bool,
    #[serde(default)]
    pub guardian_name: String,
    #[serde(default)]
    pub guardian_relationship: String,
    #[serde(default)]
    pub guardian_email: String,
    /// Whether the electronic consent box was clicked.
    #[serde(default)]
    pub electronic_consent: bool,
}

impl VolunteerDetails {
    /// Trim everything, format both phones, and reduce the SSN to bare digits.
    /// Always run before [`Self::validate`] and before storing.
    pub fn normalized(&self) -> Self {
        Self {
            skills_focus: self.skills_focus.trim().to_string(),
            volunteer_role: self.volunteer_role.trim().to_string(),
            date_of_birth: self.date_of_birth.trim().to_string(),
            ssn: digits(&self.ssn),
            phone: normalize_phone(&self.phone),
            emergency_first_name: self.emergency_first_name.trim().to_string(),
            emergency_last_name: self.emergency_last_name.trim().to_string(),
            emergency_relationship: self.emergency_relationship.trim().to_string(),
            emergency_phone: normalize_phone(&self.emergency_phone),
            legal_name: self.legal_name.trim().to_string(),
            signature_name: self.signature_name.trim().to_string(),
            signer_is_guardian: self.signer_is_guardian,
            guardian_name: self.guardian_name.trim().to_string(),
            guardian_relationship: self.guardian_relationship.trim().to_string(),
            guardian_email: self.guardian_email.trim().to_string(),
            electronic_consent: self.electronic_consent,
        }
    }

    /// Whether these details are complete and well-formed when a number is
    /// already stored (`ssn_on_file`), in which case a blank SSN means "keep it".
    /// The error is shown to the volunteer as-is. Age is not gated — paragraph 13
    /// allows minors, who sign through a parent or guardian instead.
    pub fn validate_with(&self, ssn_on_file: bool) -> Result<(), String> {
        if self.skills_focus.trim().is_empty() {
            return Err("Please describe your skills and area of focus.".to_string());
        }
        if self.volunteer_role.trim().is_empty() {
            return Err("Please give the role you are volunteering for.".to_string());
        }
        validate_date_of_birth(self.date_of_birth.trim())?;
        self.validate_ssn(ssn_on_file)?;
        if format_phone(&self.phone).is_none() {
            return Err(
                "Enter your phone number as 10 digits, for example (555) 123-4567.".to_string(),
            );
        }
        if self.emergency_first_name.trim().is_empty() || self.emergency_last_name.trim().is_empty()
        {
            return Err("Please give your emergency contact's first and last name.".to_string());
        }
        if format_phone(&self.emergency_phone).is_none() {
            return Err(
                "Enter your emergency contact's phone number as 10 digits, for example (555) 123-4567."
                    .to_string(),
            );
        }
        Ok(())
    }

    /// The strict rules, used when signing: no number on file to fall back on.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_with(false)
    }

    /// The extra rules that apply when signing the agreement: consent, a typed
    /// signature, and a parent or guardian when the volunteer is a minor.
    pub fn validate_signature(&self) -> Result<(), String> {
        if !self.electronic_consent {
            return Err(
                "Click the acknowledgment box to adopt your typed name as your electronic signature."
                    .to_string(),
            );
        }
        if self.legal_name.trim().is_empty() {
            return Err("Please give the volunteer's full legal name.".to_string());
        }
        if self.signature_name.trim().is_empty() {
            return Err("Type your full legal name as your electronic signature.".to_string());
        }
        if !dates::is_minor(&self.date_of_birth) {
            return Ok(());
        }
        if !self.signer_is_guardian {
            return Err(
                "A volunteer under 18 must have a parent or legal guardian sign on their behalf."
                    .to_string(),
            );
        }
        if self.guardian_name.trim().is_empty() {
            return Err("Please give the parent or legal guardian's full legal name.".to_string());
        }
        if self.guardian_relationship.trim().is_empty() {
            return Err("Please give the guardian's relationship to the volunteer.".to_string());
        }
        if !is_email(&self.guardian_email) {
            return Err("Enter the parent or legal guardian's email address.".to_string());
        }
        // A guardian signs in their own name, so the signature must be theirs.
        if self.signature_name.trim() != self.guardian_name.trim() {
            return Err(
                "The electronic signature must be the parent or legal guardian's full legal name."
                    .to_string(),
            );
        }
        Ok(())
    }

    /// The SSN rule on its own: nine digits, or blank only when one is on file.
    fn validate_ssn(&self, ssn_on_file: bool) -> Result<(), String> {
        if self.ssn.trim().is_empty() {
            if ssn_on_file {
                return Ok(());
            }
            return Err("A Social Security Number is required.".to_string());
        }
        if digits(&self.ssn).len() != SSN_DIGITS {
            return Err("A Social Security Number has 9 digits.".to_string());
        }
        Ok(())
    }
}

/// The read side of [`VolunteerDetails`]: identical but for the SSN, which is
/// reduced to a yes/no so the digits have no field to travel in.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VolunteerDetailsView {
    pub skills_focus: String,
    #[serde(default)]
    pub volunteer_role: String,
    /// ISO `YYYY-MM-DD`, or empty for a volunteer who predates this form.
    pub date_of_birth: String,
    /// Whether a Social Security Number is on file. Never the number itself.
    pub has_ssn: bool,
    pub phone: String,
    pub emergency_first_name: String,
    pub emergency_last_name: String,
    pub emergency_relationship: String,
    pub emergency_phone: String,
    #[serde(default)]
    pub legal_name: String,
    #[serde(default)]
    pub signature_name: String,
    #[serde(default)]
    pub signer_is_guardian: bool,
    #[serde(default)]
    pub guardian_name: String,
    #[serde(default)]
    pub guardian_relationship: String,
    #[serde(default)]
    pub guardian_email: String,
    /// Pre-formatted date and time of submission; only ever shown.
    #[serde(default)]
    pub signed_at: String,
    /// Whether the acknowledgment box was ticked at signing.
    #[serde(default)]
    pub electronic_consent: bool,
}

impl VolunteerDetailsView {
    /// Whether this volunteer has filled the details form in at all. False for
    /// rows backfilled by migration and seed.
    pub fn is_present(&self) -> bool {
        !self.skills_focus.is_empty()
            || !self.date_of_birth.is_empty()
            || self.has_ssn
            || !self.emergency_first_name.is_empty()
            || !self.emergency_last_name.is_empty()
            || !self.emergency_phone.is_empty()
    }

    /// Seed an edit form from what is on file. The SSN is necessarily blank, so
    /// an empty box means "keep what is stored" rather than "clear it".
    pub fn to_edit(&self) -> VolunteerDetails {
        VolunteerDetails {
            skills_focus: self.skills_focus.clone(),
            volunteer_role: self.volunteer_role.clone(),
            date_of_birth: self.date_of_birth.clone(),
            ssn: String::new(),
            phone: self.phone.clone(),
            emergency_first_name: self.emergency_first_name.clone(),
            emergency_last_name: self.emergency_last_name.clone(),
            emergency_relationship: self.emergency_relationship.clone(),
            emergency_phone: self.emergency_phone.clone(),
            // The signature block is fixed at the moment of signing and is not
            // editable here; sending it back blank leaves it untouched.
            legal_name: String::new(),
            signature_name: String::new(),
            signer_is_guardian: false,
            guardian_name: String::new(),
            guardian_relationship: String::new(),
            guardian_email: String::new(),
            electronic_consent: false,
        }
    }

    /// The emergency contact's full name, or empty when none is recorded.
    pub fn emergency_full_name(&self) -> String {
        format!("{} {}", self.emergency_first_name, self.emergency_last_name)
            .trim()
            .to_string()
    }
}

/// Just the digits of `value`.
fn digits(value: &str) -> String {
    value.chars().filter(char::is_ascii_digit).collect()
}

/// A crude but sufficient email check: one `@` with something either side and a
/// dot in the domain. The address is only ever a contact point for a guardian.
fn is_email(value: &str) -> bool {
    let value = value.trim();
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

/// A phone number as `(000) 000-0000`, or the trimmed input when it is not ten
/// digits, so validation can quote back what was typed.
fn normalize_phone(value: &str) -> String {
    format_phone(value).unwrap_or_else(|| value.trim().to_string())
}

/// `value` as `(000) 000-0000`, or `None` unless it holds exactly ten digits.
pub fn format_phone(value: &str) -> Option<String> {
    let digits = digits(value);
    if digits.len() != PHONE_DIGITS {
        return None;
    }
    Some(format!(
        "({}) {}-{}",
        &digits[0..3],
        &digits[3..6],
        &digits[6..10]
    ))
}

/// A stored SSN as `000-00-0000`. Only called on the audited reveal path.
pub fn format_ssn(value: &str) -> String {
    let digits = digits(value);
    if digits.len() != SSN_DIGITS {
        return digits;
    }
    format!("{}-{}-{}", &digits[0..3], &digits[3..5], &digits[5..9])
}

/// An ISO `YYYY-MM-DD` date as `MM-DD-YYYY`, the format the Foundation's form
/// uses. Anything unparseable is passed through.
pub fn format_dob(value: &str) -> String {
    dates::to_us(value)
}

/// A date of birth must be a real date, in the past, and not absurdly early.
/// "In the past" is checked against whichever clock is running this code.
fn validate_date_of_birth(value: &str) -> Result<(), String> {
    const MALFORMED: &str = "Enter your date of birth as a valid date.";
    if value.is_empty() {
        return Err("Please give your date of birth.".to_string());
    }
    let (year, _, _) = dates::parse_iso(value).ok_or(MALFORMED)?;
    if year < EARLIEST_BIRTH_YEAR {
        return Err(MALFORMED.to_string());
    }
    if value >= dates::today().as_str() {
        return Err("Your date of birth must be in the past.".to_string());
    }
    Ok(())
}
