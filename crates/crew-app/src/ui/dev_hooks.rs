//! Debug builds only: environment variables that put the window in a
//! given state at startup, for screenshots and manual checks.

use crew_core::ids::ThreadId;

use super::settings::Page;

/// What the environment asked for.
#[derive(Debug, Default, PartialEq)]
pub struct DevHooks {
    /// `CREW_APP_OPEN_THREAD`: opened once the thread is known.
    pub open_thread: Option<ThreadId>,
    /// `CREW_APP_SHOW`: `running`, `settings[:page]`, `diff:<thread>`,
    /// `terminal:<session>`, `palette:<query>` or
    /// `setting:<field>[:<name>]`.
    pub show: Option<String>,
    /// `CREW_APP_SHOW=diff:<thread>`: the thread's changes, opened once
    /// the thread is known.
    pub review: Option<ThreadId>,
    /// `CREW_APP_SHOW=running`: Running now.
    pub running: bool,
    /// `CREW_APP_SHOW=settings` or `settings:skills|agents|projects|runtimes|limits|shortcuts`.
    pub settings: Option<Page>,
    /// `CREW_APP_SHOW=terminal:<session>`: a terminal session, opened
    /// once it is known.
    pub terminal: Option<String>,
    /// `CREW_APP_SHOW=palette:<query>`: ⌘K with the query typed, once
    /// the state is loaded.
    pub palette: Option<String>,
    /// `CREW_APP_SHOW=setting:<field>[:<agent or project name>]`: the
    /// jump ⌘K makes to that settings field, once the name is known.
    pub setting: Option<(String, Option<String>)>,
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
    let mut running = false;
    let mut settings = None;
    let mut terminal = None;
    let mut palette = None;
    let mut setting = None;
    let show = var("CREW_APP_SHOW").map(|v| {
        match v.as_str() {
            "running" => running = true,
            "settings" => settings = Some(Page::General),
            _ if v.starts_with("settings:") => {
                settings = match &v["settings:".len()..] {
                    "skills" => Some(Page::Skills),
                    "agents" => Some(Page::Agents),
                    "projects" => Some(Page::Projects),
                    "runtimes" => Some(Page::Runtimes),
                    "limits" => Some(Page::Limits),
                    "shortcuts" => Some(Page::Shortcuts),
                    "general" => Some(Page::General),
                    _ => {
                        eprintln!("crew: CREW_APP_SHOW={v} names no settings page, ignored");
                        None
                    }
                }
            }
            _ if v.starts_with("palette:") => palette = Some(v["palette:".len()..].to_string()),
            _ if v.starts_with("setting:") => {
                let mut parts = v["setting:".len()..].splitn(2, ':');
                let field = parts.next().unwrap_or_default().trim();
                if field.is_empty() {
                    eprintln!("crew: CREW_APP_SHOW={v} names no settings field, ignored");
                } else {
                    let name = parts.next().map(|n| n.trim().to_string());
                    setting = Some((field.to_string(), name.filter(|n| !n.is_empty())));
                }
            }
            _ if v.starts_with("terminal:") => {
                let id = v["terminal:".len()..].trim();
                if id.is_empty() {
                    eprintln!("crew: CREW_APP_SHOW={v} names no terminal session, ignored");
                } else {
                    terminal = Some(id.to_string());
                }
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
        running,
        settings,
        terminal,
        palette,
        setting,
    }
}

#[cfg(test)]
mod tests {
    use super::{DevHooks, Page, read};
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
                running: true,
                settings: None,
                terminal: None,
                palette: None,
                setting: None,
            }
        );
        let setting = read(|k| {
            (k == "CREW_APP_SHOW").then(|| "setting:field-project-files:demo".to_string())
        });
        assert_eq!(
            setting.setting,
            Some(("field-project-files".into(), Some("demo".into())))
        );
        let global =
            read(|k| (k == "CREW_APP_SHOW").then(|| "setting:field-appearance-zoom".into()));
        assert_eq!(global.setting, Some(("field-appearance-zoom".into(), None)));
        assert_eq!(read(|_| Some("setting:".into())).setting, None);
        let palette = read(|k| (k == "CREW_APP_SHOW").then(|| "palette:auto".to_string()));
        assert_eq!(palette.palette.as_deref(), Some("auto"));
        let terminal = read(|k| (k == "CREW_APP_SHOW").then(|| "terminal:abc-1".to_string()));
        assert_eq!(terminal.terminal.as_deref(), Some("abc-1"));
        assert_eq!(read(|_| Some("terminal:".into())).terminal, None);
        let page = read(|k| (k == "CREW_APP_SHOW").then(|| "settings:agents".to_string()));
        assert_eq!(page.settings, Some(Page::Agents));
        let general = read(|k| (k == "CREW_APP_SHOW").then(|| "settings".to_string()));
        assert_eq!(general.settings, Some(Page::General));
        let diff = format!("diff:{id}");
        let review = read(|k| (k == "CREW_APP_SHOW").then(|| diff.clone()));
        assert_eq!(review.review, Some(id));
        assert!(!review.running);
        assert_eq!(read(|_| Some("diff:nope".into())).review, None);
        assert_eq!(read(|_| None), DevHooks::default());
        let bad = read(|k| (k == "CREW_APP_OPEN_THREAD").then(|| "nope".to_string()));
        assert_eq!(bad.open_thread, None);
    }
}
