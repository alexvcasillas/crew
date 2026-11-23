//! Running crewd under launchd when Crew runs from its app bundle: a
//! LaunchAgent at `~/Library/LaunchAgents/dev.crew.crewd.plist` that
//! starts the bundled crewd at login and again if it crashes.
//!
//! Everything here blocks (it runs `launchctl` and a login shell), so call
//! it off the UI thread.

use std::fmt;
use std::io::{self, Read as _};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The LaunchAgent's label.
pub const LABEL: &str = "dev.crew.crewd";
/// How long the login shell gets to print its `PATH`.
const SHELL_TIMEOUT: Duration = Duration::from_secs(3);
/// What crewd gets on its `PATH` when the login shell doesn't answer.
const FALLBACK_PATH: &str = "/usr/local/bin:/opt/homebrew/bin:$HOME/.local/bin:/usr/bin:/bin";

/// Why launchd couldn't be set up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The crewd inside the app bundle that `exe` runs from, or `None` when
/// the app doesn't run from a bundle (a dev build in `target/`).
pub fn bundled_crewd(exe: &Path) -> Option<PathBuf> {
    exe.to_str()?
        .contains(".app/Contents/MacOS/")
        .then_some(())?;
    Some(exe.parent()?.join("crewd"))
}

/// Writes the LaunchAgent for `crewd` on `crew_home` if it is missing or
/// different, loads it, and starts crewd if it isn't running.
pub fn ensure(crewd: &Path, crew_home: &Path) -> Result<()> {
    Launchd::system()?.ensure(crewd, crew_home).map(drop)
}

/// Like [`ensure`], then restarts crewd so the bundled one runs. Stops
/// its runs: ask first.
pub fn restart(crewd: &Path, crew_home: &Path) -> Result<()> {
    Launchd::system()?.restart(crewd, crew_home)
}

/// What a command printed and whether it succeeded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs commands, so tests can stand in for `launchctl`.
pub trait Runner {
    /// Runs `program` with `args`, killed after `timeout` if one is given.
    fn run(&self, program: &str, args: &[String], timeout: Option<Duration>) -> io::Result<Output>;
}

/// Runs real commands.
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[String], timeout: Option<Duration>) -> io::Result<Output> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let out = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stdout.read_to_string(&mut s);
            s
        });
        let err = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if timeout.is_some_and(|t| started.elapsed() >= t) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{program} took too long"),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        Ok(Output {
            success: status.success(),
            stdout: out.join().unwrap_or_default(),
            stderr: err.join().unwrap_or_default(),
        })
    }
}

/// What the LaunchAgent says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPlist {
    pub crewd: PathBuf,
    pub log: PathBuf,
    /// Set only when it isn't the default `~/.crew`.
    pub crew_home: Option<PathBuf>,
    pub path: String,
}

impl AgentPlist {
    pub fn render(&self) -> String {
        let mut env = String::new();
        if let Some(home) = &self.crew_home {
            env.push_str(&format!(
                "    <key>CREW_HOME</key>\n    <string>{}</string>\n",
                escape(&home.to_string_lossy())
            ));
        }
        env.push_str(&format!(
            "    <key>PATH</key>\n    <string>{}</string>\n",
            escape(&self.path)
        ));
        let log = escape(&self.log.to_string_lossy());
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{crewd}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
  <key>EnvironmentVariables</key>
  <dict>
{env}  </dict>
</dict>
</plist>
"#,
            crewd = escape(&self.crewd.to_string_lossy()),
        )
    }
}

/// The `PATH` a plist written by [`AgentPlist::render`] gives crewd.
fn plist_path(plist: &str) -> Option<String> {
    let rest = &plist[plist.find("<key>PATH</key>")?..];
    let start = rest.find("<string>")? + "<string>".len();
    let end = rest.find("</string>")?;
    Some(unescape(rest.get(start..end)?))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The `PATH` from what a login shell printed. Shell startup files may
/// print lines of their own first, so only the last line counts.
fn parse_shell_path(printed: &str) -> Option<String> {
    let path = printed.rsplit('\n').next()?.trim();
    path.contains('/').then(|| path.to_string())
}

/// launchd for one user.
pub struct Launchd<R> {
    runner: R,
    /// The user's home, holding `Library/LaunchAgents`.
    home: PathBuf,
    uid: u32,
    /// The user's login shell.
    shell: Option<String>,
}

impl Launchd<SystemRunner> {
    fn system() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| Error("HOME isn't set.".into()))?;
        Ok(Self {
            runner: SystemRunner,
            home: PathBuf::from(home),
            uid: nix::unistd::getuid().as_raw(),
            shell: std::env::var("SHELL").ok().filter(|s| !s.is_empty()),
        })
    }
}

