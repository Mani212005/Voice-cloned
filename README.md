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
  files, and only ever deletes its own call files.
- The shipped intro and beep are placeholder tones. Pass `--intro`/`--beep` WAV files
  (any rate, resampled to 8 or 16 kHz) to replace them.
- `--wideband` offers mSBC 16 kHz through the system `libsbc.so.1`. Narrowband
  CVSD 8 kHz is the default.
