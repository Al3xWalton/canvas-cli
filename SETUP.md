# Setting up canvas-cli (for agents)

You are installing **canvas-cli**, an unofficial, read-only command-line tool
for Canvas and Panopto ReCap, and connecting its MCP server (`canvas-mcp`) to
yourself. Follow these steps in order. Run each command, check its result, and
stop with a clear message to the user if a step fails. Do not skip the checks.

The user must do two things themselves: accept the terms in step 0, and sign
in through a browser window in step 6. Everything else you can do.

## 0. Show the user the terms, and get their agreement

Before installing anything, show the user this, word for word:

> canvas-cli is an unofficial tool, provided as is, without warranty. You need
> explicit permission from your university to access, download and process its
> content this way (including lecture recordings, course files and
> transcripts). You are responsible for following your university's terms,
> policies and copyright rules, and the terms of Canvas and Panopto. You use it
> entirely at your own risk; the author takes no responsibility for how it is
> used. Do you have that permission and accept these terms?

Continue only if the user clearly says yes. If they do not, stop and do not
install anything.

## 1. Check the platform

```bash
uname -s
```

This must print `Darwin` (macOS). On anything else, tell the user that only
macOS is supported for now and stop.

## 2. Check the requirements

```bash
cargo --version && git --version && ls "/Applications/Google Chrome.app" >/dev/null && echo ok
```

- If `cargo` is missing, ask the user before installing Rust. Then run
  `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y`
  followed by `. "$HOME/.cargo/env"`.
- If Google Chrome is missing, ask the user to install it from
  https://www.google.com/chrome/ and wait.
- `ffmpeg` is optional. It is only needed for `recap download --streams`.

## 3. Get the code

```bash
git clone https://github.com/Al3xWalton/canvas-cli ~/canvas-cli && cd ~/canvas-cli
```

If `~/canvas-cli` already exists, run `cd ~/canvas-cli && git pull --ff-only`
instead.

## 4. Sign and install

```bash
make setup-signing && make install
```

`make setup-signing` creates a local code-signing certificate once, so macOS
remembers keychain access across updates. It needs no password. `make install`
builds `canvas` and `canvas-mcp` into `~/.cargo/bin` and signs them. The build
takes a few minutes.

Check that it worked:

```bash
~/.cargo/bin/canvas --version && ~/.cargo/bin/canvas-mcp --version
```

## 5. Point it at the user's university

The tool has no built-in university. Ask the user for two addresses:

1. **Canvas:** the address they open Canvas at, e.g. `https://canvas.example.edu`.
2. **ReCap (Panopto):** the address of any recorded lecture they can watch
   (open one from a course's ReCap tab and copy the link), or just its host,
   e.g. `example.hosted.panopto.com`.

Then save them:

```bash
~/.cargo/bin/canvas setup --canvas-url "<canvas address>" --recap-url "<recap address>"
```

Check the settings with `~/.cargo/bin/canvas config`. Tell the user the tool
has only been tested at one university and is provided as is.

## 6. Sign in (the user does this)

Tell the user: *"A Chrome window will open. Sign in to Canvas with your
university account and MFA, then sign in to ReCap if it asks."* Then run:

```bash
~/.cargo/bin/canvas auth login
```

It waits up to 5 minutes for each sign-in. macOS may ask for keychain access.
The user should click **Always Allow**.

Check it:

```bash
~/.cargo/bin/canvas auth status
```

This should print `Signed in to ... as <name>` and `ReCap session  yes`.

## 7. Connect the MCP server to yourself

Pick the line for the client you are running in:

```bash
~/.cargo/bin/canvas mcp install --client claude   # Claude Code
~/.cargo/bin/canvas mcp install --client codex    # OpenAI Codex CLI
~/.cargo/bin/canvas mcp install --client cursor   # Cursor, or any client using mcpServers JSON
```

It prints the exact command or config to use. For Claude Code, run the
`claude mcp add ...` line it prints. For the others, add the printed snippet
to the named config file. Then tell the user to restart or reconnect the
client so the `canvas` server loads.

## 8. Verify

Once reconnected, call the `auth_status` tool. It should return
`signed_in: true`. Then call `usage` to read the guide to the tools.

Tell the user setup is done and suggest a first question, such as *"What's due
in the next 7 days?"*

## If something fails later

- `Canvas session expired or missing. Run canvas auth login.`: ask the user to
  run `canvas auth login` again, then reconnect the server.
- `canvas auth login --recap-only` renews only ReCap.
- The `canvas` MCP server shows as failed: the session has expired. Same fix.
