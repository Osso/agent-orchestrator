use llm_tasks::db::Event;

const INTERNAL_RETRY_RESET_ACTORS: &[&str] = &["runtime", "architect", "reviewer", "merger"];

pub(super) fn count_attempts_since_manual_reset(events: &[Event]) -> u32 {
    let reset_idx = events.iter().rposition(is_manual_retry_reset);
    events
        .iter()
        .skip(reset_idx.map_or(0, |idx| idx + 1))
        .filter(|event| event.action == "claimed")
        .count() as u32
}

fn is_manual_retry_reset(event: &Event) -> bool {
    event.action == "updated"
        && event.field.as_deref() == Some("status")
        && matches!(event.new_value.as_deref(), Some("pending" | "ready"))
        && !is_internal_retry_reset_actor(&event.actor)
}

fn is_internal_retry_reset_actor(actor: &str) -> bool {
    INTERNAL_RETRY_RESET_ACTORS.contains(&actor) || actor.starts_with("task-")
}

#[cfg(test)]
mod tests {
    use super::count_attempts_since_manual_reset;
    use llm_tasks::db::Event;

    fn event(
        id: i64,
        actor: &str,
        action: &str,
        field: Option<&str>,
        old_value: Option<&str>,
        new_value: Option<&str>,
    ) -> Event {
        Event {
            id,
            task_id: "lt-test".to_string(),
            actor: actor.to_string(),
            action: action.to_string(),
            field: field.map(str::to_string),
            old_value: old_value.map(str::to_string),
            new_value: new_value.map(str::to_string),
            timestamp: "2026-03-12T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn manual_rerun_resets_attempt_budget() {
        let events = vec![
            event(
                1,
                "task-lt-test",
                "claimed",
                Some("assignee"),
                None,
                Some("task-lt-test"),
            ),
            event(
                2,
                "task-lt-test",
                "claimed",
                Some("assignee"),
                None,
                Some("task-lt-test"),
            ),
            event(
                3,
                "reviewer",
                "updated",
                Some("status"),
                Some("in_review"),
                Some("ready"),
            ),
            event(
                4,
                "task-lt-test",
                "claimed",
                Some("assignee"),
                None,
                Some("task-lt-test"),
            ),
            event(
                5,
                "viewer",
                "updated",
                Some("status"),
                Some("failed"),
                Some("pending"),
            ),
            event(
                6,
                "task-lt-test",
                "claimed",
                Some("assignee"),
                None,
                Some("task-lt-test"),
            ),
        ];

        assert_eq!(count_attempts_since_manual_reset(&events), 1);
    }

    #[test]
    fn reviewer_ready_transition_does_not_reset_attempt_budget() {
        let events = vec![
            event(
                1,
                "task-lt-test",
                "claimed",
                Some("assignee"),
                None,
                Some("task-lt-test"),
            ),
            event(
                2,
                "reviewer",
                "updated",
                Some("status"),
                Some("in_review"),
                Some("ready"),
            ),
            event(
                3,
                "task-lt-test",
                "claimed",
                Some("assignee"),
                None,
                Some("task-lt-test"),
            ),
        ];

        assert_eq!(count_attempts_since_manual_reset(&events), 2);
    }
}
