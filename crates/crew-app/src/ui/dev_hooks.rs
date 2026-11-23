//! Debug builds only: environment variables that put the window in a
//! given state at startup, for screenshots and manual checks.

use crew_core::ids::ThreadId;

/// What the environment asked for.
#[derive(Debug, Default, PartialEq)]
pub struct DevHooks {
    /// `CREW_APP_OPEN_THREAD`: opened once the thread is known.
    pub open_thread: Option<ThreadId>,
    /// `CREW_APP_SHOW`: `running` or `settings`.
    pub show: Option<String>,
}

/// Reads the hooks through `var`, logging values it can't use.
pub fn read(var: impl Fn(&str) -> Option<String>) -> DevHooks {
    let open_thread = var("CREW_APP_OPEN_THREAD").and_then(|v| {
        let id = v.trim().parse().ok().map(ThreadId);
        if id.is_none() {
            eprintln!("crew: CREW_APP_OPEN_THREAD={v} is not a thread id");
        }
        id
    });
    let show = var("CREW_APP_SHOW").map(|v| {
        match v.as_str() {
            "running" | "settings" => {
                eprintln!("crew: CREW_APP_SHOW={v} has no view yet, ignored")
            }
            _ => eprintln!("crew: CREW_APP_SHOW={v} is unknown, ignored"),
        }
        v
    });
    DevHooks { open_thread, show }
}

#[cfg(test)]
mod tests {
    use super::{DevHooks, read};
    use crew_core::ids::ThreadId;

    #[test]
    fn hooks_come_from_the_environment() {
        let id = ThreadId::new();
        let env = |k: &str| match k {
            "CREW_APP_OPEN_THREAD" => Some(id.to_string()),
            "CREW_APP_SHOW" => Some("running".to_string()),
            _ => None,
        };
        assert_eq!(
            read(env),
            DevHooks {
                open_thread: Some(id),
                show: Some("running".into()),
            }
        );
        assert_eq!(read(|_| None), DevHooks::default());
        let bad = read(|k| (k == "CREW_APP_OPEN_THREAD").then(|| "nope".to_string()));
        assert_eq!(bad.open_thread, None);
    }
}
