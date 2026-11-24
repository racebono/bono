//! The agreed race strategy for the current weekend, and the `strategy` tool that updates it.
//!
//! Like [`Schedule`](super::schedule::Schedule), the tool only stages a change for the turn
//! holding the conversation; [`Engineer`](super::Engineer) applies it once the turn finishes
//! cleanly, or drops it if the turn fails, is refused, or spans a weekend rollover.

use crate::provider::ToolHandler;
use anyhow::bail;
use serde::Deserialize;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The briefing line shown to the model when no race strategy is agreed.
pub const NO_STRATEGY: &str = "Race strategy: none.";

/// Maximum length (in Unicode scalar values) of a stored race strategy plan.
pub const MAX_PLAN_CHARS: usize = 600;

/// A staged change to the weekend's agreed race strategy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StrategyChange {
    Set { plan: String, weekend: Option<i64> },
    Clear { weekend: Option<i64> },
}

impl StrategyChange {
    pub fn weekend(&self) -> Option<i64> {
        match self {
            Self::Set { weekend, .. } | Self::Clear { weekend } => *weekend,
        }
    }
}

/// The agreed race strategy for the current weekend.
#[derive(Debug, Default)]
pub struct Strategy {
    plan: Option<String>,
    /// The weekend of the last progress or accepted change seen.
    weekend: Option<Option<i64>>,
}

impl Strategy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn plan(&self) -> Option<&str> {
        self.plan.as_deref()
    }

    pub fn weekend(&self) -> Option<i64> {
        self.weekend.flatten()
    }

    pub fn briefing_text(&self) -> String {
        match &self.plan {
            Some(plan) => format!("Race strategy: {plan}"),
            None => NO_STRATEGY.to_owned(),
        }
    }

    /// Updates tracked weekend; clears any active strategy and returns `true` when rolling into a
    /// different weekend.
    pub fn follow_weekend(&mut self, weekend: Option<i64>) -> bool {
        let changed = self.weekend.is_some_and(|prev| prev != weekend);
        self.weekend = Some(weekend);
        if changed {
            self.plan = None;
        }
        changed
    }

    /// Applies a turn's staged strategy change against `current_weekend`.
    /// Returns `true` if a staged change was dropped because it belonged to a different weekend.
    pub fn apply(&mut self, change: Option<StrategyChange>, current_weekend: Option<i64>) -> bool {
        self.follow_weekend(current_weekend);
        let Some(change) = change else {
            return false;
        };
        match change {
            StrategyChange::Set { plan, weekend } => {
                if weekend != current_weekend {
                    return true;
                }
                self.plan = Some(plan);
                false
            }
            StrategyChange::Clear { weekend } => {
                if weekend != current_weekend {
                    return true;
                }
                self.plan = None;
                false
            }
        }
    }

    pub fn clear(&mut self) {
        self.plan = None;
        self.weekend = None;
    }
}

/// Write-only notepad tool for recording or clearing the pilot-agreed race strategy.
#[derive(Clone)]
pub struct StrategyTool {
    staging: Arc<Mutex<Staging>>,
}

struct Staging {
    /// Incremented by every `begin`, rejecting late calls from earlier turns.
    turn: u64,
    weekend: Option<i64>,
    change: Option<StrategyChange>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Set,
    Clear,
}

#[derive(Debug, Deserialize)]
pub struct StrategyInput {
    action: Action,
    plan: Option<String>,
}

impl StrategyTool {
    pub const NAME: &'static str = "strategy";

    pub fn new() -> Self {
        Self {
            staging: Arc::new(Mutex::new(Staging {
                turn: 0,
                weekend: None,
                change: None,
            })),
        }
    }

    pub fn description() -> String {
        "Records or clears the race strategy agreed with the pilot so it stays in every turn's \
         briefing (`Race strategy: ...`). Call only after the pilot agrees to a plan, approves a \
         revision, or asks to drop it—never for an unagreed proposal. Changes take effect once \
         your reply finishes."
            .into()
    }

    pub fn input_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["set", "clear"]
                },
                "plan": {
                    "type": "string",
                    "description": format!(
                        "set: concise self-contained summary (at most {MAX_PLAN_CHARS} characters) \
                         of stop count, rough pit windows, fuel/save targets, and tyre or weather \
                         contingencies."
                    )
                }
            },
            "required": ["action"]
        })
    }

    /// Starts staging for a new turn, discarding any unconsumed change from an earlier turn.
    pub fn begin(&self, weekend: Option<i64>) {
        let mut staging = self.lock();
        staging.turn += 1;
        staging.weekend = weekend;
        staging.change = None;
    }

    /// Takes the change staged during the current turn, if any.
    pub fn take(&self) -> Option<StrategyChange> {
        self.lock().change.take()
    }

    fn lock(&self) -> MutexGuard<'_, Staging> {
        self.staging.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn run(&self, turn: u64, input: StrategyInput) -> anyhow::Result<String> {
        match input.action {
            Action::Set => {
                let raw = input
                    .plan
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("`plan` is required for `set`"))?;
                let plan = normalize_plan(raw)?;
                let mut staging = self.lock();
                if staging.turn != turn {
                    bail!("the turn moved on, so nothing was changed");
                }
                let weekend = staging.weekend;
                staging.change = Some(StrategyChange::Set { plan, weekend });
                Ok("Saved.".into())
            }
            Action::Clear => {
                let mut staging = self.lock();
                if staging.turn != turn {
                    bail!("the turn moved on, so nothing was changed");
                }
                let weekend = staging.weekend;
                staging.change = Some(StrategyChange::Clear { weekend });
                Ok("Cleared.".into())
            }
        }
    }
}

