# A cleaner cloned voice

The cloned voice copies the reference recording it was enrolled from: its background
hiss, and a muffled or bright tone. `vcd` now cleans each reference when it is enrolled,
and gives every job's output a light finish. Both are on by default and either can be
switched off.

## How to record a good reference

The reference matters more than any setting in `vcd`. In earlier measurements a
reference picked from clean recordings raised the clone's predicted naturalness (UTMOS)
from 3.68 to 4.20, more than any model, sampler or cleanup setting. Cleanup cannot put
back what the microphone never captured.

1. **Room.** A small, soft room: a closet or a bedroom with curtains and a bed. No fan,
   air conditioner or open window. Avoid bare walls, kitchens and bathrooms (echo).
2. **Microphone.** A headset or lapel mic, or a phone held 15-20 cm from your mouth and
   a little to the side (so breath does not hit it). A laptop's built-in microphone is the
   weakest choice: it is far away and usually muffled.
3. **Recorder.** Record WAV, 44.1 or 48 kHz, with the device's noise suppression and
   "voice isolation" effects off if you can find the switch. Those effects leave a dull,
   watery sound that the clone copies.
4. **Level.** Speak at your normal volume. Peaks should stay well below the top of the
   meter (no clipping); about -20 LUFS is ideal and `vcd` tolerates anything that is not
   clipped.
5. **Content.** 20-30 s of relaxed, natural speech in your usual speaking style, in one
   take. Put your best sentence first: the engine leans on the first 6-10 s. Leave about
   half a second of silence at the start and the end.
6. **Check.** Listen back on headphones. If you hear hum, hiss, echo or traffic, move and
   record again; that is quicker than any fix.

## How the cleanup is applied at enrollment

`POST /v1/voices` decodes the upload, checks it (5-30 s, not silent, not clipped), and,
unless the upload says `cleanup=false`, cleans it before storing it:

1. A 70 Hz high-pass removes rumble.
2. RNNoise (Xiph's recurrent speech denoiser, the pure-Rust `nnnoiseless` port with its
   weights compiled in) runs at 48 kHz. Its result is mixed back with the original so
   that nothing is turned down by more than 18 dB: noise drops, and the denoiser cannot
   carve the voice thin.
3. If the recording is muffled, a high shelf at 3.5 kHz gives it back its top end: the
   4-8 kHz band is lifted toward -17 dB relative to the 0.5-2 kHz band, the median of the
   owner's clean close-mic recordings, by at most 6 dB. A recording that is already as
   bright is left alone; the shelf never cuts.
4. The speech level is restored and peaks are kept under -1 dBFS.

Only the cleaned reference is stored (never the upload). The voice reports
`"reference_cleaned": true`. Cleanup takes about a second of CPU for a 15 s reference.
Voices enrolled before this change are not touched: to clean one, enroll the same
recording again (and delete the old voice).

```sh
curl -s -X POST $B/v1/voices -F consent_id=cns_... -F name=me -F file=@ref.wav             # cleaned
curl -s -X POST $B/v1/voices -F consent_id=cns_... -F name=me -F cleanup=false -F file=@ref.wav  # as recorded
```

## The output polish

Unless a job says `"polish": false`, its joined output is finished with:

1. a 40 Hz high-pass (DC and rumble),
2. a de-esser (the band above 5.5 kHz is turned down by up to 6 dB while it is more than
   -12 dB of the full-band level, which only sibilants reach),
3. loudness normalization to -16 LUFS (ITU-R BS.1770 gated loudness, the level of
   podcasts and voice apps; the raw engine output sits near -22 LUFS),
4. a look-ahead peak limiter at -1 dBFS.

The loudness is measured over the whole job, so every chunk gets the same gain. Chunk
files and the 1.2x rate (applied per chunk before joining) are unchanged; the polish runs
in `vcd`, not in the engine, so the engine's memory budget is unaffected.

## Switches

```toml
# vcd.toml
[audio]
reference_cleanup = true   # clean new references at enrollment; uploads may say cleanup=false
output_polish = true       # polish job output; jobs may say "polish": false
```

To hear or measure either step on a file without running the service:

```sh
cargo run -q --release -p vc-audio --example cleanup -- ref.wav ref-clean.wav
cargo run -q --release -p vc-audio --example polish -- in.wav out.wav
```

## Decision log

### Where the noise comes from

Measured on the owner's enrolled reference (a 14.6 s, 16 kHz recording from a laptop)
and on clones of it made with the shipped recipe (turbo2, Q8_0 flow decoder, seed 0),
against a clean 27 s reference built from the owner's close-mic recordings and against
Kokoro (a studio-trained voice) for scale. "Clone" rows are means over 6 English
sentences. All audio stayed on this machine; only numbers are recorded here.

