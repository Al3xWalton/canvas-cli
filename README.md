# canvas-cli

A command-line tool for Canvas and Panopto ReCap, with an MCP server so agents
can use it. It reads course content, deadlines and grades, downloads course
files and lecture recordings, and saves lecture transcripts for note writing.
It only reads Canvas: it never submits, posts or changes anything there.

It signs in with your browser, so it works where students cannot create
Canvas API tokens.

```sh
$ canvas assignments --due 2w
Fri 13 Nov 12:00  ABC1016 (26/27)             Good Academic Practice - Online Quiz
Mon 23 Nov 14:00  ABC1020 (26/27)             Assignment Part 2

$ canvas recap transcript ABC1016 --latest 1
2 file(s) saved, 0 without captions, 0 failed
Transcripts are in ~/Canvas/ABC1016 (26_27)/ReCap
```

## Requirements

- [Rust](https://www.rust-lang.org/tools/install) 1.95.0, pinned by `rust-toolchain.toml`
- Google Chrome, for `canvas auth login`
- Optional: [ffmpeg](https://ffmpeg.org), only for `recap download --streams`

macOS is the tested platform. The session is stored in the macOS keychain.

## To install

```sh
git clone https://github.com/Al3xWalton/canvas-cli && cd canvas-cli
make install
```

This installs two binaries, `canvas` and `canvas-mcp`, into `~/.cargo/bin`.

On macOS, run `make setup-signing` once before `make install`. It creates a
local signing certificate so that, after you click **Always Allow** once per
binary, updates do not ask for your keychain password again.

## To run

Sign in once. A Chrome window opens: complete your university sign-in and MFA,
then sign in to ReCap if it asks.

```sh
canvas auth login
canvas auth status
```

Then ask Canvas things:

```sh
canvas courses
canvas assignments --due 7d
canvas calendar --days 7d
canvas announcements --since 14d
canvas modules ABC1020
canvas pages ABC1020 module-information
canvas grades
canvas digest                      # what is new since the last run, plus what is due
```

Name a course by id, code (`ABC1020`) or part of its name (`"example studies"`).
Add `--json` to any command for machine-readable output.

Download things into `~/Canvas` (change it in the config):

```sh
canvas files sync ABC1020          # new and changed files only
canvas files sync --all
canvas recap list ABC1016
canvas recap download ABC1016 --latest 1
canvas recap transcript ABC1016 --latest 3
canvas recap transcript <viewer-url> --stdout
```

When the session expires, commands fail with
`Canvas session expired or missing. Run canvas auth login.` Run it again.

## Options

| Command | What it does |
|---|---|
| `auth login / status / logout` | Sign in through Chrome, check, or forget the session |
| `courses` | Active courses |
| `assignments [--due 7d] [--course X]` | Work due soon, with submission state |
| `calendar [--days 7d] [--course X]` | Timetabled events and dated assignments |
| `announcements [--since 14d] [--course X]` | Announcements as Markdown |
| `modules <course>` | Modules and their items |
| `pages <course> [page]` | Pages, or one page as Markdown |
| `discussions <course>` | Discussion topics with reply counts |
| `quizzes <course>` | Classic quiz details (New Quizzes appear under `assignments`) |
| `grades [course]` | Course totals, or one course's results |
| `digest [--since last\|2d] [--upcoming 7d]` | Changes since the last run, plus what is due |
| `files list / sync` | List, or incrementally download, course files |
| `recap list <course>` | A course's lecture recordings |
| `recap info <recording>` | Length, chapters, captions |
| `recap download <recording\|course> [--latest N] [--streams]` | MP4 download |
| `recap transcript <recording\|course> [--latest N] [--stdout]` | `.srt` and readable `.txt` with chapter headings |
| `config` | Show the active settings |
| `mcp install --client claude\|codex\|cursor` | Print the setup for an agent client |

### Config

`~/.config/canvas-cli/config.toml`, every field optional:

```toml
canvas_url = "https://canvas.example.edu"
panopto_host = "example.hosted.panopto.com"
download_root = "/Users/you/Canvas"
```

## Connect an agent

`canvas-mcp` is a stdio MCP server that runs on the session from
`canvas auth login`. Add it to your client once:

```sh
canvas mcp install --client claude   # prints: claude mcp add --scope user canvas -- '<path>/canvas-mcp'
```

It offers the same reads and downloads as the CLI, plus a `digest` tool for
notifications and `get_transcript` for lecture notes. A guide to the tools is
sent to the agent when it connects. If the session has expired, the server
exits at start-up and the client shows it as failed. Run `canvas auth login`,
then reconnect.

## How it works

- **Canvas:** the read-only REST API (`/api/v1`), authenticated with the
  browser session captured at sign-in.
- **ReCap:** Panopto's viewer endpoint (`DeliveryInfo.aspx`) gives each
  recording's composite MP4 and its per-source HLS streams, and
  `GenerateSRT.ashx` gives captions. The approach was learned from
  [travbend/panopto-downloader](https://github.com/travbend/panopto-downloader);
  no code was taken from it.
- **Security:**
  - The session is stored only in the keychain and never printed.
  - Cookies are sent only to the Canvas and Panopto hosts, never to download CDNs.
  - Every name from Canvas is sanitised before it touches disk.
  - Text written on Canvas is marked as untrusted for agents.

Use it only for content you are allowed to access, and follow your
university's policies on recordings.

## License

AGPL-3.0-only. See `LICENSE.md`.
