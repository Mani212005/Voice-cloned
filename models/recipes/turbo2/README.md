# Recipe `turbo2`

The v1 voice engine: **Chatterbox V3 T3 (Q4_K) + Chatterbox Turbo S3Gen (Q4_K), 2 decoder
steps**, run by the pinned CrispASR v0.8.53 release: one warm `--server` process reused
across chunks and jobs (default), or one CLI process per chunk (`engine.warm = false`).

The machine-readable recipe is [`models/manifests/turbo2.toml`](../../manifests/turbo2.toml)
and the runtime pin is [`third_party/crispasr/PIN.toml`](../../../third_party/crispasr/PIN.toml).
Both are embedded into `vcd` at build time; nothing else defines the recipe.

## Inputs

Both files are used exactly as published; nothing is spliced or converted.

| Role | File | Bytes | sha256 | Source | Licence |
|---|---|---|---|---|---|
| T3 (text to speech tokens) | `chatterbox-v3-t3-q4_k.gguf` | 386,367,392 | `fb63d7c53c4f8bb07286d67709c4d4200fa63b4de3476a4558f37964ce8e5aa6` | huggingface `cstr/chatterbox-GGUF` @ `c45504bb` | MIT |
| S3Gen (speech tokens to audio) | `chatterbox-turbo-s3gen-q4_k.gguf` | 255,891,744 | `1c16251e6722c46500ebddd92be495e0dba160c31169f4b64d833acba819b060` | huggingface `cstr/chatterbox-turbo-GGUF` @ `b544cf8b` | MIT |
| Runtime, Linux x86_64 | `crispasr-linux-x86_64.tar.gz` v0.8.53 (commit `1694d8ef`) | 44,169,635 | `3396d49473642d1097b667270b54125279bdca740b735e15c3c96c1b32f787c0` | github `CrispStrobe/CrispASR` release | MIT |
| Runtime, macOS arm64 | `crispasr-macos-arm64.tar.gz` v0.8.53 (commit `1694d8ef`) | 18,671,735 | `741aa7f3dd4cd996248f590421987df0712fc964a5e129d6a69960828e119e8b` | github `CrispStrobe/CrispASR` release | MIT |

`vcd` and `cargo xtask` pick the runtime of the platform they run on (Apple Silicon on
macOS, Linux x86_64 otherwise) and check the unpacked binary against its own pinned size
and sha256.

## Build

```sh
cargo xtask fetch-models   # download into models/files and models/runtime, verify every hash
cargo xtask verify         # re-check later; `vcd serve` also re-checks on every start
```

`fetch-models` refuses to download when less than 5 GB would remain free.

## Engine settings and why

| Setting | Value | Why |
|---|---|---|
| `--tts-steps` | 2 | The Turbo S3Gen is a 2-step "meanflow" decoder. With the V3 T3 it is 4-5x cheaper in CPU than best1 (V3 T3 + spliced `s-v3voc` S3Gen, 6 steps), with equal speaker similarity and intelligibility in English and Hindi and 0.4-0.5 dB lower HNR. |
| `-t` | 6 | On the 2P+8E i7-1255U more threads only burn CPU (4, 6, 8 and 12 were measured). |
| `--seed` | 0 | The CLI re-seeds T3 and S3Gen from `--seed`, so output is a pure function of (binary, models, reference, text, seed). Another seed is a different rendition. |
| `CRISPASR_S3GEN_FASTCONV` | 0 | `1` is faster in the vocoder but peaks at 1.094 GB on this recipe, too close to the budget. |
| `-l` | `en`, or `hi` for Hindi and Hinglish | |
| `-sl` | the reference's language, when it differs from `-l` | Cross-lingual cloning (an English reference speaking Hindi) runs T3 without guidance: better likeness and half the T3 cost. |
| Chunk | the first sentence alone; then 19 words or fewer (14 for `hi`), cut at sentence ends, then clause marks, then between words; English words are counted after the text is put in spoken form ("467" is three words) | Time to first audio grows with the first chunk. Peak RSS grows with a chunk's audio length, and these caps keep it to about 8 s. |
| Process | one warm `--server` process, restarted before a chunk once its peak RSS is above 1.0 GB (1.35 GB on macOS) | Saves the model load and the reference encode (about 6 s idle, 15 s or more under load) on every chunk. Output is bit-identical to the CLI. |
| `--no-punctuation` | server only | Otherwise the server downloads an unpinned punctuation model that TTS does not use. |
| `MALLOC_ARENA_MAX=1`, `MALLOC_TRIM_THRESHOLD_=0`, `MALLOC_MMAP_THRESHOLD_=65536` | engine environment | With glibc's defaults a warm engine's RSS crept about 22 MB per request and never came back (1.41 GB after 15 chunks). With these it levels off at the largest chunk served. |

