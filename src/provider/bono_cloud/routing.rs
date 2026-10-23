//! What Bono Cloud's router (Jev) is told about a turn, besides the conversation: which driver
//! question it is (`routing_turn`) and which default commands it may answer it with, documented
//! from the skill actually installed (`routing_context`). Jev chooses at most one argument-free
//! `engineer` call for a turn's first request, or leaves the turn to Claude.
//!
//! The website rejects (`400`) anything outside the limits below before any inference, and never
//! cuts a context short: [`RoutingContext::validate`] checks them the same way (lengths in UTF-16
//! code units, like JavaScript's), and a context that fails isn't sent at all.

use serde::Serialize;
use std::{collections::BTreeSet, fmt};

/// The only version of either field there is
pub const VERSION: u32 = 1;
/// The commands Jev may choose: each runs with no arguments
pub const SUPPORTED_COMMANDS: [&str; 6] = ["quick", "status", "stint", "field", "track", "corners"];

/// The most the whole context may take, serialized
pub const MAX_CONTEXT: usize = 24_000;
pub const MAX_SESSION: usize = 4_000;
pub const MAX_SKILLS: usize = 6;
pub const MAX_DESCRIPTION: usize = 1_000;
pub const MAX_INSTRUCTIONS: usize = 8_000;
pub const MAX_REFERENCES: usize = 4;
pub const MAX_PATH: usize = 500;
pub const MAX_REFERENCE: usize = 4_000;
/// The longest turn id
const MAX_TURN_ID: usize = 128;

/// The `routing_context` of a request: the commands Jev may choose from, and what it should know
/// of the session to choose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoutingContext {
    pub version: u32,
    /// The session's state, as far as choosing a command goes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub skills: Vec<RoutingSkill>,
}

/// One command Jev may choose, as the skill documents it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoutingSkill {
    pub command: String,
    pub description: String,
    pub instructions: String,
    /// Always sent, empty when there are none
    pub references: Vec<RoutingReference>,
}

/// A reference document's text, sent inline: the website reads nothing from this PC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoutingReference {
    /// Only a label
    pub path: String,
    pub content: String,
}

/// Why a context can't be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalid {
    /// A field is empty, or longer than it may be
    Length {
        field: String,
        length: usize,
        max: usize,
    },
    NoSkills,
    TooManySkills(usize),
    Unsupported(String),
    Duplicate(String),
    TooManyReferences {
        command: String,
        count: usize,
    },
    /// The whole context, serialized
    TooLarge(usize),
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length { field, length, max } => {
                write!(f, "{field} is {length} characters long (1 to {max})")
            }
            Self::NoSkills => write!(f, "no commands"),
            Self::TooManySkills(count) => write!(f, "{count} commands (at most {MAX_SKILLS})"),
            Self::Unsupported(command) => write!(f, "unsupported command {command:?}"),
            Self::Duplicate(command) => write!(f, "command {command:?} listed twice"),
            Self::TooManyReferences { command, count } => {
                write!(
                    f,
                    "{command} has {count} references (at most {MAX_REFERENCES})"
                )
            }
            Self::TooLarge(size) => {
                write!(f, "{size} characters serialized (at most {MAX_CONTEXT})")
            }
        }
    }
}

impl std::error::Error for Invalid {}

/// The length JavaScript gives `text`: UTF-16 code units.
#[inline]
pub fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn check(field: impl FnOnce() -> String, text: &str, max: usize) -> Result<(), Invalid> {
    let length = js_len(text);
    match (1..=max).contains(&length) && !text.trim().is_empty() {
        true => Ok(()),
        false => Err(Invalid::Length {
            field: field(),
            length,
            max,
        }),
    }
}

impl RoutingContext {
    pub fn new(session: Option<String>, skills: Vec<RoutingSkill>) -> Self {
        Self {
            version: VERSION,
            session,
            skills,
        }
    }

    /// Its length serialized, as the website measures it.
    pub fn serialized_len(&self) -> usize {
        serde_json::to_string(self).map_or(usize::MAX, |json| js_len(&json))
    }

