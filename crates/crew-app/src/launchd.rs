//! Running crewd under launchd when Crew runs from its app bundle: a
//! LaunchAgent at `~/Library/LaunchAgents/dev.crew.crewd.plist` that
//! starts the bundled crewd at login and again if it crashes.
//!
//! [`Launchd::plan`] only looks; [`Launchd::apply`] writes the plist and
//! reloads it together, so a plist is never left written but not loaded.
//! Everything here blocks (it runs `launchctl` and a login shell), so call
//! it off the UI thread.

use std::fmt;
use std::io::{self, Read as _};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The LaunchAgent's label.
pub const LABEL: &str = "dev.crew.crewd";
/// How long the login shell gets to print its `PATH`.
const SHELL_TIMEOUT: Duration = Duration::from_secs(3);
/// Where `claude` and `codex` usually live. Added to the login shell's
/// `PATH` when missing, and all of it when the shell doesn't answer.
const FALLBACK_PATH: &str = "/usr/local/bin:/opt/homebrew/bin:$HOME/.local/bin:/usr/bin:/bin";
/// How many times `bootstrap` is tried while a booted-out job winds down.
const BOOTSTRAP_TRIES: usize = 5;
/// `launchctl print`'s exit code for a job that isn't loaded.
const NOT_LOADED: i32 = 113;

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
/// the app doesn't run from a bundle (a dev build in `target/`) or the
/// bundle has no crewd.
pub fn bundled_crewd(exe: &Path) -> Option<PathBuf> {
    exe.to_str()?
        .contains(".app/Contents/MacOS/")
        .then_some(())?;
    Some(exe.parent()?.join("crewd")).filter(|crewd| crewd.is_file())
}

/// What [`apply`] would do for `crewd` on `crew_home`. `fresh_path` asks
/// the login shell for `PATH` again instead of keeping the plist's.
pub fn plan(crewd: &Path, crew_home: &Path, fresh_path: bool) -> Result<Plan> {
    Launchd::system()?.plan(crewd, crew_home, fresh_path)
}

/// Writes and loads the plist `plan` holds, then starts crewd, or
/// restarts it if `restart`.
pub fn apply(plan: &Plan, restart: bool) -> Result<()> {
    Launchd::system()?.apply(plan, restart)
}

/// What a command printed and how it exited.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs commands, so tests can stand in for `launchctl`.
pub trait Runner {
    /// Runs `program` with `args`, killed after `timeout` if one is given.
    fn run(&self, program: &str, args: &[String], timeout: Option<Duration>) -> io::Result<Output>;
}

/// Runs real commands, each in its own process group so a timeout kills
/// whatever it started too.
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[String], timeout: Option<Duration>) -> io::Result<Output> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
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
                let group = nix::unistd::Pid::from_raw(child.id() as i32);
                let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
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
            code: status.code(),
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

/// `path` with each of `fallback`'s dirs it lacks added at the end.
fn with_fallback(path: &str, fallback: &str) -> String {
    let mut dirs: Vec<&str> = path.split(':').filter(|d| !d.is_empty()).collect();
    for dir in fallback.split(':') {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs.join(":")
}

/// What [`Launchd::apply`] will do, found by [`Launchd::plan`] without
/// changing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    file: PathBuf,
    crew_home: PathBuf,
    plist: String,
    /// The plist on disk is missing or different.
    pub changed: bool,
    /// launchd has the job.
    pub loaded: bool,
    /// The pid of the crewd launchd runs, if it runs.
    pub job_pid: Option<u32>,
}

impl Plan {
    /// Applying reloads the job, which restarts its crewd.
    pub fn reloads(&self) -> bool {
        self.loaded && self.changed
    }
}

/// launchd's view of the job.
enum Job {
    NotLoaded,
    Loaded { pid: Option<u32> },
}

/// The `pid = 812` line of `launchctl print`.
fn job_pid(printed: &str) -> Option<u32> {
    printed
        .lines()
        .find_map(|l| l.trim().strip_prefix("pid = "))
        .and_then(|pid| pid.trim().parse().ok())
}

/// launchd for one user.
pub struct Launchd<R> {
    runner: R,
    /// The user's home, holding `Library/LaunchAgents`.
    home: PathBuf,
    uid: u32,
    /// The user's login shell.
    shell: Option<String>,
    /// The pause between `bootstrap` tries.
    retry_pause: Duration,
}