## Budget

On Linux, 1.1 GB of peak RSS per engine process (macOS has its own limit, below), in the unit the measurements used (`ru_maxrss` in
KiB divided by 1e6, so `ram_budget_kib = 1100000`). `vcd` reads every chunk's peak (from
`wait4`, or the warm engine's `VmHWM`) and fails the job with `memory_budget_exceeded` if
a chunk goes over.

Measured with the malloc settings (cold CLI, one chunk each): 5.8 s of audio 1.012 GB,
6.8 s 1.046 GB, 7.8 s 1.076 GB, 9.7 s 1.099 GB (1.126 GB without the malloc settings,
which is why Hindi chunks are capped at 14 words). A warm engine's peak after a chunk is
at most about the cold peak of that chunk or its own earlier peak, whichever is higher,
plus a few MB: after the one-word warm-up it holds 0.917 GB, and pushed past its restart
line with long Hindi chunks it reached 1.068 GB. `restart_headroom_kib = 100000` restarts
it before any chunk once its peak is above 1.0 GB.

### macOS (Apple Silicon)

The same recipe runs on the macOS arm64 release, with a larger limit: `[budget.macos]` in
the manifest replaces the two numbers above (`ram_budget_kib = 1500000`,
`restart_headroom_kib = 150000`) and Linux ignores it. The engine's memory is counted
differently there: `vcd` reads the kernel's lifetime peak physical footprint (the
counterpart of `VmHWM`) and `wait4`'s `ru_maxrss` (bytes, not KiB), and the Metal path
holds extra F16 buffers. The glibc `MALLOC_*` settings do nothing on macOS. Measured on an
M-series MacBook (warm engine, English, seed 0): a 3.6 s chunk 1.203 GB, a 2.8 s chunk
1.215 GB, a 6.4 s chunk (19 words) 1.223 GB. With the Linux budget of 1.1 GB every chunk
would fail, so the budget is 1.5 GB there and a warm engine restarts once its peak is above
1.35 GB. macOS has no parent-death signal, so an engine outlives a `vcd` that is killed
outright (`kill -9`); a normal stop (SIGTERM, Ctrl-C) stops it, and so does
`scripts/voice-clone-service stop`.

## Measured levers

Owner voice, 10 English explainer sentences with numbers and acronyms, local whisper
large-v3-turbo word error rate (WER) and UTMOS predicted naturalness (1-5); the
previous Kokoro narrator scored 0.0 % / 4.52. Full tables are in the PR that added
spoken-form normalization.

| Lever | WER | UTMOS | Verdict |
|---|---|---|---|
| this recipe, text as written | 7.5 % | 3.70 | acronyms and numbers garbled |
| spoken-form normalization (shipped) | 1.4 % | 3.73 | the clarity fix |
| T3 Q8 / F16, S3Gen Q8 / F16 | 0.0-0.7 % | 3.73-3.95 | first-sentence peak 1.11-1.75 GB: over the Linux 1.1 GB budget |
| S3Gen flow decoder only at Q8 (spliced) | 0 % (6 sentences) | +0.26 | about +35 MB; a derived file, not shipped |
| 4 decoder steps | 1.4 % | 3.84 | small gain, slower |
| CFG 0.3/0.7, exaggeration 0.3/0.7, temperature 0.6 | 0.7-2.1 % | 3.70-3.78 | noise |
| reference EQ or output EQ (+6 dB shelf) | 1.4 % | 3.71-3.80 | brighter, slightly less like the speaker |
| rate 1.2 / 1.5 (WSOLA) | 0.7 / 1.4 % | 3.56 / 3.35 | intelligibility holds; a little naturalness lost |
| best of 4 seeds per sentence | - | +0.20 | 4x the compute; needs a scorer vcd does not have |

## Not covered yet

- A real-engine golden test (exact PCM hash per CPU class, HNR floor, RSS ceiling) as an
  automated tier.
- A baked voice-conditioning pack, so a restarted engine skips the reference encode
  (the only baker for this model is upstream's Python/torch), and a GPU build.