| | Reference UTMOS | HNR (dB) | Pause floor (dBFS) | 4-8 kHz tilt (dB) | 4-8 kHz SNR (dB) |
|---|---|---|---|---|---|
| Laptop reference (enrolled) | 2.55 | 9.7 | -60.4 | -21.2 | 21.8 |
| Clean close-mic reference | 3.33 | 11.6 | -58.2 | -17.6 | 39.7 |
| Clone of the laptop reference | 3.94 | 9.4 | -70.7 | -21.2 | 29.2 |
| Clone of the clean reference | 4.35 | 11.9 | -54.1 | -17.0 | 30.7 |
| Kokoro | 4.52 | 13.2 | -68.8 | -11.2 | 51.3 |

(HNR: harmonics-to-noise ratio of voiced frames, higher is a cleaner voice. Pause floor:
level of the quietest 10% of 20 ms frames. Tilt: 4-8 kHz level relative to 0.5-2 kHz,
`vc_audio::hf_tilt_db`, the measure the cleanup uses; 30 of the owner's clean close-mic
clips measure -17.6 at the median, -18.6 to -15.2 for the middle half. 4-8 kHz SNR:
speech frames over pause frames in that band.)

- **The reference, not the engine or the audio path.** The clone's voice quality follows
  its reference: HNR 9.4 from the laptop reference and 11.9 from the clean one, with the
  same engine. `vcd`'s own path (decode, join, time-stretch, encode) is sample-exact and
  adds no noise (its tests compare outputs sample for sample).
- **Not the pauses.** The clone's pauses are quiet (-71 dBFS, quieter than the clean
  reference's clone). The noise is inside the voice: a hiss in the 4-8 kHz band (only
  21.8 dB above the pause floor in the reference, 29.2 dB in the clone) and a rough,
  breathy texture (low HNR).
- **A muffled top end.** The laptop recording's 4-8 kHz band sits 3.6 dB below the
  median of the owner's clean recordings, and the clone copies it to the tenth of a dB
  (-21.2 and -21.2; the clean reference -17.6 gives a clone at -17.0), so it sounds dull
  rather than crisp.
- **Loudness.** Raw clones sit near -22 LUFS with peaks near -2.5 dBFS: quiet next to
  podcasts and voice apps (-16 LUFS).

### What was tried

Each reference variant was enrolled into the shipped recipe and the same 6 English and
2 Hindi sentences rendered with the same seed, one engine at a time at nice 10. UTMOS is
measured after matching every clip to -23 LUFS, because UTMOS rates quieter audio
slightly higher (the same clips 6 dB quieter score +0.02) and the variants differ in
level.

Reference cleanup (on the laptop reference):

| Reference | UTMOS en | UTMOS hi | HNR | Pause floor | 4-8 kHz SNR | Brightness (>4 kHz) | Likeness |
|---|---|---|---|---|---|---|---|
| as enrolled | 3.94 | 3.28 | 9.44 | -70.7 | 29.2 | -25.8 | 0.809 |
| 70 Hz high-pass only | 3.93 | 3.25 | 9.39 | -71.4 | 29.2 | -25.8 | 0.805 |
| ffmpeg afftdn (spectral) | 4.02 | - | 9.37 | -69.6 | 30.8 | -25.2 | 0.793 |
| Demucs htdemucs vocals | 3.91 | 3.30 | 9.32 | -71.8 | 31.1 | -26.1 | 0.807 |
| DeepFilterNet3 | 4.03 | 3.35 | 9.80 | -74.2 | 36.1 | -27.9 | 0.804 |
| RNNoise | 3.95 | 3.42 | 9.83 | -77.3 | 39.2 | -27.2 | 0.814 |
| +6 dB shelf only | 3.91 | 3.43 | 9.52 | -63.6 | 26.4 | -20.8 | 0.801 |
| RNNoise, then +6 dB shelf | 4.05 | 3.45 | 9.57 | -76.2 | 41.0 | -22.0 | 0.800 |
| DeepFilterNet3, then +6 dB shelf | 3.97 | 3.39 | 9.63 | -76.2 | 41.5 | -22.3 | 0.802 |
| **shipped: high-pass, RNNoise capped at 18 dB, adaptive shelf (+5.8 dB)** | **4.03** | **3.32** | 9.49 | -73.6 | 36.3 | **-20.9** | 0.810 |
| shipped, RNNoise capped at 30 dB | 3.96 | 3.19 | 9.48 | -74.2 | 38.4 | -21.1 | 0.808 |

(Likeness: Resemblyzer cosine against 30 held-out clean clips of the owner. Brightness:
energy above 4 kHz relative to the total, dB.)

- Every denoiser lowers the reference's pause floor by 7-12 dB but barely moves its own
  UTMOS (2.55 to at most 2.61): the laptop recording's weakness is its microphone and
  tone, not its background. Denoising still pays off in the clone: the 4-8 kHz hiss drops
  (+7-10 dB SNR) and Hindi gains up to +0.14 UTMOS.
- Brightening alone makes the clone hiss (pause floor up 7 dB, 4-8 kHz SNR down): the
  reference's noise must go first. Denoise then brighten is the best combination
  measured on these 8 sentences: +0.11 UTMOS in English and +0.18 in Hindi, with the hiss
  12 dB lower.