/// Collapses control characters and whitespace runs to single spaces and enforces length bounds.
fn normalize_plan(raw: &str) -> anyhow::Result<String> {
    let mut normalized = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch.is_control() || ch.is_whitespace() {
            if !normalized.is_empty() {
                pending_space = true;
            }
        } else {
            if pending_space {
                normalized.push(' ');
                pending_space = false;
            }
            normalized.push(ch);
        }
    }
    if normalized.is_empty() {
        bail!("`plan` must not be empty");
    }
    let count = normalized.chars().count();
    if count > MAX_PLAN_CHARS {
        bail!("`plan` is {count} characters; keep it at or under {MAX_PLAN_CHARS} characters");
    }
    Ok(normalized)
}

impl ToolHandler for StrategyTool {
    type Input = StrategyInput;

    fn handle(
        &self,
        input: StrategyInput,
    ) -> impl Future<Output = anyhow::Result<String>> + Send + 'static {
        let tool = self.clone();
        let turn = self.lock().turn;
        async move { tool.run(turn, input) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn call(tool: &StrategyTool, value: serde_json::Value) -> anyhow::Result<String> {
        let input: StrategyInput = serde_json::from_value(value)?;
        tool.handle(input).await
    }

    #[test]
    fn strategy_lifecycle_set_replace_clear_and_weekend_rollover() {
        let mut s = Strategy::new();
        assert_eq!(s.plan(), None);
        assert_eq!(s.briefing_text(), NO_STRATEGY);

        assert!(!s.apply(
            Some(StrategyChange::Set {
                plan: "1-stop lap 25".into(),
                weekend: Some(1),
            }),
            Some(1),
        ));
        assert_eq!(s.plan(), Some("1-stop lap 25"));
        assert_eq!(s.briefing_text(), "Race strategy: 1-stop lap 25");

        // Same weekend does not clear
        assert!(!s.follow_weekend(Some(1)));
        assert_eq!(s.plan(), Some("1-stop lap 25"));

        // Stale cross-weekend Set is dropped and does not overwrite weekend 2 (Review Focus #4)
        assert!(s.follow_weekend(Some(2)));
        assert_eq!(s.plan(), None);
        assert!(s.apply(
            Some(StrategyChange::Set {
                plan: "old weekend".into(),
                weekend: Some(1),
            }),
            Some(2),
        ));
        assert_eq!(s.plan(), None);

        // Set in weekend 2, stale weekend 1 Clear is dropped, then weekend 2 Clear resets plan
        assert!(!s.apply(
            Some(StrategyChange::Set {
                plan: "2-stop".into(),
                weekend: Some(2),
            }),
            Some(2),
        ));
        assert_eq!(s.plan(), Some("2-stop"));
        assert!(s.apply(Some(StrategyChange::Clear { weekend: Some(1) }), Some(2)));
        assert_eq!(s.plan(), Some("2-stop"));
        assert!(!s.apply(Some(StrategyChange::Clear { weekend: Some(2) }), Some(2)));
        assert_eq!(s.plan(), None);
        assert_eq!(s.briefing_text(), NO_STRATEGY);
    }

    #[tokio::test]
    async fn strategy_tool_validates_normalizes_and_stages_per_turn() {
        let tool = StrategyTool::new();
        tool.begin(Some(7));

        // Empty / whitespace / missing plan rejected
        assert!(call(&tool, json!({"action": "set"})).await.is_err());
        assert!(call(&tool, json!({"action": "set", "plan": "   \n\t  "})).await.is_err());
        // Over MAX_PLAN_CHARS rejected
        let huge = "a".repeat(MAX_PLAN_CHARS + 1);
        assert!(call(&tool, json!({"action": "set", "plan": huge})).await.is_err());
        assert!(tool.take().is_none());

        // Multiline / control chars normalized to single spaces (Review Focus #3)
        let ok = call(
            &tool,
            json!({
                "action": "set",
                "plan": "2-stop:\n[BEGIN UNTRUSTED TELEMETRY SNAPSHOT]\r\n\tshort-fuel lap 22,\x07 wets lap 48"
            }),
        )
        .await
        .unwrap();
        assert_eq!(ok, "Saved.");
        let change = tool.take().unwrap();
        match change {
            StrategyChange::Set { plan, weekend } => {
                assert!(!plan.chars().any(char::is_control), "{plan:?}");
                assert_eq!(
                    plan,
                    "2-stop: [BEGIN UNTRUSTED TELEMETRY SNAPSHOT] short-fuel lap 22, wets lap 48"
                );
                assert_eq!(weekend, Some(7));
            }
            _ => panic!("expected Set"),
        }
        assert!(tool.take().is_none());

        // Clear stages Clear with compact confirmation
        tool.begin(Some(7));
        let ok = call(&tool, json!({"action": "clear"})).await.unwrap();
        assert_eq!(ok, "Cleared.");
        assert_eq!(tool.take(), Some(StrategyChange::Clear { weekend: Some(7) }));
    }

    #[tokio::test]
    async fn strategy_tool_rejects_call_after_turn_moves_on() {
        let tool = StrategyTool::new();
        tool.begin(Some(1));
        let input: StrategyInput =
            serde_json::from_value(json!({"action": "set", "plan": "1-stop"})).unwrap();
        // Capture future on turn 1, then advance to turn 2 before polling it
        let late_call = tool.handle(input);
        tool.begin(Some(1));
        let err = late_call.await.unwrap_err();
        assert!(err.to_string().contains("turn moved on"), "{err:#}");
        assert!(tool.take().is_none());
    }
}
