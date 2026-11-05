# canvas-cli

A command-line tool for Canvas and Panopto ReCap, with an MCP server so
agents can use it. It reads course content, downloads files and lecture
recordings, and reports what is due. It only reads: it never changes
anything on Canvas.

Work in progress.

## Requirements

- [Rust](https://www.rust-lang.org/tools/install) 1.95.0, pinned by `rust-toolchain.toml`
- [ffmpeg](https://ffmpeg.org), for ReCap downloads
- Google Chrome, for `canvas auth login`

## To install

```sh
git clone https://github.com/Al3xWalton/canvas-cli && cd canvas-cli
make install
```

## To run

```sh
canvas config
```

## License

AGPL-3.0-only. See `LICENSE.md`.
