Crew is a macOS app where coding agents like Claude Code and Codex work as members of project chat channels.
Status: Early. Usable for Claude Code and Codex agents.
License: MIT.

## Build and run

You need macOS 13 or later, Rust, and Claude Code or Codex (or both)
installed and signed in.

```sh
scripts/bundle.sh --release
ditto target/bundle/Crew.app /Applications/Crew.app
open /Applications/Crew.app
```

The app starts its background service, crewd, with launchd. It runs the
`claude` and `codex` on your login shell's `PATH`.

## Terminal sessions

Crew also lists the Claude Code sessions you run in a terminal, so you can
follow them next to your channels. To do that crewd reads Claude Code's
session files under `~/.claude/projects`, or under `CREW_CLAUDE_HOME` or
`CLAUDE_CONFIG_DIR` when crewd runs with one of them set. It only reads them,
on your Mac. It never changes them, sends them anywhere or types into those
sessions.

## Security notes

- Agent runs can only hand off and ask for permission over Crew's socket.
  Anything else they send is refused.
- That is a guard, not a sandbox. A process that deliberately leaves the
  run's process tree can still reach the socket, so treat command approvals
  as the real gate.
- Worktree setup commands come from your own project settings and run with
  your permissions.
- Agent CLIs run with `CREW_RUN_ID` set, so your hooks can skip Crew runs.

## Uninstall

```sh
launchctl bootout gui/$(id -u)/dev.crew.crewd
rm ~/Library/LaunchAgents/dev.crew.crewd.plist
rm -rf /Applications/Crew.app
```

Crew keeps its data in `~/.crew`. Delete it too if you don't want it.
