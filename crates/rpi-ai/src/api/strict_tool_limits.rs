use std::sync::{LazyLock, Mutex};

const MAX_SESSIONS: usize = 10_000;
const ERROR_MESSAGES: [&str; 3] = [
    "The compiled grammar is too large",
    "Too many strict tools (",
    "Schemas contains too many parameters with union types (",
];

pub fn is_strict_tool_limit_error_message(message: &str) -> bool {
    ERROR_MESSAGES
        .iter()
        .any(|candidate| message.contains(candidate))
}

struct StrictLimitSessions(Mutex<Vec<String>>);

impl StrictLimitSessions {
    fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }

    fn contains(&self, session_id: Option<&str>) -> bool {
        let Some(session_id) = session_id else {
            return false;
        };
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .any(|id| id == session_id)
    }

    fn mark(&self, session_id: &str) {
        let mut sessions = self.0.lock().unwrap_or_else(|error| error.into_inner());
        sessions.retain(|id| id != session_id);
        sessions.push(session_id.to_owned());
        if sessions.len() > MAX_SESSIONS {
            sessions.remove(0);
        }
    }

    fn cleanup(&self, session_id: Option<&str>) {
        let mut sessions = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(session_id) = session_id {
            sessions.retain(|id| id != session_id);
        } else {
            sessions.clear();
        }
    }
}

static ANTHROPIC_SESSIONS: LazyLock<StrictLimitSessions> = LazyLock::new(|| {
    crate::session_resources::register_session_resource_cleanup(|session_id| {
        ANTHROPIC_SESSIONS.cleanup(session_id);
    });
    StrictLimitSessions::new()
});

static BEDROCK_SESSIONS: LazyLock<StrictLimitSessions> = LazyLock::new(|| {
    crate::session_resources::register_session_resource_cleanup(|session_id| {
        BEDROCK_SESSIONS.cleanup(session_id);
    });
    StrictLimitSessions::new()
});

pub fn anthropic_has_exceeded(session_id: Option<&str>) -> bool {
    ANTHROPIC_SESSIONS.contains(session_id)
}

pub fn anthropic_mark_exceeded(session_id: &str) {
    ANTHROPIC_SESSIONS.mark(session_id);
}

pub fn bedrock_has_exceeded(session_id: Option<&str>) -> bool {
    BEDROCK_SESSIONS.contains(session_id)
}

pub fn bedrock_mark_exceeded(session_id: &str) {
    BEDROCK_SESSIONS.mark(session_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_state_is_independent_and_cleanup_is_scoped() {
        let session = "strict-store-isolation-test";
        anthropic_mark_exceeded(session);
        assert!(anthropic_has_exceeded(Some(session)));
        assert!(!bedrock_has_exceeded(Some(session)));
        bedrock_mark_exceeded(session);
        assert!(bedrock_has_exceeded(Some(session)));
        crate::session_resources::cleanup_session_resources(Some(session))
            .expect("cleanup session state");
        assert!(!anthropic_has_exceeded(Some(session)));
        assert!(!bedrock_has_exceeded(Some(session)));
    }
}
