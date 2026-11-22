# Contributing

Issues and pull requests are welcome. For bigger changes, open an issue first to agree on the approach.

## Setup

You need Rust (stable) and, on Debian or Ubuntu, the ALSA and SBC headers:

```sh
sudo apt-get install libasound2-dev libsbc-dev
cargo test
```

The app only runs on Linux. On Windows, use WSL for building and the sandbox for testing.

## Before a pull request

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

CI also checks licenses (`cargo deny check`), the Dockerfile, shell scripts, spelling and Markdown.

## Testing on real hardware

`scripts/sandbox.sh` runs the app with Home Assistant and Mosquitto on a Linux board over SSH,
isolated in Docker-in-Docker. See [docs/development.md](docs/development.md).

## Pull requests

- One topic per pull request, with a short description of what changes for users.
- Label it `feature`, `bug`, `breaking-change` or `dependencies`: the release notes and the
  next version number come from the labels.
