# Voice Clone


| Property | Description |
|---|---|
| Name | Voice Clone |
|Explaination| This repo takes your voice recording and clones them|

## Introdcution
This repo is for cloning your voice, record your voice in mp3 or wav. pass the file through the repo and clone your voice. 


## Hands-free call answerer (`voice-clone hfp`)

This laptop acts as a Bluetooth hands-free unit for your own phone, like a car kit.
When a call rings for the answer delay (default 20 s) and you have not picked up,
the daemon answers, plays the intro, beeps and records the caller's message.
It talks to BlueZ directly over D-Bus and never touches PipeWire. No root is needed.

```sh
uv run voice-clone hfp doctor              # read-only readiness checks
uv run voice-clone hfp run --observe -v    # register, never answer, log the AT traffic
uv run voice-clone hfp run                 # answer after 20 s of ringing
```

- Policy: `~/.config/voiceclone/policy.txt`, one `<number> <ai|ring|deny>` per line,
  with `*` for the default and `withheld` for callers without caller ID. Without the
  file every caller gets the AI.
- Call audio and records are kept only in `~/.local/share/voiceclone-calls`
  (owner-only permissions) and are deleted after 24 hours. Never commit them.
  `--calls-dir` must be a dedicated directory: the daemon refuses one that holds other
  files, and only ever deletes its own call files: regular files named
  `<seconds>-<n>.wav`/`.json` directly inside a directory it marked with
  `.voiceclone-store`. It never follows symlinks, never enters subdirectories and
  `voice-clone hfp cleanup` refuses a directory without the marker (for example your
  home folder).
- The shipped intro and beep are placeholder tones. Pass `--intro`/`--beep` WAV files
  (any rate, resampled to 8 or 16 kHz) to replace them.
- `--wideband` offers mSBC 16 kHz through the system `libsbc.so.1`. Narrowband
  CVSD 8 kHz is the default.

## Voice service (`vcd`)

`vcd` is a local HTTP service, written in Rust, that clones the owner's voice from one
reference clip and speaks English or Hindi/Hinglish text with it. It runs on this
machine only (it refuses to listen on anything but loopback) and works in batch: a job's
first sentence is heard after roughly 10-13 s, and the rest runs slower than real time on
this laptop's CPU.

The speech engine is the pinned CrispASR v0.8.53 release with the `turbo2` recipe
(Chatterbox V3 T3 + Turbo S3Gen, both Q4_K, 2 decoder steps). `vcd` keeps one engine
process warm (its `--server` mode: models loaded, the voice already encoded) and feeds it
one text chunk at a time: the first sentence alone, then chunks of 19 words or fewer (14
for Hindi). While idle it gets the engine ready for the voice used next, and it restarts
the engine before any chunk that could take its peak RSS over the memory budget (1.1 GB on
Linux, 1.5 GB on macOS); a chunk that still goes over fails its job. `engine.warm = false`
in `vcd.toml` runs a cold CLI process per chunk instead; the audio is identical. See
[`models/recipes/turbo2/README.md`](models/recipes/turbo2/README.md).

English text is rewritten into its spoken form before it is chunked, because the engine
reads text literally (and lowercased): acronyms are spelled with letter names ("AI" is
said "ay eye", "CPUs" "see pee yous"), numbers, decimals, years, ranges ("40-80 ms" is
"forty to eighty milliseconds"), money, percentages, times, ordinals, versions, units and
common abbreviations are said as words. Callers send ordinary written text. Hindi and
Hinglish text is passed through unchanged. To see what the engine will be given:
`echo 'It ran 467 CPU tasks.' | cargo run -q -p vc-text --example speakable -- en`.

A job's optional `rate` (0.75-1.5, default 1.0) speeds the narration up or slows it
down without changing the pitch: each chunk is time-stretched after synthesis (WSOLA) and
the pause between chunks scales with it. Rate 1.0 leaves the audio untouched.
`cargo run -q -p vc-audio --example stretch -- 1.2 in.wav out.wav` applies the same
stretch to a file.

The reference decides most of how natural the voice sounds. The engine conditions on
the first 6-10 s of it and averages the speaker across all of it, so enroll 20-30 s of
your cleanest, most natural-sounding speech (close microphone, no room echo, no
clipping, level around -20 LUFS) with the best clip first. In measurements with the
owner's voice, a reference picked this way from clean recordings raised the predicted
naturalness (UTMOS) of the same sentences from 3.68 to 4.20, more than any model or
sampler setting.

Upgrading: the database migrates itself (schema v2 adds the job rate; an older vcd
refuses the newer database). The chunk plan of English text with numbers, acronyms,
symbols or abbreviations differs from the previous release's, so such a job still queued
or running at upgrade fails with "chunk plan changed since the job was accepted" and must
be resubmitted; restart the service when idle.

```sh
cargo xtask fetch-models          # model files + engine runtime into models/ (gitignored), verified
cargo run --release -p vcd -- serve   # 127.0.0.1:8740, data in ~/.local/share/voice-clone/vcd
```