impl Launchd<SystemRunner> {
    fn system() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| Error("HOME isn't set".into()))?;
        Ok(Self {
            runner: SystemRunner,
            home: PathBuf::from(home),
            uid: nix::unistd::getuid().as_raw(),
            shell: std::env::var("SHELL").ok().filter(|s| !s.is_empty()),
            retry_pause: Duration::from_millis(300),
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
            retry_pause: Duration::ZERO,
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

    /// The `PATH` an interactive login shell sets, so what you set up in
    /// `.zshrc` counts too, with the usual dirs added. Just the usual dirs
    /// if the shell doesn't answer.
    fn login_path(&self) -> String {
        let fallback = FALLBACK_PATH.replace("$HOME", &self.home.to_string_lossy());
        let printed = self.shell.as_ref().and_then(|shell| {
            let args = ["-ilc".to_string(), r#"printf %s "$PATH""#.to_string()];
            let out = self.runner.run(shell, &args, Some(SHELL_TIMEOUT)).ok()?;
            out.success.then_some(out.stdout)
        });
        match printed.as_deref().and_then(parse_shell_path) {
            Some(path) => with_fallback(&path, &fallback),
            None => fallback,
        }
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

    fn failed(args: &[&str], out: &Output) -> Error {
        let said = out.stderr.trim();
        let said = if said.is_empty() {
            out.stdout.trim()
        } else {
            said
        };
        Error(format!("launchctl {} failed: {said}", args[0]))
    }

    /// Runs `launchctl args`, failing with what it printed.
    fn launchctl_ok(&self, args: &[&str]) -> Result<()> {
        let out = self.launchctl(args)?;
        if out.success {
            Ok(())
        } else {
            Err(Self::failed(args, &out))
        }
    }

    fn job(&self) -> Result<Job> {
        let args = ["print", &self.service()];
        let out = self.launchctl(&args)?;
        if out.success {
            return Ok(Job::Loaded {
                pid: job_pid(&out.stdout),
            });
        }
        let missing = out.code == Some(NOT_LOADED)
            || out.stderr.contains("Could not find service")
            || out.stdout.contains("Could not find service");
        if missing {
            Ok(Job::NotLoaded)
        } else {
            Err(Self::failed(&args, &out))
        }
    }

    /// Bootstraps the plist, trying again while an old job winds down
    /// (error 5).
    fn bootstrap(&self, file: &str) -> Result<()> {
        let args = ["bootstrap", &self.domain(), file];
        let mut tries = 0;
        loop {
            let out = self.launchctl(&args)?;
            tries += 1;
            if out.success {
                return Ok(());
            }
            let busy = out.code == Some(5) || out.stderr.contains("5: Input/output error");
            if !busy || tries >= BOOTSTRAP_TRIES {
                return Err(Self::failed(&args, &out));
            }
            std::thread::sleep(self.retry_pause);
        }
    }

    /// Finds what [`Launchd::apply`] would do. Changes nothing.
    pub fn plan(&self, crewd: &Path, crew_home: &Path, fresh_path: bool) -> Result<Plan> {
        let file = self.plist_file();
        let existing = std::fs::read_to_string(&file).ok();
        let kept = existing.as_deref().filter(|_| !fresh_path);
        let plist = self.plist(crewd, crew_home, kept).render();
        let (loaded, job_pid) = match self.job()? {
            Job::NotLoaded => (false, None),
            Job::Loaded { pid } => (true, pid),
        };
        Ok(Plan {
            changed: existing.as_deref() != Some(plist.as_str()),
            file,
            crew_home: crew_home.to_path_buf(),
            plist,
            loaded,
            job_pid,
        })
    }

    /// Writes the plist if it changed and reloads a loaded job in the
    /// same go, then starts crewd. `restart` restarts a running one, which
    /// a reload already does.
    pub fn apply(&self, plan: &Plan, restart: bool) -> Result<()> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&plan.crew_home)?;
        if plan.changed {
            if let Some(dir) = plan.file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = plan.file.with_extension("plist.tmp");
            std::fs::write(&tmp, &plan.plist)?;
            std::fs::rename(&tmp, &plan.file)?;
        }
        let service = self.service();
        if plan.reloads() {
            self.launchctl_ok(&["bootout", &service])?;
        }
        if !plan.loaded || plan.changed {
            self.bootstrap(&plan.file.to_string_lossy())?;
        }
        if restart && plan.loaded && !plan.changed {
            self.launchctl_ok(&["kickstart", "-k", &service])
        } else {
            self.launchctl_ok(&["kickstart", &service])
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::HashSet;

    use super::*;

    /// Records each command. `launchctl print` finds the job when
    /// `job` is set (with its pid), else says it isn't loaded, or fails
    /// some other way with `print_fails`. `bootstrap` fails with error 5
    /// `bootstrap_busy` times first. The shell prints `shell_out`.
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<String>>,
        job: Option<Option<u32>>,
        print_fails: bool,
        bootstrap_busy: Cell<usize>,
        shell_out: Option<String>,
        fail: HashSet<&'static str>,
    }

    fn out(code: i32, stdout: &str, stderr: &str) -> io::Result<Output> {
        Ok(Output {
            success: code == 0,
            code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
        })
    }

    impl Runner for &Fake {
        fn run(&self, program: &str, args: &[String], _: Option<Duration>) -> io::Result<Output> {
            self.calls
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            if program != "launchctl" {
                return match &self.shell_out {
                    Some(printed) => out(0, printed, ""),
                    None => Err(io::Error::new(io::ErrorKind::TimedOut, "slow")),
                };
            }
            match args[0].as_str() {
                "print" if self.print_fails => out(1, "", "Operation not permitted"),
                "print" => match self.job {
                    Some(Some(pid)) => out(
                        0,
                        &format!("{LABEL} = {{\n\tstate = running\n\tpid = {pid}\n}}"),
                        "",
                    ),
                    Some(None) => out(0, &format!("{LABEL} = {{\n\tstate = not running\n}}"), ""),
                    None => out(
                        113,
                        "",
                        &format!("Could not find service \"{LABEL}\" in domain for user gui: 501"),
                    ),
                },
                "bootstrap" if self.bootstrap_busy.get() > 0 => {
                    self.bootstrap_busy.set(self.bootstrap_busy.get() - 1);
                    out(5, "", "Bootstrap failed: 5: Input/output error")
                }
                cmd if self.fail.contains(cmd) => out(1, "", "Input/output error"),
                _ => out(0, "", ""),
            }
        }
    }

    impl Fake {
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    const CREWD: &str = "/Applications/Crew.app/Contents/MacOS/crewd";
    const SHELL_CALL: &str = r#"/bin/zsh -ilc printf %s "$PATH""#;

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

    /// A home whose plist was written for `crewd` with `PATH=/usr/bin`.
    fn installed(crewd: &str) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let first = Fake {
            shell_out: Some("/usr/bin".into()),
            ..Fake::default()
        };
        let ld = launchd(&first, home.path());
        let plan = ld
            .plan(Path::new(crewd), &home.path().join(".crew"), false)
            .unwrap();
        ld.apply(&plan, false).unwrap();
        home
    }

    fn plist_text(home: &Path) -> String {
        std::fs::read_to_string(home.join("Library/LaunchAgents/dev.crew.crewd.plist")).unwrap()
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
    fn only_an_app_bundle_with_crewd_runs_it_under_launchd() {
        let dir = tempfile::tempdir().unwrap();
        let macos = dir.path().join("Crew.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        let exe = macos.join("crew-app");
        assert_eq!(bundled_crewd(&exe), None, "no crewd in the bundle");
        std::fs::write(macos.join("crewd"), "").unwrap();
        assert_eq!(bundled_crewd(&exe), Some(macos.join("crewd")));
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::write(dir.path().join("target/debug/crewd"), "").unwrap();
        assert_eq!(
            bundled_crewd(&dir.path().join("target/debug/crew-app")),
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
    fn path_comes_from_an_interactive_login_shell_plus_the_usual_dirs() {
        let home = tempfile::tempdir().unwrap();
        let fake = Fake {
            shell_out: Some("/work/bin:/opt/homebrew/bin:/usr/bin".into()),
            ..Fake::default()
        };
        let h = home.path().display();
        let plist =
            launchd(&fake, home.path()).plist(Path::new(CREWD), &home.path().join(".crew"), None);
        assert_eq!(
            plist.path,
            format!("/work/bin:/opt/homebrew/bin:/usr/bin:/usr/local/bin:{h}/.local/bin:/bin")
        );
        assert_eq!(plist.crew_home, None);
        assert_eq!(fake.calls(), [SHELL_CALL]);
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
    fn first_apply_writes_bootstraps_and_starts() {
        let home = tempfile::tempdir().unwrap();
        let fake = Fake {
            shell_out: Some("/usr/bin:/bin".into()),
            ..Fake::default()
        };
        let ld = launchd(&fake, home.path());
        let crew_home = home.path().join(".crew");
        let plan = ld.plan(Path::new(CREWD), &crew_home, false).unwrap();
        assert!(plan.changed && !plan.loaded && !plan.reloads());
        assert_eq!(plan.job_pid, None);
        ld.apply(&plan, false).unwrap();
        let file = ld.plist_file();
        assert_eq!(
            file,
            home.path()
                .join("Library/LaunchAgents/dev.crew.crewd.plist")
        );
        assert!(plist_text(home.path()).contains(CREWD));
        assert!(crew_home.is_dir());
        assert_eq!(
            fake.calls(),
            [
                SHELL_CALL.to_string(),
                "launchctl print gui/501/dev.crew.crewd".into(),
                format!("launchctl bootstrap gui/501 {}", file.display()),
                "launchctl kickstart gui/501/dev.crew.crewd".into(),
            ]
        );
    }

    #[test]
    fn an_unchanged_loaded_agent_is_only_kickstarted() {
        let home = installed(CREWD);
        // The PATH is kept from the plist: no shell this time.
        let fake = Fake {
            job: Some(Some(812)),
            ..Fake::default()
        };
        let ld = launchd(&fake, home.path());
        let plan = ld
            .plan(Path::new(CREWD), &home.path().join(".crew"), false)
            .unwrap();
        assert!(!plan.changed && plan.loaded);
        assert_eq!(plan.job_pid, Some(812));
        ld.apply(&plan, false).unwrap();
        assert_eq!(
            fake.calls(),
            [
                "launchctl print gui/501/dev.crew.crewd",
                "launchctl kickstart gui/501/dev.crew.crewd",
            ]
        );
    }

    /// Planning never writes: a busy crewd can be left as it is.
    #[test]
    fn planning_a_reload_changes_nothing() {
        let home = installed("/Old/Crew.app/Contents/MacOS/crewd");
        let before = plist_text(home.path());
        let fake = Fake {
            job: Some(Some(812)),
            ..Fake::default()
        };
        let plan = launchd(&fake, home.path())
            .plan(Path::new(CREWD), &home.path().join(".crew"), false)
            .unwrap();
        assert!(plan.reloads());
        assert_eq!(plist_text(home.path()), before);
        assert_eq!(fake.calls(), ["launchctl print gui/501/dev.crew.crewd"]);
    }

    #[test]
    fn a_changed_loaded_agent_is_written_and_reloaded_together() {
        let home = installed("/Old/Crew.app/Contents/MacOS/crewd");
        let fake = Fake {
            job: Some(Some(812)),
            bootstrap_busy: Cell::new(2),
            ..Fake::default()
        };
        let ld = launchd(&fake, home.path());
        let plan = ld
            .plan(Path::new(CREWD), &home.path().join(".crew"), false)
            .unwrap();
        ld.apply(&plan, false).unwrap();
        let text = plist_text(home.path());
        assert!(text.contains(CREWD));
        assert!(
            text.contains("<string>/usr/bin:/usr/local/bin:"),
            "PATH kept"
        );
        let bootstrap = format!("launchctl bootstrap gui/501 {}", ld.plist_file().display());
        assert_eq!(
            fake.calls(),
            [
                "launchctl print gui/501/dev.crew.crewd".to_string(),
                "launchctl bootout gui/501/dev.crew.crewd".into(),
                bootstrap.clone(),
                bootstrap.clone(),
                bootstrap,
                "launchctl kickstart gui/501/dev.crew.crewd".into(),
            ]
        );
    }

    #[test]
    fn bootstrap_gives_up_after_a_few_tries() {
        let home = installed("/Old/Crew.app/Contents/MacOS/crewd");
        let fake = Fake {
            job: Some(Some(812)),
            bootstrap_busy: Cell::new(100),
            ..Fake::default()
        };
        let ld = launchd(&fake, home.path());
        let plan = ld
            .plan(Path::new(CREWD), &home.path().join(".crew"), false)
            .unwrap();
        let err = ld.apply(&plan, false).unwrap_err();
        assert_eq!(
            err.to_string(),
            "launchctl bootstrap failed: Bootstrap failed: 5: Input/output error"
        );
        let tries = fake
            .calls()
            .iter()
            .filter(|c| c.contains("bootstrap"))
            .count();
        assert_eq!(tries, BOOTSTRAP_TRIES);
    }

    #[test]
    fn restart_kills_and_starts_a_loaded_unchanged_agent() {
        let home = installed(CREWD);
        let fake = Fake {
            job: Some(Some(812)),
            ..Fake::default()
        };
        let ld = launchd(&fake, home.path());
        let plan = ld
            .plan(Path::new(CREWD), &home.path().join(".crew"), false)
            .unwrap();
        ld.apply(&plan, true).unwrap();
        assert_eq!(
            fake.calls().last().map(String::as_str),
            Some("launchctl kickstart -k gui/501/dev.crew.crewd")
        );

        // A reload already restarts it.
        let home = installed("/Old/Crew.app/Contents/MacOS/crewd");
        let fake = Fake {
            job: Some(Some(812)),
            ..Fake::default()
        };
        let ld = launchd(&fake, home.path());
        let plan = ld
            .plan(Path::new(CREWD), &home.path().join(".crew"), false)
            .unwrap();
        ld.apply(&plan, true).unwrap();
        assert!(fake.calls().iter().all(|c| !c.contains("kickstart -k")));
    }

    /// Restart asks the shell again, so a changed PATH is picked up.
    #[test]
    fn restart_refreshes_the_path() {
        let home = installed(CREWD);
        let fake = Fake {
            job: Some(Some(812)),
            shell_out: Some("/work/new/bin:/usr/bin".into()),
            ..Fake::default()
        };
        let ld = launchd(&fake, home.path());
        let plan = ld
            .plan(Path::new(CREWD), &home.path().join(".crew"), true)
            .unwrap();
        assert!(plan.reloads());
        ld.apply(&plan, true).unwrap();
        assert!(plist_text(home.path()).contains("<string>/work/new/bin:/usr/bin:"));
        assert_eq!(fake.calls()[0], SHELL_CALL);
    }

    #[test]
    fn a_failed_print_is_not_taken_for_not_loaded() {
        let home = tempfile::tempdir().unwrap();
        let fake = Fake {
            shell_out: Some("/usr/bin".into()),
            print_fails: true,
            ..Fake::default()
        };
        let err = launchd(&fake, home.path())
            .plan(Path::new(CREWD), &home.path().join(".crew"), false)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "launchctl print failed: Operation not permitted"
        );
    }

    #[test]
    fn a_loaded_job_that_isnt_running_has_no_pid() {
        let home = installed(CREWD);
        let fake = Fake {
            job: Some(None),
            ..Fake::default()
        };
        let plan = launchd(&fake, home.path())
            .plan(Path::new(CREWD), &home.path().join(".crew"), false)
            .unwrap();
        assert!(plan.loaded);
        assert_eq!(plan.job_pid, None);
    }

    #[test]
    fn a_failed_launchctl_says_what_it_printed() {
        let home = tempfile::tempdir().unwrap();
        let fake = Fake {
            shell_out: Some("/usr/bin".into()),
            fail: HashSet::from(["kickstart"]),
            ..Fake::default()
        };
        let ld = launchd(&fake, home.path());
        let plan = ld
            .plan(Path::new(CREWD), &home.path().join(".crew"), false)
            .unwrap();
        let err = ld.apply(&plan, false).unwrap_err();
        assert_eq!(
            err.to_string(),
            "launchctl kickstart failed: Input/output error"
        );
    }

    #[test]
    fn system_runner_reports_output() {
        let out = SystemRunner
            .run("/bin/sh", &["-c".into(), "printf hi; exit 3".into()], None)
            .unwrap();
        assert_eq!(out.stdout, "hi");
        assert_eq!(out.code, Some(3));
        assert!(!out.success);
    }

    /// The shell and whatever it started are killed, so nothing holds
    /// the output open.
    #[test]
    fn system_runner_kills_the_whole_group_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        let started = Instant::now();
        let err = SystemRunner
            .run(
                "/bin/sh",
                &["-c".into(), script],
                Some(Duration::from_millis(300)),
            )
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
            assert!(Instant::now() < deadline, "the background sleep survived");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
