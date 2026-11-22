use std::time::Duration;

pub const CHECK_EVERY: Duration = Duration::from_secs(30);
pub const ORPHANED_EXIT_CODE: i32 = 75;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    Live,
    Orphaned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Serve,
    Exit,
}

pub fn probe() -> Session {
    match nix::unistd::User::from_uid(nix::unistd::getuid()) {
        Ok(Some(_)) => Session::Live,
        _ => Session::Orphaned,
    }
}

pub fn decide(at_start: Session, now: Session) -> Action {
    match (at_start, now) {
        (Session::Live, Session::Orphaned) => Action::Exit,
        _ => Action::Serve,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_daemon_that_loses_its_session_exits() {
        assert_eq!(decide(Session::Live, Session::Orphaned), Action::Exit);
    }

    #[test]
    fn a_live_session_keeps_serving() {
        assert_eq!(decide(Session::Live, Session::Live), Action::Serve);
    }

    #[test]
    fn a_host_that_never_resolved_the_user_does_not_flap() {
        assert_eq!(decide(Session::Orphaned, Session::Orphaned), Action::Serve);
        assert_eq!(decide(Session::Orphaned, Session::Live), Action::Serve);
    }
}
