# wetype-hex

A fork of [anomalyco/hex](https://github.com/anomalyco/hex) whose speech
engine is **WeChat Input Method's cloud ASR** instead of a local speech model.

Hold a shortcut, speak, let go: the audio goes to
`wss://wetype.weixin.qq.com/` over the reverse-engineered WeType protocol, and
the recognized, punctuated text is pasted into the focused app. Nothing is
downloaded, no model file is kept on disk, and no GPU is required — the
server also adds punctuation and turns spoken phrasing into written Chinese.

The protocol client is a native Rust port of
[`wetype-asr`](https://github.com/Howardzhangdqs/wetype-asr) (WSS + protobuf +
Snappy + AES-256-ECB + secp128r1 ECDH + Opus); no Python, ffmpeg, or libopus
runtime is needed at use time.

## Status

- **Linux** is the tested target (X11 and wlroots-compatible Wayland).
- The macOS and Windows code paths still compile, but this fork's backend has
  only been exercised on Linux.
- The service recognizes **Mandarin Chinese** (with mixed English words).
- **Automatic 文字整理**: each Linux dictation is run through the service's
  text-tidy model after recognition and before it is pasted, so spoken lists
  and plans come out structured. It is on by default; turn it off with
  `hex settings set wetype_tidy off` (General › Tidy dictation). A failed tidy
  pastes the untidied text. File transcription and meetings keep their raw
  recognition text.
- No word timestamps: a timed request gets one segment per clip.
- **Model downloading is disabled** in this build. The local GGUF catalog and
  its code stay compiled for the rest of Hex (meetings, history, imports), but
  no UI or CLI command downloads or selects a model.
- Updates are disabled: an upstream Hex update would replace this backend.

## Build

```sh
cargo build --release --bin hex

./target/release/hex start          # open the window (starts it if needed)
./target/release/hex                # same, when it is not running
./target/release/hex transcribe recording.m4a
./target/release/hex quit
```

Add it to the application menu (copies the release binary to
`~/.local/bin/wetype-hex` and writes a launcher):

```sh
./packaging/linux/install-desktop-entry.sh
```

Ubuntu/Debian build dependencies: Rust 1.85+, `cmake`, `clang`, and the
regular X11/Wayland development packages Hex lists in
[`docs/linux.md`](docs/linux.md). The local engine builds CPU-only in this fork
(no `glslc`/Vulkan needed); see [`.cargo/config.toml`](.cargo/config.toml).

## Command line

`hex transcribe meeting.m4a` prints a recording's transcript, whether or not
the app is running. It reads WAV, AIFF, CAF, FLAC, MP3, Ogg, WebM, and
MP4/M4A/MOV, including a video's soundtrack. The same `hex` controls the
running app (`hex status`, `hex dictation`, `hex settings`); see
[`docs/cli.md`](docs/cli.md).

The first run registers an anonymous UIN with the service and caches it in
`~/.cache/wetype-asr/identity.json` (the same file `wetype-asr` uses), so
later launches skip registration.

## How it works

- `src/wetype/` — the protocol client: protobuf envelope, handshake and
  request signing, secp128r1 ECDH, AES-256-ECB/PKCS#7, Snappy, 20 ms Opus
  frames, and the `/api_v2` voice and text-polish requests.
- `src/transcription/wetype.rs` — the engine adapter. It exposes the same
  surface as a local model, so dictation, meetings, the local API, and
  `hex transcribe` use it unchanged.
- `src/transcription/models.rs` — `TranscriptionModelId::WeType` and
  `ModelRuntime::WeType`, the default and only listed selection; model
  downloads short-circuit (`CLOUD_ONLY`).

Deeper protocol notes live in the `wetype-asr` repository's
[`docs/protocol.md`](https://github.com/Howardzhangdqs/wetype-asr/blob/main/docs/protocol.md).

## Relationship to upstream Hex

MIT licensed, like upstream. `upstream` remains a git remote, so the fork can
take fixes; `docs/` and most of the app are unchanged. This fork adds the
WeType backend and turns off model downloads and self-updates.