impl<R: Runner> Launchd<R> {
    pub fn new(runner: R, home: PathBuf, uid: u32, shell: Option<String>) -> Self {
        Self {
            runner,
            home,
            uid,
            shell,
        }
    }

    pub fn plist_file(&self) -> PathBuf {
        self.home
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"))
    }

    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn service(&self) -> String {
        format!("gui/{}/{LABEL}", self.uid)
    }

    /// The `PATH` from the login shell, else [`FALLBACK_PATH`].
    fn login_path(&self) -> String {
        let printed = self.shell.as_ref().and_then(|shell| {
            let args = ["-lc".to_string(), r#"printf %s "$PATH""#.to_string()];
            let out = self.runner.run(shell, &args, Some(SHELL_TIMEOUT)).ok()?;
            out.success.then_some(out.stdout)
        });
        printed
            .as_deref()
            .and_then(parse_shell_path)
            .unwrap_or_else(|| FALLBACK_PATH.replace("$HOME", &self.home.to_string_lossy()))
    }

    /// The plist for `crewd` on `crew_home`. Keeps the `PATH` of a plist
    /// already there, so it is captured once.
    pub fn plist(&self, crewd: &Path, crew_home: &Path, existing: Option<&str>) -> AgentPlist {
        let default_home = self.home.join(".crew");
        AgentPlist {
            crewd: crewd.to_path_buf(),
            log: crew_home.join("crewd.log"),
            crew_home: (crew_home != default_home).then(|| crew_home.to_path_buf()),
            path: existing
                .and_then(plist_path)
                .unwrap_or_else(|| self.login_path()),
        }
    }

    fn launchctl(&self, args: &[&str]) -> Result<Output> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        self.runner
            .run("launchctl", &args, None)
            .map_err(|e| Error(format!("launchctl couldn't run ({e})")))
    }

    /// Runs `launchctl args`, failing with what it printed.
    fn launchctl_ok(&self, args: &[&str]) -> Result<()> {
        let out = self.launchctl(args)?;
        if out.success {
            return Ok(());
        }
        let said = out.stderr.trim();
        let said = if said.is_empty() {
            out.stdout.trim()
        } else {
            said
        };
        Err(Error(format!("launchctl {} failed: {said}", args[0])))
    }

    fn loaded(&self) -> Result<bool> {
        Ok(self.launchctl(&["print", &self.service()])?.success)
    }

