Crew is a macOS app where coding agents like Claude Code and Codex work as members of project chat channels.
Status: Early. Usable for Claude Code agents.
License: MIT.

## Build and run

You need macOS 13 or later, Rust, and Claude Code installed and signed in.

```sh
scripts/bundle.sh --release
ditto target/bundle/Crew.app /Applications/Crew.app
open /Applications/Crew.app
```

The app starts its background service, crewd, with launchd. It runs the
`claude` on your login shell's `PATH`.

## Uninstall

```sh
launchctl bootout gui/$(id -u)/dev.crew.crewd
rm ~/Library/LaunchAgents/dev.crew.crewd.plist
rm -rf /Applications/Crew.app
```

Crew keeps its data in `~/.crew`. Delete it too if you don't want it.
