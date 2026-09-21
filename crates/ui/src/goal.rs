//! Compact public goal state. Model-turn activity and goal completion are independent.
use crate::theme::Theme;
use gpui::{IntoElement, SharedString, div, prelude::*, px};
use kratos_proto::{GoalPhase, GoalState};

pub(crate) fn is_control(goal_control: bool, prompt: &str) -> bool {
    goal_control && kratos_proto::goal_control_command(prompt).is_some()
}

fn label(phase: GoalPhase) -> Option<&'static str> {
    match phase {
        GoalPhase::Active => Some("Active"),
        GoalPhase::Checking => Some("Checking"),
        GoalPhase::Paused => Some("Paused"),
        GoalPhase::Blocked => Some("Blocked"),
        GoalPhase::Complete => Some("Complete"),
        GoalPhase::Cleared => None,
    }
}

fn details(goal: &GoalState) -> String {
    let mut text = goal.objective.clone();
    if matches!(goal.phase, GoalPhase::Paused | GoalPhase::Blocked)
        && let Some(reason) = goal.reason.as_deref().filter(|s| !s.trim().is_empty())
    {
        text.push_str("\n\n");
        text.push_str(reason);
    }
    text.push_str(
        "\n\n/goal show · /goal pause · /goal resume\n/goal edit <objective> · /goal clear",
    );
    text
}

pub(crate) fn render(goal: &GoalState, theme: &Theme) -> Option<gpui::AnyElement> {
    let label = label(goal.phase)?;
    let color = match goal.phase {
        GoalPhase::Paused | GoalPhase::Blocked => theme.warning,
        GoalPhase::Active | GoalPhase::Checking => theme.text,
        _ => theme.text_muted,
    };
    let detail = details(goal);
    Some(
        div()
            .id("goal-status")
            .flex()
            .min_w_0()
            .items_center()
            .gap(px(6.0))
            .text_color(color)
            .child(div().flex_none().child(format!("Goal · {label}")))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.text_muted)
                    .child(SharedString::from(goal.objective.clone())),
            )
            .tooltip(move |_, cx| cx.new(|_| GoalDetails(detail.clone())).into())
            .into_any_element(),
    )
}

struct GoalDetails(String);
impl gpui::Render for GoalDetails {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        crate::popover::popover_card(theme)
            .w(px(340.0))
            .p(px(12.0))
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text)
            .child(SharedString::from(self.0.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_phases_are_distinct_and_cleared_is_hidden() {
        for (phase, expected) in [
            (GoalPhase::Active, "Active"),
            (GoalPhase::Checking, "Checking"),
            (GoalPhase::Paused, "Paused"),
            (GoalPhase::Blocked, "Blocked"),
            (GoalPhase::Complete, "Complete"),
        ] {
            assert_eq!(label(phase), Some(expected));
        }
        assert_eq!(label(GoalPhase::Cleared), None);
    }

    #[test]
    fn only_public_pause_and_block_reasons_are_shown() {
        let mut goal = GoalState {
            id: "goal".into(),
            objective: "Ship the fix".into(),
            phase: GoalPhase::Paused,
            reason: Some("Waiting for your decision".into()),
            completion: None,
        };
        assert!(details(&goal).contains("Ship the fix\n\nWaiting for your decision"));
        goal.phase = GoalPhase::Blocked;
        assert!(details(&goal).contains("Waiting for your decision"));
        goal.phase = GoalPhase::Checking;
        assert!(!details(&goal).contains("Waiting for your decision"));
    }

    #[test]
    fn only_negotiated_controls_bypass_queue_and_start_and_resume_remain_prompts() {
        for prompt in [
            "/goal",
            "/goal show",
            "/goal pause",
            "/goal clear",
            "/goal edit ship it",
        ] {
            assert!(is_control(true, prompt));
            assert!(!is_control(false, prompt));
        }
        for prompt in [
            "/goal ship it",
            "/goal resume",
            "/goal resume continue",
            "/goalkeeper",
        ] {
            assert!(!is_control(true, prompt));
        }
    }
}
