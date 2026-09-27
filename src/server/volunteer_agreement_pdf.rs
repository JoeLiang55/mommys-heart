//! Render a volunteer's accepted agreement as a PDF: the wording they accepted,
//! the details they gave with it, and their electronic signature block.
//!
//! Built from the stored record on demand — nothing is archived at signing time.
//! The SSN is never printed, only whether one is on file, matching
//! [`VolunteerDetailsView`].

use printpdf::{
    BuiltinFont, Color, Line, LinePoint, Mm, Op, ParsedFont, PdfDocument, PdfFontHandle, PdfPage,
    PdfSaveOptions, Point, Pt, Rgb, TextItem,
};

use crate::helpers::volunteer_details::{format_dob, VolunteerDetailsView};
use crate::helpers::volunteer_terms::{VOLUNTEER_AGREEMENT_SECTIONS, VOLUNTEER_AGREEMENT_VERSION};
use crate::server_fns::volunteers::VolunteerApplication;

/// US Letter, with a comfortable margin on every side.
const PAGE_WIDTH: Mm = Mm(215.9);
const PAGE_HEIGHT: Mm = Mm(279.4);
const MARGIN: f32 = 20.0;
const FOOTER_TOP: f32 = 14.0;

const BODY_SIZE: f32 = 10.0;
const HEADING_SIZE: f32 = 11.5;
const TITLE_SIZE: f32 = 16.0;
const FOOTER_SIZE: f32 = 8.0;

/// A font plus the parsed copy used to measure text for word wrapping.
struct Face {
    handle: PdfFontHandle,
    parsed: Option<ParsedFont>,
}

impl Face {
    fn new(font: BuiltinFont) -> Self {
        Self {
            handle: PdfFontHandle::Builtin(font),
            parsed: font.get_parsed_font(),
        }
    }

    /// Width of `text` at `size`, in points. Falls back to a rough average when
    /// the built-in font could not be parsed, so layout degrades rather than panics.
    fn width(&self, text: &str, size: f32) -> f32 {
        let Some(parsed) = self.parsed.as_ref() else {
            return text.chars().count() as f32 * size * 0.5;
        };
        let units: u32 = text
            .chars()
            .map(|character| {
                parsed
                    .lookup_glyph_index(character as u32)
                    .and_then(|glyph| parsed.get_glyph_width(glyph))
                    // The built-in fonts do not map the space character, and an
                    // unknown glyph gets a plausible average width.
                    .unwrap_or(if character == ' ' { 569 } else { 1024 }) as u32
            })
            .sum();
        // These fonts are 2048 units per em, so 'H' measures 1479 == 0.722 em.
        units as f32 * size / 2048.0
    }

