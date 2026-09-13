//! Parsing the reflector worker's output: `===`-separated note segments in
//! the distiller's lenient `tags\n---\nbody` shape, plus an optional
//! `===PERSONA===` block with WHY:/HOW: metadata and a revised persona.

use crate::util::parse_tag_line;

/// Markers for the optional persona block in the reflector's output.
const PERSONA_OPEN: &str = "===PERSONA===";
const PERSONA_CLOSE: &str = "===END===";
/// One note parsed from the reflector worker's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReflectedNote {
    pub(super) tags: Vec<String>,
    pub(super) body: String,
}

/// An optional persona revision the reflector proposed (M4.5). The agent may
/// nudge its own character during reflection; the change and its reasoning are
/// always recorded in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PersonaRevision {
    pub(super) why: String,
    pub(super) how: String,
    pub(super) persona: String,
}

/// The parsed reflector output: durable notes plus an optional persona nudge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Reflection {
    pub(super) notes: Vec<ReflectedNote>,
    pub(super) persona: Option<PersonaRevision>,
}
/// Split the reflector's output into durable notes plus the optional persona
/// block (the module docs describe the shapes).
pub(super) fn parse_reflection(text: &str) -> Reflection {
    let mut reflection = Reflection::default();
    let mut segment: Vec<&str> = Vec::new();
    let mut in_persona = false;
    let mut persona_lines: Vec<&str> = Vec::new();

    let flush_notes = |lines: &mut Vec<&str>, notes: &mut Vec<ReflectedNote>| {
        let segment = lines.join("\n");
        if let Some(note) = parse_segment(&segment) {
            notes.push(note);
        }
        lines.clear();
    };

    for line in text.lines() {
        if in_persona {
            if line.trim() == PERSONA_CLOSE {
                reflection.persona = parse_persona_block(&persona_lines.join("\n"));
                persona_lines.clear();
                in_persona = false;
            } else {
                persona_lines.push(line);
            }
            continue;
        }
        if line.trim() == PERSONA_OPEN {
            flush_notes(&mut segment, &mut reflection.notes);
            in_persona = true;
            continue;
        }
        if line.trim() == "===" {
            flush_notes(&mut segment, &mut reflection.notes);
        } else {
            segment.push(line);
        }
    }
    // An unterminated persona block is ignored; regular notes still count.
    flush_notes(&mut segment, &mut reflection.notes);
    reflection
}

/// Parse the inside of a persona block: `WHY:`/`HOW:` lines describing the
/// reflection, an optional `---`, then an optional complete revised persona.
///
/// A block with only the reasoning (no revision) is a persona *reflection*: it
/// is recorded in memory but changes nothing. Returns `None` only when the
/// block holds nothing at all.
fn parse_persona_block(block: &str) -> Option<PersonaRevision> {
    let (meta, persona) = match block.split_once("\n---") {
        Some((meta, persona)) => (meta, persona.trim().to_owned()),
        None => (block, String::new()),
    };
    let mut why = String::new();
    let mut how = String::new();
    for line in meta.lines() {
        let line = line.trim();
        if let Some(value) = line
            .strip_prefix("WHY:")
            .or_else(|| line.strip_prefix("Why:"))
        {
            why = value.trim().to_owned();
        } else if let Some(value) = line
            .strip_prefix("HOW:")
            .or_else(|| line.strip_prefix("How:"))
        {
            how = value.trim().to_owned();
        }
    }
    if why.is_empty() && how.is_empty() && persona.is_empty() {
        return None;
    }
    Some(PersonaRevision { why, how, persona })
}

fn parse_segment(segment: &str) -> Option<ReflectedNote> {
    let mut tags = Vec::new();
    let mut body = String::new();
    let mut in_body = false;
    for line in segment.lines() {
        if !in_body {
            if line.trim() == "---" {
                in_body = true;
                continue;
            }
            tags.push(line);
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    let body = body.trim();
    if !in_body || body.is_empty() {
        let fallback = segment.trim();
        if fallback.is_empty() {
            return None;
        }
        return Some(ReflectedNote {
            tags: Vec::new(),
            body: fallback.to_owned(),
        });
    }
    Some(ReflectedNote {
        tags: parse_tag_line(&tags.join(" ")),
        body: body.to_owned(),
    })
}