    /// Whether the website would take it, as it says.
    pub fn validate(&self) -> Result<(), Invalid> {
        if self.version != VERSION {
            return Err(Invalid::Unsupported(format!("version {}", self.version)));
        }
        if let Some(session) = &self.session {
            check(|| "session".into(), session, MAX_SESSION)?;
        }
        match self.skills.len() {
            0 => return Err(Invalid::NoSkills),
            count if count > MAX_SKILLS => return Err(Invalid::TooManySkills(count)),
            _ => {}
        }
        let mut seen = BTreeSet::new();
        for skill in &self.skills {
            let command = &skill.command;
            if !SUPPORTED_COMMANDS.contains(&command.as_str()) {
                return Err(Invalid::Unsupported(command.clone()));
            }
            if !seen.insert(command.as_str()) {
                return Err(Invalid::Duplicate(command.clone()));
            }
            check(
                || format!("{command}.description"),
                &skill.description,
                MAX_DESCRIPTION,
            )?;
            check(
                || format!("{command}.instructions"),
                &skill.instructions,
                MAX_INSTRUCTIONS,
            )?;
            if skill.references.len() > MAX_REFERENCES {
                return Err(Invalid::TooManyReferences {
                    command: command.clone(),
                    count: skill.references.len(),
                });
            }
            for (index, reference) in skill.references.iter().enumerate() {
                check(
                    || format!("{command}.references[{index}].path"),
                    &reference.path,
                    MAX_PATH,
                )?;
                check(
                    || format!("{command}.references[{index}].content"),
                    &reference.content,
                    MAX_REFERENCE,
                )?;
            }
        }
        match self.serialized_len() {
            size if size > MAX_CONTEXT => Err(Invalid::TooLarge(size)),
            _ => Ok(()),
        }
    }
}

/// Which request of a driver turn this is: its first, or one carrying on after tool results (or
/// after a first request whose answer was lost).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Start,
    Continue,
}

/// The `routing_turn` of a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoutingTurn<'a> {
    pub version: u32,
    pub id: &'a str,
    pub phase: Phase,
}

/// A new driver turn's id: the website routes each one at most once.
pub fn turn_id() -> String {
    let id = format!("turn-{:032x}", rand::random::<u128>());
    debug_assert!(valid_turn_id(&id));
    id
}

