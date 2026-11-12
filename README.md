<h1 align="center">torrential</h1>

<p align="center">Search torrents and watch them while they download, from your browser, your terminal, or your coding agent.</p>

<p align="center">
  <img src="docs/web-player.png" alt="Sintel playing in the browser while it downloads, with its chapters and the downloaded parts marked on the seek bar" width="800">
</p>

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/nexxeln/torrential/main/install.sh | sh
```

This puts one file at `~/.local/bin/torrential`. It runs on macOS 14 or later on Apple silicon, and on 64-bit Linux.

<details>
<summary>Build from source</summary>

With Rust and Nix, with flakes turned on:

```sh
git clone --recursive https://github.com/nexxeln/torrential
cd torrential
nix develop -c cargo install --path crates/torrential
```

</details>

## In your browser

```sh
torrential web
```

Search, pick a result, and it plays while it downloads. Jump anywhere and torrential fetches that part first. Subtitles come from the file, or are found online when it has none, and you can shift them when they are out of sync.

<p align="center">
  <img src="docs/web-search.png" alt="Search results for Sintel in the web app, ranked for this Mac" width="800">
</p>

Browsers cannot read every format. Install [ffmpeg](https://ffmpeg.org) (`brew install ffmpeg`) and torrential converts MKV files and surround sound as you watch.

## In your terminal

Run `torrential` to open the terminal UI:

<p align="center">
  <img src="docs/tui.gif" alt="The terminal UI searches for Big Buck Bunny, picks one file, and downloads it while the gauge fills" width="800">
</p>

Or run the commands yourself:

```sh
torrential search big buck bunny         # each result starts with a short ID
torrential get fd69b2 --files '*.mp4'    # download only the files you want
torrential play fd69b2                   # watch in IINA, mpv, or VLC while it downloads
torrential status                        # list your downloads
```

`torrential --help` lists the rest. Every command takes `--json`.

## With your agent

```sh
torrential skill install
```

This teaches Claude Code, Codex, Gemini CLI, Cursor, and other agents to use torrential. Then ask in plain words, such as "download the public-domain film Night of the Living Dead, one MP4 only". Downloads run in a background engine, so they continue after the agent's session ends.

## Settings

Results are ranked for your screen, CPU, and language. `torrential setup` shows what it found and lets you change it.

Everything else is in `~/.config/torrential/config.yaml`:

```yaml
downloads:
  dir: ~/Downloads/torrents   # default: a torrential folder in Downloads
player: mpv                   # for torrential play: iina, mpv, vlc, or a path
subtitles:
  online: false               # do not look for subtitles online
updates: auto                 # install new versions by itself, or off
```

## It never seeds

torrential never uploads a piece, and a download stops as soon as it finishes. It puts lawful sources such as archive.org first among equal matches. Download only what you have the right to.

## License

MIT. See [LICENSE](LICENSE).
