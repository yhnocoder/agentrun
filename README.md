# agentrun

[![CI](https://github.com/yhnocoder/agentrun/actions/workflows/ci.yml/badge.svg)](https://github.com/yhnocoder/agentrun/actions/workflows/ci.yml)

Run claude-code, codex and pi in a sandbox with one command, on macOS, Linux, cloud containers and docker.

## What agentrun does

A program that needs a coding agent calls agentrun with a runtime and a prompt. The command is the same on a macOS or Linux machine, in a cloud managed container and in a docker container. agentrun handles the rest:

- It starts the runtime in a sandbox. The agent can write only to the working directory and to a temporary directory for the session, and its commands cannot reach the network unless `--network` allows it.
- It keeps the agent away from the user's own runtime configuration, plugins, rules and session history.
- It converts the different output of the three runtimes into one event stream and decides success or failure in the same way for all of them. The exit code reports the result: 0 when the agent finished, 1 when it failed, 2 when agentrun refused to start it.
- It checks before starting that the runtime, its login and the sandbox can be used. `agentrun doctor` runs the same checks on their own, and every failed check says what to fix.

## Supported runtimes and environments

| Runtime | Command | Minimum version |
|---|---|---|
| claude-code | `agentrun claude-code` | 2.1.288 |
| codex | `agentrun codex` | 0.159.2 |
| pi (npm package `@earendil-works/pi-coding-agent`) | `agentrun pi` | 1.0.1 |

| Environment | Sandbox | What the caller adds |
|---|---|---|
| macOS | Seatbelt, part of the system | nothing |
| Linux | bubblewrap (`bwrap`); claude-code and pi also need `socat` | nothing |
| Cloud managed container, such as Claude Code on the web | bubblewrap, as on Linux; the container needs `bwrap` and `socat` and must allow user namespaces | nothing |
| docker container | none, because a default docker container cannot create user namespaces; the container is the isolation boundary | `--sandbox off`, or `AGENTRUN_SANDBOX=off` |

codex uses its own sandbox on both macOS and Linux. agentrun does not detect which environment it runs in: before every run it starts the sandbox once, and with the default `--sandbox on` it refuses to run when the sandbox cannot start.

agentrun is a single executable for macOS and Linux on x86_64 and arm64. Windows is not supported.

## Install

Download a release file. Replace `linux-x86_64` with `linux-arm64`, `macos-arm64` or `macos-x86_64` to match your machine:

```sh
VERSION=1.0.0
FILE=agentrun-$VERSION-linux-x86_64
curl -LO https://github.com/yhnocoder/agentrun/releases/download/v$VERSION/$FILE
chmod +x $FILE
mkdir -p ~/.local/bin
mv $FILE ~/.local/bin/agentrun
```

If `~/.local/bin` is not in your `PATH` (it is not by default on macOS), add `export PATH="$HOME/.local/bin:$PATH"` to your shell's startup file.

On macOS, a file downloaded with a browser carries the quarantine attribute and the system refuses to run it. Remove the attribute after moving the file:

```sh
xattr -d com.apple.quarantine ~/.local/bin/agentrun
```

Install from crates.io:

```sh
cargo install agentrun --locked
```

Build from source:

```sh
git clone https://github.com/yhnocoder/agentrun
cd agentrun
cargo build --release
cp target/release/agentrun ~/.local/bin/
```

On Linux, install the sandbox tools:

```sh
# Debian, Ubuntu
sudo apt-get install -y bubblewrap socat
# Fedora
sudo dnf install -y bubblewrap socat
```

Ubuntu 23.10 and later do not allow ordinary programs to create user namespaces by default, so `bwrap` fails to start until it has its own AppArmor profile. The steps are in [Sandbox and network](https://yhnocoder.github.io/agentrun/pages/isolation.html#apparmor).

Install and log in to the runtimes following their own documentation. agentrun finds them in `PATH` and uses the existing login. In a container without the user's login files, pass the login through environment variables, as described in the [user guide](https://yhnocoder.github.io/agentrun/pages/guide.html#containers).

## Quick start

Check the environment first. Each line is one check of one runtime:

```console
$ agentrun doctor claude-code pi
[ok]   claude-code  executable  /opt/node22/bin/claude
[ok]   claude-code  login       oauth_token
[ok]   claude-code  sandbox     bubblewrap: wrote inside, blocked outside
[skip] claude-code  network     --network none
[ok]   pi           executable  /opt/node22/bin/pi
[ok]   pi           login       deepseek/deepseek-flash (from pi settings)
[ok]   pi           sandbox     bubblewrap: wrote inside, blocked outside, pi started
[skip] pi           network     --network none
```

Run an agent in the current directory. When standard output is a terminal, agentrun shows the `rich` format by default: completed steps scroll above a status bar that shows the model, the running tool, the elapsed time and the token usage. This is the screen while claude-code is running:

```console
$ agentrun claude-code --prompt "Create hello.txt that contains the word hello, then reply in one short sentence"
[main] prompt Create hello.txt that contains the word hello, then reply in one short sentence
[main] tool Write: hello.txt
[main] text I created `/tmp/demo/hello.txt` containing the word "hello".
───────────────────────────────────────────────────────────────────────────────
⠇ main  claude-sonnet-5-5  0:03  in 16.3k  cached 76%  ctx 16.3k
```

When the run ends, the status bar is replaced by the end line:

```console
[end] finished 5.2s  in 33.6k  out 0.1k  cached 85%
```

`--format text` writes the same lines without the status bar. The label in brackets names the agent: `main` is the main agent, and a subagent is shown by its type and a number. The `[end]` line gives the result, the duration and the token usage.

```console
$ agentrun pi --format text --prompt "Count the lines in hello.txt with wc, then reply in one short sentence"
[main] prompt Count the lines in hello.txt with wc, then reply in one short sentence
[main] tool bash: wc -l hello.txt
[main] text hello.txt contains 1 line.
[end] finished 5.2s  in 4.5k  out 90  cached 91%
```

`--format jsonl` writes one JSON event per line, for programs that call agentrun. It is the default when standard output is not a terminal. The `start` event records the sandbox, the network mode and the full command of the runtime; the `network` event shows that pi reached its model service through the filter proxy; the `end` event gives the status and the total usage.

```console
$ agentrun pi --format jsonl --prompt "Count the lines in hello.txt with wc, then reply in one short sentence"
{"schema":1,"type":"start","time":"2026-10-06T03:19:46.563Z","runtime":"pi","sandbox":"bubblewrap","network":{"mode":"none","allow":[],"enforced":true},"model":null,"cwd":"/tmp/demo","argv":["/usr/bin/bwrap","--ro-bind","/","/","--bind","/tmp/demo","/tmp/demo","--bind","/tmp/agentrun-LkywMR","/tmp/agentrun-LkywMR","--dev","/dev","--proc","/proc","--die-with-parent","--unshare-net","--","/bin/sh","-c","\"$0\" \"TCP-LISTEN:$1,bind=127.0.0.1,fork,reuseaddr\" \"UNIX-CONNECT:$2\" 2>/dev/null & shift 2; exec \"$@\"","/usr/bin/socat","34501","/tmp/agentrun-LkywMR/proxy.sock","/opt/node22/bin/pi","-p","--mode","json","--no-session","--no-extensions","--no-skills","--no-prompt-templates","--no-themes","--no-context-files","--no-approve","--offline","--tools","read,bash,edit,write,grep,find,ls","--"],"env":[]}
{"schema":1,"type":"prompt","time":"2026-10-06T03:19:47.077Z","text":"Count the lines in hello.txt with wc, then reply in one short sentence"}
{"schema":1,"type":"network","time":"2026-10-06T03:19:47.208Z","host":"api.deepseek.com","port":443,"allowed":true,"reason":null}
{"schema":1,"type":"usage","time":"2026-10-06T03:19:48.758Z","parent":null,"model":"deepseek/deepseek-flash","input_tokens":165,"output_tokens":79,"cache_read_tokens":2048,"cache_write_tokens":0,"context_tokens":2213}
{"schema":1,"type":"tool","time":"2026-10-06T03:19:48.777Z","id":"call_00_fKSpSPw3Qpm6M9pkhl9O6002","parent":null,"name":"bash","summary":"bash: wc -l hello.txt","denied":false}
{"schema":1,"type":"text","time":"2026-10-06T03:19:50.410Z","parent":null,"text":"hello.txt contains 1 line."}
{"schema":1,"type":"usage","time":"2026-10-06T03:19:50.410Z","parent":null,"model":"deepseek/deepseek-flash","input_tokens":260,"output_tokens":8,"cache_read_tokens":2048,"cache_write_tokens":0,"context_tokens":2308}
{"schema":1,"type":"end","time":"2026-10-06T03:19:51.011Z","status":"finished","exit_code":0,"detail":"","duration_ms":4459,"usage":{"input_tokens":425,"output_tokens":87,"cache_read_tokens":4096,"cache_write_tokens":0,"by_model":{"deepseek/deepseek-flash":{"input_tokens":425,"output_tokens":87,"cache_read_tokens":4096,"cache_write_tokens":0}}},"result":"hello.txt contains 1 line."}
```

codex takes the same options:

```sh
agentrun codex --prompt "Count the lines in hello.txt with wc, then reply in one short sentence"
```

## Documentation

- [Quick start](https://yhnocoder.github.io/agentrun/pages/quickstart.html): install agentrun, check the environment and run a first task.
- [User guide](https://yhnocoder.github.io/agentrun/pages/guide.html): network access, environment variables and secrets, time limits, docker and cloud containers, and common problems.
- [Developer documentation](https://yhnocoder.github.io/agentrun/): how each mechanism works and why it was chosen, including every option, event and exit code.

## Development

See [CONTRIBUTING.md](https://github.com/yhnocoder/agentrun/blob/main/CONTRIBUTING.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](https://github.com/yhnocoder/agentrun/blob/main/LICENSE-APACHE))
- MIT license ([LICENSE-MIT](https://github.com/yhnocoder/agentrun/blob/main/LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in agentrun by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
