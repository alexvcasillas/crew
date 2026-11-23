//! Debug builds only: environment variables that put the window in a
//! given state at startup, for screenshots and manual checks.

use crew_core::ids::ThreadId;

/// What the environment asked for.
#[derive(Debug, Default, PartialEq)]
pub struct DevHooks {
    /// `CREW_APP_OPEN_THREAD`: opened once the thread is known.
    pub open_thread: Option<ThreadId>,
    /// `CREW_APP_SHOW`: `running`, `settings` or `diff:<thread>`.
    pub show: Option<String>,
    /// `CREW_APP_SHOW=diff:<thread>`: the thread's changes, opened once
    /// the thread is known.
    pub review: Option<ThreadId>,
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
    let mut review = None;
    let show = var("CREW_APP_SHOW").map(|v| {
        match v.as_str() {
            "running" | "settings" => {
                eprintln!("crew: CREW_APP_SHOW={v} has no view yet, ignored")
            }
            _ => match v.strip_prefix("diff:").map(|id| id.trim().parse()) {
                Some(Ok(id)) => review = Some(ThreadId(id)),
                Some(Err(_)) => eprintln!("crew: CREW_APP_SHOW={v} is not diff:<thread id>"),
                None => eprintln!("crew: CREW_APP_SHOW={v} is unknown, ignored"),
            },
        }
        v
    });
    if let (Some(open), Some(diff)) = (open_thread, review)
        && open != diff
    {
        eprintln!(
            "crew: CREW_APP_OPEN_THREAD and CREW_APP_SHOW=diff: name different threads, opening {diff}"
        );
    }
    DevHooks {
        open_thread,
        show,
        review,
    }
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
                review: None,
            }
        );
        let diff = format!("diff:{id}");
        let review = read(|k| (k == "CREW_APP_SHOW").then(|| diff.clone()));
        assert_eq!(review.review, Some(id));
        assert_eq!(read(|_| Some("diff:nope".into())).review, None);
        assert_eq!(read(|_| None), DevHooks::default());
        let bad = read(|k| (k == "CREW_APP_OPEN_THREAD").then(|| "nope".to_string()));
        assert_eq!(bad.open_thread, None);
    }
}
