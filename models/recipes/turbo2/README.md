# Recipe `turbo2`

The v1 voice engine: **Chatterbox V3 T3 (Q4_K) + Chatterbox Turbo S3Gen (Q4_K), 2 decoder
steps**, run by the pinned CrispASR v0.8.53 CLI, one process per text chunk.

The machine-readable recipe is [`models/manifests/turbo2.toml`](../../manifests/turbo2.toml)
and the runtime pin is [`third_party/crispasr/PIN.toml`](../../../third_party/crispasr/PIN.toml).
Both are embedded into `vcd` at build time; nothing else defines the recipe.

## Inputs

Both files are used exactly as published; nothing is spliced or converted.

| Role | File | Bytes | sha256 | Source | Licence |
|---|---|---|---|---|---|
| T3 (text to speech tokens) | `chatterbox-v3-t3-q4_k.gguf` | 386,367,392 | `fb63d7c53c4f8bb07286d67709c4d4200fa63b4de3476a4558f37964ce8e5aa6` | huggingface `cstr/chatterbox-GGUF` @ `c45504bb` | MIT |
| S3Gen (speech tokens to audio) | `chatterbox-turbo-s3gen-q4_k.gguf` | 255,891,744 | `1c16251e6722c46500ebddd92be495e0dba160c31169f4b64d833acba819b060` | huggingface `cstr/chatterbox-turbo-GGUF` @ `b544cf8b` | MIT |
| Runtime | `crispasr-linux-x86_64.tar.gz` v0.8.53 (commit `1694d8ef`) | 44,169,635 | `3396d49473642d1097b667270b54125279bdca740b735e15c3c96c1b32f787c0` | github `CrispStrobe/CrispASR` release | MIT |

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
| Chunk | 19 words or fewer, cut at sentence ends, then clause marks, then between words | Keeps every engine process within the budget. |
| Process | one per chunk | A long-lived CrispASR server grew from 1.15 to 1.41 GB over 15 chunks; separate processes peak at 1.00-1.07 GB. |

## Budget

1.1 GB of peak RSS per engine process, in the unit the measurements used (`ru_maxrss` in
KiB divided by 1e6, so `ram_budget_kib = 1100000`). `vcd` reads every chunk's peak from
`wait4` and fails the job with `memory_budget_exceeded` if a chunk goes over.

## Not covered yet

- A real-engine golden test (exact PCM hash per CPU class, HNR floor, RSS ceiling) as an
  automated tier.
- A persistent engine worker through the C ABI, a baked voice-conditioning cache, and a
  GPU build.
