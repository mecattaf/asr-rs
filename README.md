# asr-rs

Streaming local speech-to-text daemon for Wayland/Linux.

asr-rs is a ~1100 LOC Rust daemon that captures microphone audio, streams it over WebSocket to a local [WhisperLiveKit](https://github.com/QuentinFuxa/WhisperLiveKit) + [SimulStreaming](https://github.com/QuentinFuxa/SimulStreaming) inference server, receives cumulative text snapshots, diffs them to extract deltas, runs post-processing (hallucination filter + spoken punctuation + auto-capitalization), and injects the result into the focused window via a fallback chain of output drivers. Toggled on and off by SIGUSR1 from a compositor keybind, with Niri IPC auto-detection for terminal-safe paste mode.

```
 mic
  |
  v
 cpal (PipeWire, 48kHz)
  |
  v
 rubato (FFT polyphase resample 48kHz -> 16kHz mono)
  |
  v
 ringbuf (lock-free SPSC ring buffer, f32)
  |
  v
 drain_s16le (f32 -> signed 16-bit LE PCM)
  |
  v
 WebSocket binary frames (20ms chunks, 640 bytes each)
  |
  v
 WhisperLiveKit + SimulStreaming (ws://localhost:8000/asr)
  |  (AlignAtt simultaneous decoding, large-v3-turbo, ROCm)
  |
  v
 JSON transcript snapshots (cumulative lines[] array)
  |
  v
 SegmentTracker (diff lines to extract text deltas)
  |
  v
 Post-processing pipeline:
  |   1. Hallucination filter (two-tier: phrase blocklist + single-word gate)
  |   2. Spoken punctuation (44 entries, regex replacement)
  |   3. Spacing normalization (left/right-attaching punctuation)
  |   4. Auto-capitalization (after sentence-ending punctuation)
  |   5. Sanitization (strip embedded line terminators)
  |   6. Inter-chunk spacing (smart space prepending)
  |
  v
 Output driver chain (wtype -> dotool -> clipboard, with Niri IPC terminal detection)
  |
  v
 focused window
```

Built for [Niri](https://github.com/YaLTeR/niri) on Fedora Atomic with a Strix Halo iGPU, but works on any Wayland compositor with PipeWire.

---

## Prerequisites

**Host system:**

- PipeWire (or PulseAudio/ALSA -- cpal abstracts over all three)
- One or more injection backends (all optional — the fallback chain tries each in order):
  - **wtype** (wlroots compositors, zero permissions)
  - **dotool** (any Linux session, requires `input` group membership)
  - **wl-copy** (clipboard fallback, any Wayland compositor)
- A Wayland compositor for keybind integration
- Rust toolchain (for building from source)

**Inference server (runs in a Fedora toolbox or container):**

- [WhisperLiveKit](https://github.com/QuentinFuxa/WhisperLiveKit) 0.2.x with SimulStreaming
- PyTorch with ROCm (or CUDA) for GPU-accelerated inference
- Whisper large-v3-turbo model (~3.2 GB VRAM fp16)
- The server must be started with `--pcm-input` to accept raw s16le PCM over WebSocket

**Critical: the `--vac` flag and `is_final` behavior.**
WhisperLiveKit's Silero VAD (VAC) drives segment boundaries. Without VAC, `is_final` / segment-boundary events never fire and the internal segment buffer grows unbounded. In recent WhisperLiveKit versions VAC is ON by default (use `--no-vac` to disable, which is not advised). If you are running an older version, pass `--vac` explicitly. Verify by checking that new lines appear in the `lines[]` array when you pause speaking -- if lines never finalize, VAC is not active.

---

## Building from source

```sh
git clone https://github.com/mecattaf/asr-rs
cd asr-rs
cargo build --release
```

The release binary is at `target/release/asr-rs`. Release profile enables LTO, single codegen unit, panic=abort, and symbol stripping for a small (~1 MB) binary.

To install into `~/.cargo/bin`:

```sh
cargo install --path .
```

---

## Configuration

asr-rs reads a TOML config file from:

```
$XDG_CONFIG_HOME/asr-rs/config.toml
```

Typically `~/.config/asr-rs/config.toml`. If the file does not exist, all defaults are used. Every field is optional.

### Full config reference

```toml
[backend]
# WebSocket URL of the WhisperLiveKit server.
# Default: "ws://localhost:8000/asr"
url = "ws://localhost:8000/asr"

# AlignAtt frame threshold -- not sent to the server by the client,
# but documented here as the primary latency/accuracy knob.
# Configure this on the server side with --frame-threshold.
# 15 = aggressive (faster, more hallucination risk)
# 25 = balanced (default)
# 30 = conservative (slower, more accurate)
# Default: 25
frame_threshold = 25

[audio]
# Audio input device. "default" uses the system default.
# Or a substring of a device name (e.g. "USB" to match "USB Microphone").
# Default: "default"
device = "default"

[injection]
# Ordered list of drivers to try. Each driver is attempted in order;
# if unavailable or injection fails, the next driver is tried.
# Available drivers: "wtype", "dotool", "clipboard", "paste"
# Default: ["wtype", "dotool", "clipboard"]
driver_order = ["wtype", "dotool", "clipboard"]

# Paste keystroke for the "paste" driver.
# Used for terminal-safe injection (copies to clipboard, then simulates paste).
# Default: "ctrl+shift+v"
paste_keys = "ctrl+shift+v"

# App IDs that trigger paste mode instead of direct typing.
# When Niri IPC detects one of these app_ids in the focused window,
# the terminal chain (paste -> clipboard) is used instead of the default chain.
# Default: ["kitty", "foot", "Alacritty"]
terminal_app_ids = ["kitty", "foot", "Alacritty"]

# Enable Niri IPC detection for auto-switching between terminal and default chains.
# Queries `niri msg -j focused-window` before each injection.
# Set to false to always use the default driver chain.
# Default: true
niri_detect = true

[postprocessing]
# Enable the two-tier Whisper hallucination filter.
# Drops known phantom phrases ("thanks for watching", etc.) and
# single-word hallucinations ("uh", "um", etc.).
# Default: true
hallucination_filter = true

# Enable spoken punctuation replacement.
# "period" -> ".", "comma" -> ",", "open paren" -> "(", etc.
# See the full 44-entry table below.
# Default: true
spoken_punctuation = true
```

---

## Usage

### Running the daemon

```sh
# Run in foreground (useful for development)
asr-rs

# Run with debug logging
RUST_LOG=asr_rs=debug asr-rs

# Run with trace-level logging (very verbose)
RUST_LOG=asr_rs=trace asr-rs
```

The daemon starts in **INACTIVE** state. It does not capture audio or open a WebSocket connection until activated. On startup, it pre-connects to the WhisperLiveKit server in the background for instant activation.

### Toggling with SIGUSR1

```sh
# Activate (start capturing audio, connect WebSocket, begin transcribing)
pkill -USR1 asr-rs

# Deactivate (stop capturing, close WebSocket, stop transcribing)
pkill -USR1 asr-rs
```

SIGUSR1 is a toggle. Each signal flips between INACTIVE and ACTIVE. When deactivating, the daemon tears down the cpal audio stream and WebSocket connection cleanly, then immediately pre-connects for the next session.

### Explicit deactivate with SIGUSR2

```sh
# Deactivate only (no toggle -- safe emergency stop)
pkill -USR2 asr-rs
```

SIGUSR2 only deactivates. If already inactive, it does nothing. Useful for separate start/stop keybinds.

### Niri keybinds

```kdl
// ~/.config/niri/config.kdl

binds {
    // Toggle dictation on/off
    Mod+V { spawn "pkill" "-USR1" "asr-rs"; }

    // Explicit stop only (safe emergency stop)
    Mod+Shift+V { spawn "pkill" "-USR2" "asr-rs"; }
}
```

Press `Mod+V` to start dictating. Press `Mod+V` again to stop. Text appears at the cursor in whatever window is focused. Use `Mod+Shift+V` as a dedicated stop key.

When Niri IPC detection is enabled (default), dictating into a terminal (kitty, foot, Alacritty) automatically uses the paste driver (wl-copy + Ctrl+Shift+V), which triggers bracketed paste and is safe in nvim/tmux. GUI applications use the default chain (wtype → dotool → clipboard).

### Clean shutdown

```sh
# SIGTERM
pkill asr-rs

# Or Ctrl-C if running in foreground
```

Both SIGTERM and SIGINT trigger a clean shutdown: the active session (if any) is deactivated, the WebSocket is closed, and the daemon exits.

### systemd user service

```sh
mkdir -p ~/.config/systemd/user
# Create a unit file:
cat > ~/.config/systemd/user/asr-rs.service << 'EOF'
[Unit]
Description=asr-rs dictation daemon

[Service]
Type=simple
ExecStart=%h/.cargo/bin/asr-rs
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
systemctl --user enable --now asr-rs
```

A standalone systemd service file is also available at `packaging/asr-rs.service`.

---

## WhisperLiveKit server setup

The inference server runs in an isolated Fedora toolbox (or any container) with PyTorch ROCm. asr-rs talks to it over `ws://localhost:8000/asr` -- toolbox networking is transparent (shares localhost with the host).

```sh
toolbox enter asr-toolbox

wlk --model large-v3-turbo \
    --backend whisper \
    --pcm-input \
    --language en \
    --frame-threshold 25
```

| Flag | Purpose |
|------|---------|
| `--model large-v3-turbo` | 32-layer encoder (large-v3 quality), 4-layer decoder (8x faster). ~3.2 GB fp16. |
| `--backend whisper` | Pure PyTorch. Required for ROCm (faster-whisper/CTranslate2 lacks gfx1151 support). |
| `--pcm-input` | Accept raw s16le 16kHz mono PCM over WebSocket. Without this, the server expects compressed audio and invokes FFmpeg. |
| `--language en` | Hardcode English. `auto` adds 2-3x latency on the first chunk for language detection. |
| `--frame-threshold 25` | AlignAtt safety margin: 25 Mel frames = 0.5s. The single most impactful latency/accuracy tradeoff. Lower = faster word emission, higher hallucination risk. |

---

## WebSocket protocol

asr-rs implements the WhisperLiveKit client protocol. Understanding this is essential for debugging.

1. Client connects to `ws://localhost:8000/asr`.
2. Server sends a config message: `{"type": "config", "useAudioWorklet": true}`.
3. Client sends raw s16le PCM as binary WebSocket frames (~20ms chunks, 320 samples, 640 bytes each).
4. Server sends JSON transcript **snapshots** (cumulative, several times per second).
5. Client sends an empty binary frame `b""` to signal end of audio.
6. Server sends `{"type": "ready_to_stop"}` when processing is complete.

### Transcript snapshot format

```json
{
  "status": "active_transcription",
  "lines": [
    { "speaker": 1, "text": "Hello world.", "start": "0:00:02", "end": "0:00:05" }
  ],
  "buffer_transcription": "partial unconfirmed"
}
```

**`lines`** is cumulative -- every snapshot contains ALL confirmed segments from session start. The text within each line also grows cumulatively as SimulStreaming confirms additional words.

**`buffer_transcription`** is unconfirmed ephemeral text that may be replaced or committed. asr-rs ignores it in v1 and only injects from confirmed `lines[]` entries -- backspacing ephemeral text via wtype into arbitrary applications is fragile.

### How asr-rs diffs snapshots (SegmentTracker)

The `SegmentTracker` in `src/ws.rs` maintains two counters:
- `last_line_count`: number of confirmed lines seen in the previous snapshot
- `last_line_text_len`: character length of the last line's text in the previous snapshot

On each snapshot:
- If a line's text has grown (length increased), inject only the delta (new characters).
- If new lines appeared, inject the text of the newly completed lines plus any delta on the current in-progress line.
- If the server **rewinds** (line count decreases or text length shrinks), resync without injecting. This guards against AlignAtt's rewind behavior when its 200-frame threshold triggers.

---

## Post-processing pipeline

Each text delta extracted by SegmentTracker passes through these stages before injection:

### 1. Hallucination filter

Whisper generates phantom phrases from silence -- "Thanks for watching", "Subscribe", YouTube outros, podcast boilerplate. With a long-running local connection, this is a real problem. The two-tier filter (from [talktype](https://github.com/AshkanArabim/talktype)):

**Gate 1 -- Minimum length:** Text shorter than 3 characters is dropped.

**Gate 2 -- Single-word phantom set (exact match):**

| Dropped words |
|---------------|
| ah, bye, goodbye, hmm, huh, i, oh, so, uh, um, you |

**Gate 3 -- Phrase blocklist (substring match, only for text under 40 characters):**

| Dropped phrases |
|-----------------|
| thanks for watching |
| thank you for watching |
| thanks for listening |
| thank you for listening |
| subscribe |
| like and subscribe |
| see you next time |
| the end |
| silence |
| no speech |
| inaudible |
| [music] |
| (music) |

Text over 40 characters skips the phrase check to avoid false positives on real speech that happens to contain a blocklist substring.

### 2. Spoken punctuation

44 entries. Say the spoken form; the daemon replaces it with the punctuation character. Multi-word phrases are matched first (longest match wins) to prevent prefix collisions. Matching is case-insensitive. Trailing punctuation after the spoken command (e.g., Whisper transcribing "period." as literal text) is stripped by the regex.

| Spoken form | Output | | Spoken form | Output |
|---|---|---|---|---|
| period | `.` | | open parentheses | `(` |
| comma | `,` | | open parenthesis | `(` |
| question mark | `?` | | open paren | `(` |
| exclamation mark | `!` | | close parentheses | `)` |
| exclamation point | `!` | | close parenthesis | `)` |
| colon | `:` | | close paren | `)` |
| semicolon | `;` | | open bracket | `[` |
| new line | `\n` | | close bracket | `]` |
| tab | `\t` | | open brace | `{` |
| dash dash | `--` | | close brace | `}` |
| dash | `-` | | at symbol | `@` |
| hyphen | `-` | | hash | `#` |
| underscore | `_` | | dollar sign | `$` |
| double quote | `"` | | percent | `%` |
| single quote | `'` | | caret | `^` |
| quote | `"` | | ampersand | `&` |
| apostrophe | `'` | | asterisk | `*` |
| slash | `/` | | plus | `+` |
| backslash | `\` | | equals | `=` |
| pipe | `\|` | | less than | `<` |
| tilde | `~` | | greater than | `>` |
| grave | `` ` `` | | | |

**Spacing rules for punctuation:**

- **Left-attaching punctuation** (`. , ; : ! ?`): the preceding space is removed. "hello period" becomes "hello." not "hello .".
- **Grouping delimiters**: space after `(` is removed; space before `)` is removed. "open paren hello close paren" becomes "(hello)".
- **Collapsed spaces**: multiple consecutive spaces are collapsed to one.

### 3. Auto-capitalization

The first letter of the text is capitalized. After sentence-ending punctuation (`. ? !`), the next alphabetic character is capitalized. "hello period how are you question mark" becomes "Hello. How are you?"

### 4. Sanitization

Embedded line-terminating characters (`\n`, `\r`, `\x0B`, `\x0C`, `U+0085`, `U+2028`, `U+2029`) are replaced with spaces before injection. This prevents accidental form submission or Enter keypresses when Whisper includes line breaks in its output.

### 5. Inter-chunk spacing

A space is prepended before each delta unless:
- It is the first chunk of the session.
- The delta starts with left-attaching punctuation (`. , ? ! : ; ) ] }`).

---

## Injection methods

asr-rs uses a fallback chain of output drivers. Each driver is tried in order from `driver_order`; if a driver isn't available or fails, the next one is tried.

### wtype (default, wlroots)

[wtype](https://github.com/atx/wtype) sends keystrokes via the Wayland `zwp_virtual_keyboard_v1` protocol. Zero permissions required on wlroots compositors (Niri, Sway, Hyprland). Does NOT work on GNOME or KDE -- they do not implement this protocol.

Each delta is injected as `wtype -- "<text>"`. The `--` separator is critical: without it, text that starts with `-` is misinterpreted as a wtype flag.

### dotool (universal)

[dotool](https://git.sr.ht/~geb/dotool) uses a persistent Unix socket (`dotoold`) that holds a uinput device open. The client `dotoolc` sends `type <text>` commands. No per-invocation overhead. Works on any Linux session -- Wayland, X11, TTY.

**Requirement:** The user must be in the `input` group for uinput access.

```sh
sudo usermod -aG input $USER
# Log out and back in

# Start the daemon (or use a systemd service)
dotoold &
```

### clipboard (Wayland fallback)

Copies text to the Wayland clipboard via `wl-copy`. Text is placed on the clipboard but not automatically pasted — useful as a last resort when no typing injector is available.

### paste (terminal-safe)

Copies text to clipboard via `wl-copy`, then simulates a paste keystroke via `wtype` (default: `Ctrl+Shift+V`). This triggers bracketed paste in terminal emulators, making it safe for kitty, foot, Alacritty with nvim/tmux. The paste keystroke is configurable via `paste_keys`.

### Niri IPC auto-detection

When `niri_detect = true` (default), asr-rs queries `niri msg -j focused-window` before each injection to check the focused app's `app_id`. If the app is in `terminal_app_ids`, the terminal chain (paste → clipboard) is used instead of the default chain. This means you can seamlessly dictate into both GUI text boxes and terminal applications without manual mode switching.

---

## Latency

asr-rs has an honest ~1-3 second latency from speech to text appearing on screen. This is an architectural floor, not a performance bug.

**Why:** Whisper is an encoder-decoder transformer designed for complete utterances. SimulStreaming with AlignAtt is a brilliant adaptation to make it stream, but the attention mechanism needs enough audio context (~1.2 seconds minimum chunk accumulation + 0.5 second frame threshold headroom) before it can confirm words. On a Strix Halo with GPU acceleration, compute time per chunk is fast, but this algorithmic floor is irreducible.

**What it looks like:** Words appear in small bursts every ~0.5-1 seconds after an initial ~1.5-2.5 second delay. Not character-by-character as you speak, but confirmed-word-by-confirmed-word.

**What this is NOT:** This is not Gboard-like instant dictation. Gboard uses an RNN-T architecture that processes audio frame-by-frame (~10ms) and produces characters nearly instantly. The closest local alternative for that experience would be NVIDIA Parakeet-Realtime-EOU (80-160ms, RNN-T) -- but it is NVIDIA-only with no AMD support.

**Tuning:** Lowering `--frame-threshold` on the server (e.g., 15 instead of 25) makes words appear faster at the cost of more boundary errors and hallucination risk. Raising it (e.g., 30) increases accuracy but adds delay.

**Pre-connect optimization:** asr-rs pre-connects to the WhisperLiveKit server on startup and after each deactivation, eliminating the ~100-200ms WebSocket handshake from activation latency.

---

## Project structure

```
src/
  main.rs        -- Entry point, SIGUSR1/SIGUSR2 handlers, ActiveSession lifecycle, pre-connect
  config.rs      -- TOML config loading from $XDG_CONFIG_HOME/asr-rs/config.toml
  audio.rs       -- cpal capture, rubato resampling, ringbuf SPSC, f32->s16le conversion
  ws.rs          -- WebSocket session (concurrent send/recv), SegmentTracker diffing
  filter.rs      -- Two-tier hallucination filter (phrase blocklist + single-word gate)
  inject.rs      -- TextInjector trait, driver chain, Niri IPC detection, clipboard/paste drivers
  postprocess.rs -- Spoken punctuation, spacing rules, auto-capitalization pipeline
packaging/
  asr-rs.spec    -- RPM spec file for Fedora COPR
  asr-rs.service -- systemd user service file
Cargo.toml       -- Dependencies and release profile
```

### Key dependencies

| Crate | Version | Role |
|-------|---------|------|
| cpal | 0.17 | Audio capture (PipeWire/PulseAudio/ALSA backends) |
| rubato | 1.0 | FFT polyphase resampler (48kHz -> 16kHz) |
| ringbuf | 0.4 | Lock-free SPSC ring buffer between audio callback and async runtime |
| audioadapter-buffers | 2.0 | Buffer adapter for rubato's `process_into_buffer` API |
| tokio-tungstenite | 0.26 | Async WebSocket client |
| futures-util | 0.3 | `SinkExt`/`StreamExt` for WebSocket stream splitting |
| tokio | 1 | Async runtime (multi-thread, signals, timers) |
| serde / serde_json | 1 | JSON deserialization of WhisperLiveKit snapshots |
| toml | 0.8 | Config file parsing |
| regex | 1 | Spoken punctuation pattern matching |
| anyhow | 1 | Error handling with context |
| tracing / tracing-subscriber | 0.1/0.3 | Structured logging with env-filter |
| directories | 6 | XDG base directory resolution |

---

## Planned / future features

These are documented v2 candidates from the design research phase. None are implemented yet.

| Feature | Description | Source |
|---------|-------------|--------|
| Config hot-reload | Poll config file mtime every 500ms, reload on change. ~20 LOC tokio background task. | hyprwhspr-rs |
| evdev push-to-talk | Hold a physical key (e.g., RightAlt) to activate instead of toggle. Requires evdev 0.13, `O_NONBLOCK` + 5ms polling. Needs `input` group. | sotto |
| Energy gate | Client-side silence filter (RMS > 0.01, 50ms window) to reduce server load by not sending silent audio. | talktype |
| User word replacements | `[postprocessing.replacements]` table in config for domain vocabulary corrections ("dokker" -> "docker"). | design doc |
| Transcription history | Append transcriptions to `~/.local/share/asr-rs/history.json`, capped at 50 entries. ~30 LOC. | hyprwhspr-rs |
| Last-transcript cache | Write last transcription to `~/.cache/asr-rs/last.txt` for recovery / clipboard integration. | comparison-matrix |
| `--dry-run` mode | Route text to stdout instead of injector. Uses a MockInjector. | voice-keyboard-linux |
| `--list-devices` | Enumerate audio input devices with NFC-normalized names. | dictate |
| `--check` health flag | Verify PipeWire, injection binary, and WebSocket server are reachable before entering the main loop. | hyprvoice |
| PID file guard | Single-instance enforcement via `$XDG_RUNTIME_DIR/asr-rs.pid`. | hyprvoice |
| Latency instrumentation | 4 tracing stages: SIGUSR1 -> WS open -> first audio -> first transcript -> inject. | comparison-matrix |
| DEBUG diff logging | `similar` crate `TextDiff::from_words()` at TRACE level for pipeline debugging. | hyprwhspr-rs |
| Confidence-gated LLM punctuation | Route low-confidence transcriptions through Qwen3-1.7B via llama.cpp for cleanup. Skip when mean logprob >= 85%. Phase 2. | design doc |

---

## Acknowledgements

asr-rs draws patterns from several open-source projects:

| Project | What we learned |
|---------|----------------|
| [dictate](https://github.com/nkoppel/dictate) | cpal + rubato + ringbuf audio pipeline architecture |
| [SimulStreaming](https://github.com/QuentinFuxa/SimulStreaming) | Cumulative text protocol, segment buffer diffing, AlignAtt parameters |
| [hyprwhspr-rs](https://github.com/maotseantonio/hyprwhspr-rs) | Spoken punctuation table, post-processing pipeline ordering, spacing rules |
| [sotto](https://github.com/carson-mccombs/sotto) | SIGUSR1 toggle pattern, daemon state machine, wtype `--` invocation |
| [voice-keyboard-linux](https://github.com/mhartington/voice-keyboard-linux) | Concurrent WebSocket task structure, TextInjector trait abstraction |
| [talktype](https://github.com/AshkanArabim/talktype) | Whisper hallucination blocklist data and two-tier filter logic |
| [VoxType](https://github.com/Jooris-AETHER/VoxType) | Output driver fallback chain, clipboard/paste injection, paste keystroke configuration |

---

## Status and remaining work

asr-rs is a client-only daemon. It does **not** manage containers, toolboxes, or the inference server in any way. The WhisperLiveKit + SimulStreaming server must already be running (e.g., in a Fedora toolbox with ROCm) before asr-rs can do anything useful. When the server isn't reachable, the pre-connect loop retries silently with exponential backoff until it appears.

### What works now

- Full audio pipeline: cpal capture, rubato resampling, ringbuf SPSC, s16le conversion
- WebSocket streaming to WhisperLiveKit with SegmentTracker diffing
- Post-processing pipeline: hallucination filter, 44-entry spoken punctuation, spacing, auto-capitalization, sanitization
- Output driver fallback chain: wtype -> dotool -> clipboard, with PasteInjector for terminals
- Niri IPC auto-detection: seamless switching between direct typing (GUI) and bracketed paste (terminals)
- Pre-connect WebSocket on startup and after deactivation for instant activation
- SIGUSR1 toggle + SIGUSR2 explicit deactivate
- RPM spec for Fedora COPR

### What remains before daily-driving

1. **Build verification** -- `cargo build && cargo clippy && cargo test` must pass. The recent refactors (fallback chain, pre-connect, SIGUSR2) haven't been compiled yet.
2. **Integration testing on Niri** -- verify the Niri IPC path works end-to-end: dictate into a GUI text box (wtype path), then into kitty with nvim open (paste path). Confirm the auto-detection correctly reads `app_id` from `niri msg -j focused-window`.
3. **Pre-connect edge cases** -- verify that a stale pre-connected WebSocket (server restarted between pre-connect and activation) is handled gracefully (the server will RST the connection, `run_session` should fall back to a fresh connect).
4. **RPM build** -- `rpmbuild -ba packaging/asr-rs.spec` hasn't been tested. The spec assumes standard Fedora Rust packaging macros.

### Not planned (by design)

- No container/toolbox management -- asr-rs is a pure client
- No alternative STT backends -- WhisperLiveKit is the only target
- No Waybar/statusline integration -- removed from roadmap
- No X11 or GNOME/KDE support -- Niri/wlroots only for the IPC path; dotool still works universally but without terminal auto-detection

---

## License

MIT License. See [LICENSE](LICENSE) for details.