/// 1 to 128 ASCII letters, digits, underscores or hyphens.
pub fn valid_turn_id(id: &str) -> bool {
    (1..=MAX_TURN_ID).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(command: &str) -> RoutingSkill {
        RoutingSkill {
            command: command.into(),
            description: "Does it.".into(),
            instructions: "Use it.".into(),
            references: Vec::new(),
        }
    }

    fn context(skills: Vec<RoutingSkill>) -> RoutingContext {
        RoutingContext::new(Some("Race session.".into()), skills)
    }

    #[test]
    fn takes_every_supported_command_once() {
        let all = SUPPORTED_COMMANDS.iter().map(|c| skill(c)).collect();
        assert_eq!(context(all).validate(), Ok(()));
        assert_eq!(
            context(vec![skill("stint"), skill("stint")]).validate(),
            Err(Invalid::Duplicate("stint".into()))
        );
        for command in ["laps", "sql", "nearby", "Quick"] {
            assert_eq!(
                context(vec![skill(command)]).validate(),
                Err(Invalid::Unsupported(command.into()))
            );
        }
        assert_eq!(context(Vec::new()).validate(), Err(Invalid::NoSkills));
    }

    #[test]
    fn serializes_as_the_website_expects() {
        let mut stint = skill("stint");
        stint.references.push(RoutingReference {
            path: "references/strategy.md".into(),
            content: "Verdict first.".into(),
        });
        let value = serde_json::to_value(context(vec![stint, skill("quick")])).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "version": 1,
                "session": "Race session.",
                "skills": [
                    {"command": "stint", "description": "Does it.", "instructions": "Use it.",
                     "references": [{"path": "references/strategy.md", "content": "Verdict first."}]},
                    {"command": "quick", "description": "Does it.", "instructions": "Use it.",
                     "references": []},
                ],
            })
        );
        let turn = RoutingTurn {
            version: VERSION,
            id: "abc",
            phase: Phase::Continue,
        };
        assert_eq!(
            serde_json::to_value(turn).unwrap(),
            serde_json::json!({"version": 1, "id": "abc", "phase": "continue"})
        );
    }

    #[test]
    fn measures_lengths_in_utf16_code_units() {
        // One code unit each, but three bytes
        let mut ok = skill("quick");
        ok.description = "€".repeat(MAX_DESCRIPTION);
        assert!(ok.description.len() > MAX_DESCRIPTION);
        assert_eq!(context(vec![ok.clone()]).validate(), Ok(()));
        // Two code units each
        let mut long = skill("quick");
        long.description = "🏁".repeat(MAX_DESCRIPTION / 2 + 1);
        assert!(long.description.chars().count() <= MAX_DESCRIPTION);
        assert!(matches!(
            context(vec![long]).validate(),
            Err(Invalid::Length { length: 1002, .. })
        ));
    }

    #[test]
    fn checks_every_field() {
        let cases: [(fn(&mut RoutingContext), &str); 7] = [
            (|c| c.session = Some(String::new()), "session"),
            (|c| c.session = Some("x".repeat(MAX_SESSION + 1)), "session"),
            (
                |c| c.skills[0].description = "  ".into(),
                "quick.description",
            ),
            (
                |c| c.skills[0].instructions = "x".repeat(MAX_INSTRUCTIONS + 1),
                "quick.instructions",
            ),
            (
                |c| {
                    c.skills[0].references.push(RoutingReference {
                        path: "references/a.md".into(),
                        content: String::new(),
                    })
                },
                "quick.references[0].content",
            ),
            (
                |c| {
                    c.skills[0].references.push(RoutingReference {
                        path: String::new(),
                        content: "text".into(),
                    })
                },
                "quick.references[0].path",
            ),
            (
                |c| {
                    c.skills[0].references.push(RoutingReference {
                        path: "references/a.md".into(),
                        content: "x".repeat(MAX_REFERENCE + 1),
                    })
                },
                "quick.references[0].content",
            ),
        ];
        for (change, expected) in cases {
            let mut context = context(vec![skill("quick")]);
            change(&mut context);
            match context.validate() {
                Err(Invalid::Length { field, .. }) => assert_eq!(field, expected),
                other => panic!("{expected}: {other:?}"),
            }
        }

        let mut many = context(vec![skill("quick")]);
        many.skills[0].references = (0..5)
            .map(|i| RoutingReference {
                path: format!("references/{i}.md"),
                content: "text".into(),
            })
            .collect();
        assert!(matches!(
            many.validate(),
            Err(Invalid::TooManyReferences { count: 5, .. })
        ));
        let mut seven = context(SUPPORTED_COMMANDS.iter().map(|c| skill(c)).collect());
        seven.skills.push(skill("quick"));
        assert_eq!(seven.validate(), Err(Invalid::TooManySkills(7)));
    }

    #[test]
    fn counts_json_escaping_in_the_serialized_size() {
        // Within every field's limit, but quotes and newlines double once escaped
        let skills = SUPPORTED_COMMANDS
            .iter()
            .map(|command| {
                let mut skill = skill(command);
                skill.instructions = "\"\n".repeat(1_800);
                skill
            })
            .collect();
        let context = context(skills);
        let raw: usize = context.skills.iter().map(|s| js_len(&s.instructions)).sum();
        assert!(raw < MAX_CONTEXT);
        assert!(matches!(context.validate(), Err(Invalid::TooLarge(_))));
    }

    #[test]
    fn turn_ids_are_unique_and_valid() {
        let (a, b) = (turn_id(), turn_id());
        assert_ne!(a, b);
        assert!(valid_turn_id(&a));
        assert!(!valid_turn_id(""));
        assert!(!valid_turn_id("has space"));
        assert!(!valid_turn_id(&"a".repeat(129)));
    }
}
