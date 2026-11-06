# Updating canvas-cli (for agents)

You are updating an installed **canvas-cli** (`canvas` and `canvas-mcp`) to
the newest version. Follow these steps in order. Run each command, check its
result, and stop with a clear message to the user if a step fails.

The user's settings, sign-in and downloads are kept: updating replaces only
the two programs.

## 1. Find the code

```bash
cd ~/canvas-cli && git remote get-url origin
```

This should print `https://github.com/Al3xWalton/canvas-cli`. If
`~/canvas-cli` does not exist, ask the user where they cloned canvas-cli. If
they never installed it, follow
https://github.com/Al3xWalton/canvas-cli/blob/main/SETUP.md instead and stop
here.

## 2. Check for a new version

```bash
git fetch origin main && git log --oneline HEAD..origin/main
```

If this prints nothing, tell the user canvas-cli is already up to date and
stop. Otherwise, show the user the list: it is what changed.

## 3. Get the new code

```bash
git pull --ff-only origin main
```

If this fails because of local changes, tell the user and stop. Do not
discard their changes.

## 4. Build and install

```bash
make install
```

This rebuilds `canvas` and `canvas-mcp` into `~/.cargo/bin` and signs them,
so macOS keeps its keychain permission. It takes a few minutes. If it says
the binaries were not signed, run `make setup-signing && make install`.

Check it:

```bash
~/.cargo/bin/canvas --version && ~/.cargo/bin/canvas auth status
```

If `auth status` reports an expired session, ask the user to run
`canvas auth login`.

## 5. Reload the MCP server

The running `canvas` MCP server is still the old version. Tell the user to
restart or reconnect their client (in Claude Code: `/mcp`, then reconnect
`canvas`). Once reconnected, call `usage` to read the guide, which may list
new tools.

Tell the user the update is done, and summarise the changes from step 2.