    /// Break `text` into lines that fit `max_width` points, splitting on spaces.
    fn wrap(&self, text: &str, size: f32, max_width: f32) -> Vec<String> {
        let mut lines = Vec::new();
        let mut current = String::new();
        for word in text.split_whitespace() {
            let candidate = if current.is_empty() {
                word.to_string()
            } else {
                format!("{current} {word}")
            };
            if self.width(&candidate, size) <= max_width || current.is_empty() {
                current = candidate;
            } else {
                lines.push(std::mem::take(&mut current));
                current = word.to_string();
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        lines
    }
}

/// A cursor walking down the page, spilling into a new page when it runs out.
struct Layout {
    regular: Face,
    bold: Face,
    pages: Vec<Vec<Op>>,
    ops: Vec<Op>,
    y: f32,
}

impl Layout {
    fn new() -> Self {
        Self {
            regular: Face::new(BuiltinFont::Helvetica),
            bold: Face::new(BuiltinFont::HelveticaBold),
            pages: Vec::new(),
            ops: Vec::new(),
            y: PAGE_HEIGHT.0 - MARGIN,
        }
    }

    fn content_width(&self) -> f32 {
        Mm(PAGE_WIDTH.0 - 2.0 * MARGIN).into_pt().0
    }

    fn new_page(&mut self) {
        self.pages.push(std::mem::take(&mut self.ops));
        self.y = PAGE_HEIGHT.0 - MARGIN;
    }

    /// Make room for `needed` millimetres, breaking the page if there is none.
    fn reserve(&mut self, needed: f32) {
        if self.y - needed < MARGIN + FOOTER_TOP {
            self.new_page();
        }
    }

    /// Draw one line of text at the cursor and advance past it.
    fn line(&mut self, text: &str, size: f32, bold: bool, indent: f32) {
        let height = Mm::from(Pt(size * 1.35)).0;
        self.reserve(height);
        let font = if bold { &self.bold } else { &self.regular };
        let handle = font.handle.clone();
        self.y -= height;
        self.ops.extend([
            Op::StartTextSection,
            Op::SetTextCursor {
                pos: Point {
                    x: Mm(MARGIN + indent).into(),
                    y: Mm(self.y).into(),
                },
            },
            Op::SetFont { font: handle, size: Pt(size) },
            Op::ShowText {
                items: vec![TextItem::Text(text.to_string())],
            },
            Op::EndTextSection,
        ]);
    }

    /// Word-wrap a paragraph across as many lines and pages as it needs.
    fn paragraph(&mut self, text: &str, size: f32, bold: bool, indent: f32) {
        let width = self.content_width() - Mm(indent).into_pt().0;
        let font = if bold { &self.bold } else { &self.regular };
        for line in font.wrap(text, size, width) {
            self.line(&line, size, bold, indent);
        }
    }

    fn gap(&mut self, millimetres: f32) {
        self.y -= millimetres;
    }

    /// A "Label: value" row, wrapped with the value hanging under the label.
    fn field(&mut self, label: &str, value: &str) {
        let value = if value.trim().is_empty() {
            "Not provided"
        } else {
            value.trim()
        };
        self.paragraph(&format!("{label}: {value}"), BODY_SIZE, false, 4.0);
    }

    fn heading(&mut self, text: &str) {
        self.gap(3.0);
        self.reserve(14.0);
        self.paragraph(text, HEADING_SIZE, true, 0.0);
        self.gap(1.0);
    }

    /// A horizontal rule, used for the signature line.
    fn rule(&mut self, width_mm: f32) {
        self.reserve(4.0);
        self.y -= 2.0;
        let y: Pt = Mm(self.y).into();
        self.ops.extend([
            Op::SetOutlineColor {
                col: Color::Rgb(Rgb::new(0.45, 0.45, 0.45, None)),
            },
            Op::SetOutlineThickness { pt: Pt(0.7) },
            Op::DrawLine {
                line: Line {
                    points: vec![
                        LinePoint {
                            p: Point { x: Mm(MARGIN).into(), y },
                            bezier: false,
                        },
                        LinePoint {
                            p: Point {
                                x: Mm(MARGIN + width_mm).into(),
                                y,
                            },
                            bezier: false,
                        },
                    ],
                    is_closed: false,
                },
            },
        ]);
    }

    /// Close the last page and stamp "Page N of M" plus provenance on each.
    fn finish(mut self, version: &str, generated_at: &str) -> Vec<PdfPage> {
        self.pages.push(std::mem::take(&mut self.ops));
        let total = self.pages.len();
        let handle = self.regular.handle.clone();
        self.pages
            .into_iter()
            .enumerate()
            .map(|(index, mut ops)| {
                let footer = format!(
                    "Volunteer Agreement \u{2014} version {version} \u{2014} generated {generated_at} \u{2014} page {} of {total}",
                    index + 1
                );
                ops.extend([
                    Op::StartTextSection,
                    Op::SetFillColor {
                        col: Color::Rgb(Rgb::new(0.4, 0.4, 0.4, None)),
                    },
                    Op::SetTextCursor {
                        pos: Point {
                            x: Mm(MARGIN).into(),
                            y: Mm(MARGIN * 0.6).into(),
                        },
                    },
                    Op::SetFont {
                        font: handle.clone(),
                        size: Pt(FOOTER_SIZE),
                    },
                    Op::ShowText {
                        items: vec![TextItem::Text(footer)],
                    },
                    Op::EndTextSection,
                ]);
                PdfPage::new(PAGE_WIDTH, PAGE_HEIGHT, ops)
            })
            .collect()
    }
}

/// Render the agreement `volunteer` accepted as PDF bytes.
pub fn render(name: &str, email: &str, volunteer: &VolunteerApplication) -> Vec<u8> {
    let generated_at = chrono::Local::now()
        .format("%m-%d-%Y %I:%M %p")
        .to_string();
    let mut layout = Layout::new();

    layout.paragraph(
        "Mommy\u{2019}s Heart, Inc. \u{2014} Volunteer Agreement",
        TITLE_SIZE,
        true,
        0.0,
    );
    layout.gap(2.0);
    layout.paragraph(
        &format!("Record of acceptance for {name}"),
        BODY_SIZE,
        false,
        0.0,
    );

    layout.heading("Acceptance");
    layout.field("Volunteer", name);
    layout.field("Email", email);
    layout.field("Status", volunteer.status.label());
    layout.field("Accepted on", &volunteer.agreed_at);
    layout.field("Agreement version", &volunteer.agreement_version);
    if !volunteer.decided_at.is_empty() {
        let who = if volunteer.decided_by_name.is_empty() {
            String::new()
        } else {
            format!(" by {}", volunteer.decided_by_name)
        };
        layout.field("Decided", &format!("{}{}", volunteer.decided_at, who));
    }
    if !volunteer.decision_note.is_empty() {
        layout.field("Decision note", &volunteer.decision_note);
    }

    let details = &volunteer.details;
    layout.heading("Volunteer information");
    layout.field("Skills and area of focus", &details.skills_focus);
    layout.field("Volunteer role", &details.volunteer_role);
    layout.field("Date of birth", &format_dob(&details.date_of_birth));
    layout.field("Phone", &details.phone);
    layout.field("Home address", &details.home_address);
    layout.field(
        "Social Security Number",
        if details.has_ssn {
            "On file (not printed)"
        } else {
            "Not provided"
        },
    );
    layout.field("Emergency contact", &details.emergency_full_name());
    layout.field("Relationship to volunteer", &details.emergency_relationship);
    layout.field("Emergency contact phone", &details.emergency_phone);

    signature_block(&mut layout, details);

    layout.heading("Agreement text");
    if volunteer.agreement_version == VOLUNTEER_AGREEMENT_VERSION {
        for section in VOLUNTEER_AGREEMENT_SECTIONS {
            if !section.heading.is_empty() {
                layout.gap(2.0);
                layout.paragraph(section.heading, BODY_SIZE + 0.5, true, 0.0);
            }
            for paragraph in section.paragraphs {
                layout.paragraph(paragraph, BODY_SIZE, false, 0.0);
                layout.gap(1.5);
            }
        }
    } else {
        // Showing today's wording for an older acceptance would misrepresent
        // what was agreed to, exactly as the profile page refuses to.
        layout.paragraph(
            &format!(
                "This acceptance records version {}, which is not the wording this version of the application carries. The text is omitted rather than showing wording the signer never saw.",
                volunteer.agreement_version
            ),
            BODY_SIZE,
            false,
            0.0,
        );
    }

    let pages = layout.finish(&volunteer.agreement_version, &generated_at);
    let mut document = PdfDocument::new("Volunteer Agreement");
    document
        .with_pages(pages)
        .save(&PdfSaveOptions::default(), &mut Vec::new())
}

/// The electronic signature the agreement was accepted with.
fn signature_block(layout: &mut Layout, details: &VolunteerDetailsView) {
    layout.heading("Electronic signature");
    layout.paragraph(
        "The signer typed the name below and acknowledged that it is their electronic signature, with the same effect as a handwritten one.",
        BODY_SIZE,
        false,
        0.0,
    );
    layout.gap(2.0);
    layout.field("Volunteer\u{2019}s full legal name", &details.legal_name);
    if details.signer_is_guardian {
        layout.field("Signed by parent or legal guardian", &details.guardian_name);
        layout.field("Guardian\u{2019}s relationship", &details.guardian_relationship);
        layout.field("Guardian\u{2019}s email", &details.guardian_email);
    }
    layout.gap(4.0);
    layout.line(
        if details.signature_name.trim().is_empty() {
            "Not provided"
        } else {
            details.signature_name.trim()
        },
        13.0,
        false,
        0.0,
    );
    layout.rule(90.0);
    layout.line("Electronic signature", FOOTER_SIZE, false, 0.0);
    layout.field("Signed on", &details.signed_at);
}
