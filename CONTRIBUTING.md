# Contributing to agentrun

## Development environment

- Rust stable, installed with [rustup](https://rustup.rs).
- [uv](https://docs.astral.sh/uv/), which runs the Python scripts in `scripts/`.
- On Linux, `bubblewrap` and `socat`, which the sandbox tests need:

  ```sh
  # Debian, Ubuntu
  sudo apt-get install -y bubblewrap socat
  # Fedora
  sudo dnf install -y bubblewrap socat
  ```

- On macOS, nothing else. The sandbox uses Seatbelt, which is part of the system.

The tests use fake runtimes and recorded output, so claude-code, codex and pi do not need to be installed to run them.

## Checks

Run all checks before you open a pull request:

```sh
scripts/test.sh
```

It runs `cargo fmt --check`, `cargo clippy` with warnings as errors, `cargo test`, the comment check and, when `shellcheck` is installed, `shellcheck` on the acceptance scripts.

Code that only works on one platform must be compiled only for that platform. To check this, also run clippy for the other platform. On Linux:

```sh
rustup target add aarch64-apple-darwin
cargo clippy --all-targets --target aarch64-apple-darwin -- -D warnings
```

On macOS:

```sh
rustup target add x86_64-unknown-linux-gnu
cargo clippy --all-targets --target x86_64-unknown-linux-gnu -- -D warnings
```

## Code conventions

- Code has no comments and no doc comments. Names and structure carry the meaning. The one exception is code that looks removable or changeable but must stay as it is: it gets one line `why(#N): reason`, where `#N` is the issue that explains it. `scripts/check_comment.py` enforces this.
- Commit messages have the form `<type>(<scope>): <one sentence>`, for example `fix(sandbox): ...`.
- A pull request description has two sections, `## Summary` and `## Main Takeaway`, and ends with `Closes #N`. Summary lists each changed file or module and what it does, one line each. Main Takeaway shows what the change gives the reader: commands with their actual output for command line behavior, and screenshots for documentation pages.

The complete rules are in [CLAUDE.md](https://github.com/yhnocoder/agentrun/blob/main/CLAUDE.md). That file is also the working agreement for the AI agents that work on this repository, and it is written in Chinese.

## Workflow

1. Every change starts from a GitHub issue. For a larger feature, the issue body holds the spec, and the spec is agreed on before implementation starts.
2. The design is in `docs/`, starting from `docs/index.html`, and is published at https://yhnocoder.github.io/agentrun/. A change to user visible behavior updates the design in the same pull request.
3. After changing `docs/`, run the page check and look at the screenshots it writes. In a container without Chrome, add `--browser chromium`:

   ```sh
   uv run scripts/check_design.py
   ```

   It takes desktop, narrow and dark screenshots of each page and reports console errors, failed resources, broken formulas, horizontal overflow and broken relative links.

## Reporting a problem

Open an issue with:

- the output of `agentrun --version`;
- the output of `agentrun doctor --json`;
- the command that shows the problem, and its output.

Two options help find the cause. `--debug` writes diagnostics, including the full command of the runtime, to standard error and keeps the session temporary directory. `--raw FILE` writes the unconverted output of the runtime to `FILE`.

Before you post any output, check that it contains no tokens, API keys or other credentials.

## License of contributions

agentrun is licensed under either of the [Apache License, Version 2.0](https://github.com/yhnocoder/agentrun/blob/main/LICENSE-APACHE) or the [MIT license](https://github.com/yhnocoder/agentrun/blob/main/LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in agentrun by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