    /// Writes the plist if missing or different and loads it, reloading a
    /// loaded one that changed, then starts crewd if it isn't running.
    /// Returns whether a loaded service was reloaded, which restarts it.
    pub fn ensure(&self, crewd: &Path, crew_home: &Path) -> Result<bool> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(crew_home)?;
        let file = self.plist_file();
        let existing = std::fs::read_to_string(&file).ok();
        let wanted = self.plist(crewd, crew_home, existing.as_deref()).render();
        let changed = existing.as_deref() != Some(wanted.as_str());
        if changed {
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = file.with_extension("plist.tmp");
            std::fs::write(&tmp, &wanted)?;
            std::fs::rename(&tmp, &file)?;
        }
        let plist = file.to_string_lossy();
        let loaded = self.loaded()?;
        let reloaded = loaded && changed;
        if reloaded {
            self.launchctl_ok(&["bootout", &self.service()])?;
        }
        if !loaded || changed {
            self.launchctl_ok(&["bootstrap", &self.domain(), &plist])?;
        }
        self.launchctl_ok(&["kickstart", &self.service()])?;
        Ok(reloaded)
    }

    /// [`Launchd::ensure`], then restarts crewd unless that already did.
    pub fn restart(&self, crewd: &Path, crew_home: &Path) -> Result<()> {
        if !self.ensure(crewd, crew_home)? {
            self.launchctl_ok(&["kickstart", "-k", &self.service()])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashSet;

    use super::*;

    /// Records each command. `launchctl print` succeeds when `loaded`;
    /// the shell prints `shell_out`; everything else succeeds.
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<String>>,
        loaded: bool,
        shell_out: Option<String>,
        fail: HashSet<&'static str>,
    }

    impl Runner for &Fake {
        fn run(&self, program: &str, args: &[String], _: Option<Duration>) -> io::Result<Output> {
            self.calls
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            if program != "launchctl" {
                return match &self.shell_out {
                    Some(out) => Ok(Output {
                        success: true,
                        stdout: out.clone(),
                        stderr: String::new(),
                    }),
                    None => Err(io::Error::new(io::ErrorKind::TimedOut, "slow")),
                };
            }
            let success = if args[0] == "print" {
                self.loaded
            } else {
                !self.fail.contains(args[0].as_str())
            };
            Ok(Output {
                success,
                stdout: String::new(),
                stderr: if success { "" } else { "Input/output error" }.into(),
            })
        }
    }

    impl Fake {
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    const CREWD: &str = "/Applications/Crew.app/Contents/MacOS/crewd";

    fn launchd<'a>(fake: &'a Fake, home: &Path) -> Launchd<&'a Fake> {
        Launchd::new(fake, home.to_path_buf(), 501, Some("/bin/zsh".into()))
    }

    fn sample() -> AgentPlist {
        AgentPlist {
            crewd: CREWD.into(),
            log: "/work/me/.crew/crewd.log".into(),
            crew_home: None,
            path: "/opt/homebrew/bin:/usr/bin:/bin".into(),
        }
    }

    #[test]
    fn plist_runs_the_bundled_crewd_and_restarts_it_on_crashes_only() {
        let text = sample().render();
        assert!(text.contains("<key>Label</key>\n  <string>dev.crew.crewd</string>"));
        assert!(text.contains(&format!(
            "<array>\n    <string>{CREWD}</string>\n  </array>"
        )));
        assert!(text.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(text.contains(
            "<key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>"
        ));
        assert!(
            text.contains(
                "<key>StandardOutPath</key>\n  <string>/work/me/.crew/crewd.log</string>"
            )
        );
        assert!(
            text.contains(
                "<key>StandardErrorPath</key>\n  <string>/work/me/.crew/crewd.log</string>"
            )
        );
        assert!(text.contains("<key>PATH</key>\n    <string>/opt/homebrew/bin:/usr/bin:/bin"));
        assert!(!text.contains("CREW_HOME"), "the default home isn't set");
    }

    #[test]
    fn plist_sets_a_non_default_home_and_escapes_text() {
        let plist = AgentPlist {
            crew_home: Some("/work/a&b <crew>".into()),
            ..sample()
        };
        let text = plist.render();
        assert!(
            text.contains("<key>CREW_HOME</key>\n    <string>/work/a&amp;b &lt;crew&gt;</string>")
        );
        assert_eq!(plist_path(&text).as_deref(), Some(sample().path.as_str()));
    }

    #[test]
    fn rendered_plist_passes_plutil() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent.plist");
        let plist = AgentPlist {
            crew_home: Some("/work/a&b".into()),
            path: "/a b/bin:/usr/bin".into(),
            ..sample()
        };
        std::fs::write(&file, plist.render()).unwrap();
        let out = Command::new("plutil")
            .arg("-lint")
            .arg(&file)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    #[test]
    fn only_an_app_bundle_runs_crewd_under_launchd() {
        assert_eq!(
            bundled_crewd(Path::new("/Applications/Crew.app/Contents/MacOS/crew-app")),
            Some(PathBuf::from(CREWD))
        );
        assert_eq!(
            bundled_crewd(Path::new("/work/crew/target/debug/crew-app")),
            None
        );
    }

    #[test]
    fn path_is_the_last_line_the_login_shell_prints() {
        assert_eq!(
            parse_shell_path("welcome back\n/opt/homebrew/bin:/usr/bin").as_deref(),
            Some("/opt/homebrew/bin:/usr/bin")
        );
        assert_eq!(parse_shell_path(""), None);
        assert_eq!(parse_shell_path("no path here"), None);
    }

    #[test]
    fn path_comes_from_the_login_shell() {
        let home = tempfile::tempdir().unwrap();
        let fake = Fake {
            shell_out: Some("/opt/homebrew/bin:/usr/bin".into()),
            ..Fake::default()
        };
        let plist =
            launchd(&fake, home.path()).plist(Path::new(CREWD), &home.path().join(".crew"), None);
        assert_eq!(plist.path, "/opt/homebrew/bin:/usr/bin");
        assert_eq!(plist.crew_home, None);
        assert_eq!(fake.calls(), [r#"/bin/zsh -lc printf %s "$PATH""#]);
    }

    #[test]
    fn a_slow_or_missing_shell_falls_back_to_the_usual_path() {
        let home = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        let crew_home = home.path().join("other");
        let plist = launchd(&fake, home.path()).plist(Path::new(CREWD), &crew_home, None);
        let h = home.path().display();
        assert_eq!(
            plist.path,
            format!("/usr/local/bin:/opt/homebrew/bin:{h}/.local/bin:/usr/bin:/bin")
        );
        assert_eq!(plist.crew_home, Some(crew_home));

        let no_shell = Launchd::new(&fake, home.path().to_path_buf(), 501, None);
        let plist = no_shell.plist(Path::new(CREWD), &home.path().join(".crew"), None);
        assert!(plist.path.starts_with("/usr/local/bin:"));
    }

    #[test]
    fn first_ensure_writes_bootstraps_and_starts() {
        let home = tempfile::tempdir().unwrap();
        let fake = Fake {
            shell_out: Some("/usr/bin:/bin".into()),
            ..Fake::default()
        };
        let ld = launchd(&fake, home.path());
        let crew_home = home.path().join(".crew");
        assert_eq!(ld.ensure(Path::new(CREWD), &crew_home), Ok(false));
        let file = ld.plist_file();
        assert_eq!(
            file,
            home.path()
                .join("Library/LaunchAgents/dev.crew.crewd.plist")
        );
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains(CREWD));
        assert!(crew_home.is_dir());
        assert_eq!(
            fake.calls(),
            [
                r#"/bin/zsh -lc printf %s "$PATH""#.to_string(),
                "launchctl print gui/501/dev.crew.crewd".into(),
                format!("launchctl bootstrap gui/501 {}", file.display()),
                "launchctl kickstart gui/501/dev.crew.crewd".into(),
            ]
        );
    }

    #[test]
    fn an_unchanged_loaded_agent_is_only_kickstarted() {
        let home = tempfile::tempdir().unwrap();
        let first = Fake {
            shell_out: Some("/usr/bin:/bin".into()),
            ..Fake::default()
        };
        let crew_home = home.path().join(".crew");
        launchd(&first, home.path())
            .ensure(Path::new(CREWD), &crew_home)
            .unwrap();

        // The PATH is kept from the plist: no shell this time.
        let fake = Fake {
            loaded: true,
            ..Fake::default()
        };
        assert_eq!(
            launchd(&fake, home.path()).ensure(Path::new(CREWD), &crew_home),
            Ok(false)
        );
        assert_eq!(
            fake.calls(),
            [
                "launchctl print gui/501/dev.crew.crewd",
                "launchctl kickstart gui/501/dev.crew.crewd",
            ]
        );
    }

    #[test]
    fn a_changed_loaded_agent_is_reloaded() {
        let home = tempfile::tempdir().unwrap();
        let first = Fake {
            shell_out: Some("/usr/bin:/bin".into()),
            ..Fake::default()
        };
        let crew_home = home.path().join(".crew");
        launchd(&first, home.path())
            .ensure(Path::new("/Old/Crew.app/Contents/MacOS/crewd"), &crew_home)
            .unwrap();

        let fake = Fake {
            loaded: true,
            ..Fake::default()
        };
        let ld = launchd(&fake, home.path());
        assert_eq!(ld.ensure(Path::new(CREWD), &crew_home), Ok(true));
        let text = std::fs::read_to_string(ld.plist_file()).unwrap();
        assert!(text.contains(CREWD));
        assert!(text.contains("<string>/usr/bin:/bin</string>"), "PATH kept");
        assert_eq!(
            fake.calls(),
            [
                "launchctl print gui/501/dev.crew.crewd".to_string(),
                "launchctl bootout gui/501/dev.crew.crewd".into(),
                format!("launchctl bootstrap gui/501 {}", ld.plist_file().display()),
                "launchctl kickstart gui/501/dev.crew.crewd".into(),
            ]
        );
    }

    #[test]
    fn restart_kills_and_starts_unless_ensure_reloaded() {
        let home = tempfile::tempdir().unwrap();
        let crew_home = home.path().join(".crew");
        let first = Fake {
            shell_out: Some("/usr/bin:/bin".into()),
            ..Fake::default()
        };
        launchd(&first, home.path())
            .ensure(Path::new(CREWD), &crew_home)
            .unwrap();

        let fake = Fake {
            loaded: true,
            ..Fake::default()
        };
        launchd(&fake, home.path())
            .restart(Path::new(CREWD), &crew_home)
            .unwrap();
        assert_eq!(
            fake.calls().last().map(String::as_str),
            Some("launchctl kickstart -k gui/501/dev.crew.crewd")
        );

        let fake = Fake {
            loaded: true,
            ..Fake::default()
        };
        launchd(&fake, home.path())
            .restart(Path::new("/New/Crew.app/Contents/MacOS/crewd"), &crew_home)
            .unwrap();
        assert!(fake.calls().iter().all(|c| !c.contains("kickstart -k")));
    }

    #[test]
    fn a_failed_launchctl_says_what_it_printed() {
        let home = tempfile::tempdir().unwrap();
        let fake = Fake {
            shell_out: Some("/usr/bin".into()),
            fail: HashSet::from(["bootstrap"]),
            ..Fake::default()
        };
        let err = launchd(&fake, home.path())
            .ensure(Path::new(CREWD), &home.path().join(".crew"))
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "launchctl bootstrap failed: Input/output error"
        );
    }

    #[test]
    fn system_runner_reports_output_and_times_out() {
        let out = SystemRunner
            .run("/bin/sh", &["-c".into(), "printf hi; exit 3".into()], None)
            .unwrap();
        assert_eq!(out.stdout, "hi");
        assert!(!out.success);
        let started = Instant::now();
        let err = SystemRunner
            .run(
                "/bin/sh",
                &["-c".into(), "sleep 5".into()],
                Some(Duration::from_millis(200)),
            )
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