- Likeness to the owner's real voice is unchanged (0.80-0.81 throughout).
- On 4 more English and 4 more Hindi sentences the picture held for noise and tone (the
  shipped cleanup: 4-8 kHz SNR 25.5 to 34.2 dB, pause floor -68.6 to -72.5 dBFS, 3 dB
  brighter) while UTMOS moved within its spread (English 4.03 to 3.90, Hindi 3.28 to
  3.38). Over all 10 English and 6 Hindi sentences: as enrolled 3.98 / 3.28, shipped
  cleanup 3.98 / 3.36, the same with an RNNoise cap of 30 dB 3.99 / 3.33. The cap makes
  no measurable difference, so the safer 18 dB stays. English WER is 0/93 before and
  after; Hindi CER 40/171 and 42/171, both from one sentence the engine finds hard.
- No option made the voice thin or metallic by the numbers (HNR never fell, UTMOS never
  dropped beyond run-to-run spread). The cap on RNNoise and the never-cut shelf keep the
  worst case mild by construction.

Output polish (on clones of the laptop reference, loudness-matched UTMOS):

| Output chain | UTMOS en | UTMOS hi | HNR |
|---|---|---|---|
| none | 3.94 | 3.28 | 9.44 |
| 70 Hz high-pass | 3.93 | 3.25 | 9.39 |
| + presence +2.5 dB at 3.5 kHz, de-esser | 3.93 | 3.25 | 8.99 |
| + 2:1 compressor, -16 LUFS | 3.86 | 3.12 | 8.88 |
| -16 LUFS and limiter only | 3.93 | 3.24 | 9.36 |

- The compressor cost 0.06-0.11 UTMOS: it made the voice less natural, so it is not
  shipped. A presence boost on the output only raised the hiss already in the clone
  (lower HNR); brightening belongs in the reference, where the engine learns the voice
  from it, so it is not shipped either.
- Loudness normalization, the limiter and the de-esser are neutral within run-to-run
  spread and make the voice as loud as other voice audio, which is most of what makes a
  voice "pop".

### End to end through vcd

The real service on its own port and a copy of the owner's data (database migrated from
v2, the old voice reads `reference_cleaned: false`), the laptop reference enrolled again
with the default cleanup (0.8 s), then one job after another on a warm engine:

| Job | Chunks | Peak RSS (worst chunk) | Audio |
|---|---|---|---|
| English paragraph, old voice, no polish | 5 | 1.013 GB | 13.64 s |
| same, cleaned voice, polish | 5 | 1.014 GB | 13.88 s |
| same at rate 1.2 | 5 | 1.017 GB | 11.57 s |
| Hindi, old voice, no polish | 2 | 1.009 GB | 6.96 s |
| Hindi, cleaned voice, polish | 2 | 0.994 GB | 6.04 s |
| same at rate 1.2 | 2 | 1.001 GB | 5.03 s |

Every chunk stays under the 1.1 GB budget; the cleanup does not change the engine's
memory (the reference is the same length) and the polish runs in `vcd`. The two English
paragraphs, before and after, measure: UTMOS 3.74 and 3.92 (loudness-matched), HNR 9.07
and 9.46 dB, 4-8 kHz SNR 36.1 and 45.6 dB, energy above 4 kHz -27.4 and -22.1 dB, loudness
-22.9 and -16.3 LUFS, likeness 0.811 and 0.825, ASR errors 0/42 words in both. The Hindi
pair: UTMOS 3.38 and 3.84, CER 16/56 and 15/56. These are single renditions: the cleaner reference also changes timing and
intonation a little, so a single pair can swing more than the averages above. A blind A/B
pair of these two clips is kept on the owner's machine for a listening verdict, which
no number here replaces.

### Defaults and why

- **Reference cleanup on.** It lowered the hiss and restored the top end in every test,
  held naturalness, intelligibility and likeness, and a clean recording passes through
  nearly untouched (synthetic test: SI-SDR 15.5 dB against
  the high-passed input, level within 0.2 dB). It changes only new enrollments.
- **Output polish on.** Neutral for naturalness, louder and more consistent output.
  Anyone who masters the audio themselves can send `"polish": false` or switch it off.
- Both defaults are recommendations for the owner to confirm.

### Options not shipped

- **DeepFilterNet3** denoised as well as RNNoise (better English UTMOS, worse Hindi), but
  it needs a new model file (an 8.7 MB PyTorch checkpoint, or its ONNX export) and an ONNX
  runtime in the binary. The only Rust ports on crates.io are third-party forks. It would
  need its own pin and verification like the engine's models; RNNoise needs neither.
- **Demucs** (music source separation) and ffmpeg's spectral denoisers did less and need
  Python or ffmpeg at runtime.
- A compressor, presence or air EQ on the output (see above).

### Dependencies

`nnnoiseless` 0.5.2 (BSD-3-Clause, RNNoise weights compiled in), with `easyfft`,
`rustfft` and their small helpers, all from crates.io and pinned by `Cargo.lock`
checksums; `cargo deny` passes. No model file is downloaded and nothing runs outside the
`vcd` process.
