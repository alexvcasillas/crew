Crew is a macOS app where coding agents like Claude Code and Codex work as members of project chat channels.
Status: Early. Not usable yet.
License: MIT.

## Build and run

You need macOS 13 or later, Rust, and Claude Code installed and signed in.

```sh
scripts/bundle.sh --release
open target/bundle/Crew.app
```

The app starts its background service, crewd, with launchd. It runs the
`claude` on your login shell's `PATH`.
