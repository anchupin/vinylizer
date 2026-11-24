# vinylizer

*[Русская версия](README.ru.md)*

Real-time "from a record" sound for all macOS system audio. Written in Rust, no SoundSource needed.
It has its own vinyl DSP and can also host an Audio Unit effect (for example, iZotope Vinyl).
Tuned for small Bluetooth speakers such as the Marshall Emberton III.

Requires macOS 14.2 or later (Core Audio process taps).

## Build and run

```sh
./Vinylizer.command                           # build, install to ~/Applications/Vinylizer.app and launch
cargo build --release
./target/release/vinylizer                    # menu bar app (default)
./target/release/vinylizer --cli              # terminal mode: all system audio → current output
./target/release/vinylizer --cli --preset worn
./target/release/vinylizer --cli --app com.spotify.client   # Spotify only
./target/release/vinylizer --cli --au Vinyl         # iZotope Vinyl instead of the built-in DSP
./target/release/vinylizer --cli --au Vinyl --chain # iZotope Vinyl, then the built-in DSP
./target/release/vinylizer --list-devices | --list-apps | --list-plugins
```

On first launch macOS asks for "System Audio Recording" permission (for the app, or for the terminal
in CLI mode). Without it the tap delivers silence. Grant it in System Settings → Privacy & Security →
Screen & System Audio Recording.

In CLI mode a console is available: `help`, `set crackle 0.5`, `preset shellac`, `bypass`, `meter`,
`au` (AU parameters), `au set <id> <v>`, `au presets`, `au preset <n>`, `quit`.

## How it works

```
CATapDescription (global, exclude self | per-app), muteBehavior = mutedWhenTapped
        │  the apps' original sound never reaches the speaker
        ▼
Private aggregate device: [speaker (clock master)] + [tap, drift compensation]
        │
        ▼  one HAL IOProc, Core Audio's real-time thread
tap buffer ──► DSP / AU ──► speaker output buffer
```

- **No intermediate buffer, no extra thread.** The tap and the output device live in one aggregate with a
  shared clock, so input and output arrive in the same callback. The built-in DSP reads straight from the
  tap's buffer and writes straight into the device's buffer. The AU path adds one deinterleave and one
  interleave: AU v2 wants planar audio.
- **A real-time-safe hot path.** No allocation, locks, syscalls or logging. Parameters travel through
  versioned atomics; filter coefficients are recomputed once per block and only when something changed.
  `panic = "abort"`, so nothing can unwind across the FFI boundary.
- **No dependencies** besides `libc`. Bindings to CoreAudio, AudioToolbox, CoreFoundation and the
  Objective-C runtime are hand-written (`src/ffi.rs`); struct layouts are checked by a test.
- If the output device has a microphone (a Bluetooth headset), its input streams are switched off through
  `kAudioDevicePropertyIOProcStreamUsage`, so Bluetooth does not drop into HFP.
- Switching the output device or the target apps rebuilds the route automatically.

### Menu bar app

Clicking the icon opens a panel. Clicks inside keep it open; a click outside closes it.
The interface is in English and Russian ("RU | EN" in the header; the system language until you pick one).
Settings are saved automatically and survive restarts. Three modes:

