# canvas-cli

An unofficial command-line tool and MCP server for Canvas and Panopto ReCap.
`canvas` reads your courses, deadlines, announcements, pages and grades,
downloads course files and lecture recordings, and saves lecture transcripts
with chapter headings for note writing. `canvas-mcp` gives the same tools to
an AI agent, so it can answer questions like `what's due this week` or
`write notes from Tuesday's lecture`. It signs in through your browser, so it
works where students can't create Canvas API tokens, and it only reads:
nothing is ever submitted or changed on Canvas.

**Set it up with an agent.** Paste this into Claude Code, Codex or Cursor:

```
Download and set up the unofficial Canvas CLI and its MCP server by following https://github.com/Al3xWalton/canvas-cli/blob/main/SETUP.md
```

## Requirements

- macOS
- [`rust`](https://www.rust-lang.org/tools/install)
- Google Chrome (for sign-in)
- [`ffmpeg`](https://ffmpeg.org), optional (only for `recap download --streams`)

## To install

Clone this repository

```bash
git clone https://github.com/Al3xWalton/canvas-cli && cd ./canvas-cli
```

Create a local signing certificate (once), then build and install `canvas`
and `canvas-mcp` to `~/.cargo/bin`

```bash
make setup-signing && make install
```

Sign in. A Chrome window opens for your university sign-in and MFA, then ReCap

```bash
canvas auth login
```

The defaults are for the university. For another university, set
`canvas_url` and `panopto_host` in `~/.config/canvas-cli/config.toml`.

See what's due

```bash
canvas assignments --due 7d
```

Write a lecture's transcript to the terminal

```bash
canvas recap transcript <viewer-url> --stdout
```

Connect an agent (prints the exact command or config for your client)

```bash
canvas mcp install --client claude
```

## Commands

Name a course by id, code (`ABC1020`) or part of its name. Add `--json` to any
command for machine-readable output.

- `courses`, `assignments --due 7d`, `calendar --days 7d`, `grades [course]`: courses, deadlines, timetable and grades.
- `announcements --since 14d`, `modules <course>`, `pages <course> [page]`, `discussions <course>`, `quizzes <course>`: course content as Markdown.
- `digest`: what's new since the last run (announcements, assignments, files, recordings), plus what's due soon.
- `files sync <course|--all>`: download new and changed course files to `~/Canvas`.
- `recap list <course>`, `recap info <recording>`: a course's lecture recordings, with chapters.
- `recap download <recording|course> --latest N`: save lectures as MP4. Interrupted downloads resume.
- `recap transcript <recording|course> [--language all]`: save `.srt` and readable `.txt` transcripts.
- `recap file <course> --into <Lectures folder> --apply`: file recordings into your own `L01 - ...` lecture folders. Without `--apply` it only shows the plan.
- `auth login [--recap-only] | status | logout`: manage the browser session, kept in the macOS keychain.
- `--help`: see every option.

## Notes

- Not affiliated with or endorsed by Instructure (Canvas) or Panopto. Use it
  only for content you can already access, and follow your university's
  policies on recordings.
- The ReCap download approach was learned from
  [travbend/panopto-downloader](https://github.com/travbend/panopto-downloader).
  No code was taken from it.
- AGPL-3.0-only. See `LICENSE.md`.