```sh
B=http://127.0.0.1:8740
# 1. Consent record (own voice only in v1), then enroll one WAV reference (5-30 s).
curl -s -X POST $B/v1/consents -H 'content-type: application/json' -d '{"speaker_name":"Me",
  "relationship":"self","attestation":"This is my own voice.","languages":["en","hi"]}'
curl -s -X POST $B/v1/voices -F consent_id=cns_... -F name=me -F reference_language=en -F file=@ref.wav
# 2. A job (language "en", or "hi" for Hindi and Hinglish), then poll it and download the WAV.
curl -s -X POST $B/v1/jobs -H 'content-type: application/json' \
  -d '{"voice_id":"voice_...","language":"hi","text":"Namaste, main abhi busy hoon."}'
curl -s -X POST $B/v1/jobs -H 'content-type: application/json' \
  -d '{"voice_id":"voice_...","language":"en","rate":1.2,"text":"It runs 467 CPU tasks in 40-80 ms."}'
curl -s $B/v1/jobs/job_...                     # state, queue position, chunks, timings, peak RSS
curl -s -o out.wav $B/v1/jobs/job_.../audio    # 24 kHz mono WAV once succeeded
curl -s -X DELETE $B/v1/jobs/job_...           # cancel (kills the engine) or delete
```

| Endpoint | |
|---|---|
| `GET /v1/health`, `/v1/version`, `/v1/models` | liveness; backend, runtime pin, model hashes, consent chain head; capabilities |
| `POST /v1/consents` | append-only, hash-chained consent records |
| `POST/GET /v1/voices`, `GET/DELETE /v1/voices/{id}` | enroll one WAV reference, list, delete (409 while a job uses it) |
| `POST /v1/jobs`, `GET/DELETE /v1/jobs/{id}`, `GET /v1/jobs/{id}/audio` | queue a job (202, 429 when the queue is full), progress, cancel or delete, the final WAV |

Requests whose `Host` is not `127.0.0.1`, `localhost` or `[::1]` are refused with a 400,
which blocks DNS-rebinding pages from driving the loopback listener.

Errors are `application/problem+json` with a stable `code`. Settings live in an optional
`vcd.toml` (`--config`); every key has a default.

Privacy: only the normalized reference is stored (the upload itself is not kept), job
text is dropped when a job ends, outputs are deleted after 24 hours, and everything sits
in an owner-only data directory. The engine runs with a cleared environment and a
private HOME, cache and TMPDIR per job. Nothing is sent off the machine. Never commit
references, outputs or recordings; tests use synthetic audio only.

Workspace: `crates/vc-core` (ids, job states, rate, error codes), `vc-text` (spoken form,
chunk planner), `vc-audio` (WAV, time-stretch), `vc-backend` (`Backend` trait,
`CrispWarmBackend` and its restart policy, `CrispCliBackend`, fakes, process control),
`vc-store` (SQLite + data dir), `vc-engine` (scheduler: one engine slot, queue, cancel, recovery, retention), `vc-api`
(axum routes), `vcd` (the binary), `xtask`.

Not in v1 yet: encryption at rest and crypto-erase, server-sent progress events and
per-chunk downloads, `/v1/speech` streaming, C2PA signing of the joined output, metrics,
a systemd unit with an engine sandbox, API keys, a baked voice pack that survives engine
restarts, Opus output, an OpenAI-compatible shim, and an option for the spoken AI
disclaimer (always off in v1).

### Running it on a Mac

On Apple Silicon `vcd` runs the pinned CrispASR macOS arm64 release (same version, its own
size and sha256 pin). One command starts the service in the background, from a lasting home
outside the checkout, `~/.local/share/voice-clone` (override with `VOICE_CLONE_HOME`):

```sh
scripts/voice-clone-service start    # first run: builds vcd, fetches and verifies the models
```

The first run needs Rust (`cargo`) and about 1 GB of free disk for the build, which it
deletes afterwards, plus 0.7 GB for the models and engine. It then installs `vcd` and a copy
of the script into the home, so later runs need no checkout and only start the service:

```sh
~/.local/share/voice-clone/bin/voice-clone-service start    # background, 127.0.0.1:8740 only; waits for /v1/health
~/.local/share/voice-clone/bin/voice-clone-service status   # pid, uptime, health; exit code 3 when stopped
~/.local/share/voice-clone/bin/voice-clone-service stop     # stops vcd and its engine
~/.local/share/voice-clone/bin/voice-clone-service reset    # stop, then remove binary, models and logs
tail -f ~/.local/share/voice-clone/logs/vcd.log             # logs (JSON lines)
```

What lives where: `bin/` (vcd and the script), `models/` (hash-verified model files and
engine), `data/` (voices, jobs, `vcd.sqlite3`; owner-only), `logs/`, `run/` (pid file).
`reset` never touches `data/`; to start from nothing, run `reset` and then delete the
`data/` folder yourself (that removes enrolled voices). To move existing voices over, stop
the service and copy `voices/` and `vcd.sqlite3` into `data/`. After pulling a new version,
run `scripts/voice-clone-service install` once to rebuild.

Nothing starts it automatically: no launchd agent or login item is installed, so after a
reboot or logout run `start` again. The service is local only; nothing leaves the machine.
The engine's memory limit is higher on macOS than on Linux; see
[`models/recipes/turbo2/README.md`](models/recipes/turbo2/README.md#macos-apple-silicon).