- **V2** — a ready sound for the Emberton III with four knobs instead of seventeen:
  - *Condition* (new → worn out) moves everything that wears together: dust, clicks, pops, groove hiss,
    dull treble, wow and stylus distortion;
  - *Era* (1930–2000): the recording band and character of its time, mono before 1955–1960;
  - *Effects volume* moves all crackle and hiss together;
  - *Color*: a warm vintage tone of the whole sound.

  Styles *Plain*, *Jazz* (a played record, 1958, narrower stereo) and *Rock* (cleaner, brighter, 1972,
  background and hiss +6 dB against a dense master). No scratches, 33⅓, mono bass. Your own settings in
  the Vinylizer tab are kept aside while V2 plays.

  The middle of *Condition* is calibrated against a recording of a real LP's silent grooves
  ([Freesound #140295](https://freesound.org/people/joedeshon/sounds/140295/) by joedeshon, CC BY 4.0)
  with `examples/reference_stats.rs`, relative to its noise floor: ~16 ticks/s sitting close to the hiss
  (15.6 / 1.4 / 0.22 / 0.01 per second louder by +12 / +18 / +24 / +30 dB), each click a ~2 kHz ring of
  ~1.4 ms, pink hiss rolling off above 4 kHz. On top of that come rare loud pops from big particles
  (about one in 40 s), which that clean record didn't have but played ones do.
- **Vinylizer** — the built-in DSP with every parameter by hand: presets, your own presets
  (*Save*, *Save As…*, *Delete…*, *Reset* to *Warm*), *Bypass* to compare with the original.
  Your presets live in `~/Library/Application Support/Vinylizer/Presets/<name>.preset`.
- **Plugin** — a third-party Audio Unit (for example iZotope Vinyl) with its own parameters and presets;
  `.aupreset` files in `~/Library/Audio/Presets/<manufacturer>/<plugin>/`; *Reset* loads the plugin's
  `Default` preset. Parameter names are shown as the plugin reports them.

When nothing plays (paused, app closed), the needle "lifts": after 3 s of input below −70 dBFS the crackle
and hiss fade out in ~0.4 s and come back within 20 ms when sound returns. Gaps between tracks shorter than
3 s keep their crackle. In plugin mode the plugin's output fades the same way. *Crackle without music*
turns this off: crackle and hiss keep going. While the selected app is paused or closed macOS doesn't run
its tap, so the needle then plays through a separate output-only IOProc on the device (the microphone stays
closed) and steps aside as soon as the main path runs again. Built-in effect only: a third-party plugin
can't run in two places at once.

### Built-in vinyl DSP (`src/dsp.rs`)

| Parameter (`--set`) | In the panel | What it does |
|---|---|---|
| `wet` | Effect amount | how much "record" is mixed into the original |
| `year` | Era | 1930–2000: recording band (1930s ~150 Hz–4.5 kHz, 1950s ~60 Hz–10 kHz, 1970s on full), a forward mid on older records, mono up to 1955, stereo from 1960 |
| `age` | Record wear | treble roll-off (18 → 5 kHz) and a little warmth at 200 Hz |
| `crackle` | Crackle | dust particles per second (up to ~25) and fizz; nearly all of them form a quiet background |
| `crackle_level` | Background level | level of the usual quiet ticks, −55 to −25 dBFS |
| `clicks` | Clicks | how heavy the tail is: particle loudness follows a power law (the louder, the rarer); shown as "1 in N" particles 15+ dB above the background |
| `pops` | Pops | rare loud pops from big particles, apart from the dust (up to one every 2 s) |
| `scratches` | Scratches | how often a scratch appears (up to 4 at once): it clicks every revolution for 6–30 revolutions, then a pause from 5 s to ~2 min |
| `click_level` | Click peak | ceiling for clicks, pops and scratches, −40 to 0 dBFS; the background doesn't depend on it |
| `noise` | Surface noise | pink groove hiss rolling off above 4 kHz, −72 to −42 dBFS RMS, "breathing" with each revolution |
| `surface_gain` | Effects volume | moves background, clicks, pops, scratches and hiss by the same dB (−24…+24), but no further than the click peak's headroom below 0 dBFS ("max +N" when it stops) |
| `wow` | Wow & flutter | wow at the rotation rate plus its 2nd harmonic, flutter at 6.5 Hz |
| `drive` | Stylus distortion | mostly 2nd harmonic, stronger on sibilants (RIAA-style pre-emphasis) |
| `color` | Color | warm tone of the whole chain, music and surface alike: up to +3.5 dB below ~300 Hz, −2 dB at 3.5 kHz, −4.5 dB above ~6 kHz |
| `width` | Stereo width | 0 is mono; below 150 Hz always mono, as cut on a lathe |
| `gain` | Volume | 0..2, linear |
| `rpm` | Speed | 0 = 33⅓, 1 = 45, 2 = 78 rpm: the period of scratches and wow |

Signal chain: modulated delay → HP 28 Hz → low shelf → pre-emphasis → stylus (even harmonic + a very gentle
`tanh`) → de-emphasis → DC block → 2×LP (wear) → era band and mid → M/S (mono bass, era stereo) → crackle
and hiss → color. The dry signal is read from the same delay line, so `wet < 1` doesn't comb-filter.
Latency is 3 ms. A soft limiter at 0.9 on the output keeps clicks over loud masters from clipping.

Tuned for a small Bluetooth speaker (Marshall Emberton III): the effect generates nothing it can't play —
no motor rumble or warp infrasound, which would only eat headroom and pump the speaker's limiter; hiss is
cut below ~120 Hz, clicks below 150 Hz.

Presets: `new`, `subtle`, `warm` (default), `worn`, `single` (45), `shellac` (78, mono), `jazz`,
`sinatra`, `jazz78`. They are generated from the same calibrated model as V2 (`examples/gen_presets.rs`:
record condition, era, color), so dust and hiss in all of them follow the real record; only scratches,
stereo width, gain and speed are set per preset.

### Measuring

- `cargo run --release --example reference_stats -- file <wav>` and `-- model k=v ...` compare any vinyl
  recording with the model: noise spectrum per octave, clicks per second at each level above the noise
  floor, click length and brightness (all relative to the recording's own noise floor).
- `cargo run --release --example surface_stats -- k=v ...` counts crackle events per second by absolute level.

## Limitations

- In CLI mode the AU's own window doesn't open; change its parameters from the console (`au`, `au set`).
- iZotope Vinyl must be authorized, otherwise it may output silence.
- A sample rate change needs a restart.
- If the app crashes, the private tap and aggregate go away with the process and the sound returns by itself.

## Developing on a non-Mac

```sh
cargo test --lib                                     # DSP and parameters
RUSTFLAGS="--cfg vinyl_check" cargo clippy --bins    # type-check the macOS code without linking
```

## License

MIT, see [LICENSE](LICENSE).
