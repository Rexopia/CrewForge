#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerState {
    pub running: bool,
    pub dirty: bool,
}

impl WorkerState {
    pub const fn new() -> Self {
        Self {
            running: false,
            dirty: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeDecision {
    Skip,
    ClearDirty,
    Run,
}

pub fn decide_wake(state: WorkerState, has_unread: bool) -> WakeDecision {
    if state.running {
        return WakeDecision::Skip;
    }
    if !state.dirty && !has_unread {
        return WakeDecision::Skip;
    }
    if !has_unread {
        return WakeDecision::ClearDirty;
    }
    WakeDecision::Run
}

pub fn on_wake_finished(mut state: WorkerState, has_unread_after: bool) -> WorkerState {
    state.running = false;
    if has_unread_after {
        state.dirty = true;
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_does_not_reenter_when_running() {
        let state = WorkerState {
            running: true,
            dirty: true,
        };
        assert_eq!(decide_wake(state, true), WakeDecision::Skip);
    }

    #[test]
    fn worker_clears_dirty_when_no_unread() {
        let state = WorkerState {
            running: false,
            dirty: true,
        };
        assert_eq!(decide_wake(state, false), WakeDecision::ClearDirty);
    }

    #[test]
    fn worker_marks_dirty_when_unread_remains_after_run() {
        let state = WorkerState {
            running: true,
            dirty: false,
        };
        let next = on_wake_finished(state, true);
        assert!(!next.running);
        assert!(next.dirty);
    }
}
