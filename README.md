# vinylizer

Звук «с пластинки» для всего системного аудио macOS в реальном времени. Написан на Rust, без SoundSource.
Работает со встроенным DSP, а при желании хостит AU-плагин (например, iZotope Vinyl).

Нужна macOS 14.2 или новее (Core Audio process taps).

## Сборка и запуск

```sh
cargo build --release
./target/release/vinylizer                    # всё системное аудио → текущий выход (Marshall)
./target/release/vinylizer --preset worn
./target/release/vinylizer --app spotify      # только Spotify
./target/release/vinylizer --au Vinyl         # iZotope Vinyl вместо встроенного DSP
./target/release/vinylizer --au Vinyl --chain # iZotope Vinyl + встроенный DSP после него
./target/release/vinylizer --list-devices | --list-apps | --list-plugins
```

При первом запуске macOS попросит разрешение «System Audio Recording» для терминала.
Если разрешения нет, tap отдаёт тишину. Выдать его можно в System Settings → Privacy & Security → Screen & System Audio Recording.

Пока программа работает, доступна консоль: `help`, `set crackle 0.5`, `preset shellac`, `bypass`, `meter`,
`au` (параметры AU), `au set <id> <v>`, `au presets`, `au preset <n>`, `quit`.

## Как это устроено

```
CATapDescription (global, exclude self | per-app), muteBehavior = mutedWhenTapped
        │  оригинальный звук приложений до колонки не доходит
        ▼
Private aggregate device: [Marshall (clock master)] + [tap, drift compensation]
        │
        ▼  один HAL IOProc, realtime-поток Core Audio
tap buffer ──► DSP / AU ──► output buffer Marshall
```

- **Без промежуточного буфера и лишнего потока.** Tap и устройство вывода живут в одном агрегате с общим клоком,
  поэтому вход и выход приходят в один callback. Встроенный DSP читает прямо из буфера tap'а и пишет прямо
  в буфер устройства, без промежуточных копий. На AU-пути есть один deinterleave и один interleave:
  AU v2 работает с planar-форматом.
- **Горячий путь realtime-safe.** Нет аллокаций, локов, syscall'ов и логов. Параметры передаются через атомики
  с версионированием, коэффициенты фильтров пересчитываются раз в блок и только при изменении. `panic = "abort"`,
  поэтому unwind через FFI невозможен.
- **io_uring.** Его нет на macOS, а Core Audio сам вызывает нас в RT-потоке. Syscall'ов в горячем пути нет вообще.
- **Без зависимостей**, кроме `libc`. Биндинги к CoreAudio, AudioToolbox, CoreFoundation и objc runtime написаны
  вручную (`src/ffi.rs`), раскладки структур проверены тестом.
- Если у устройства вывода есть микрофон (BT-гарнитура), его входные потоки отключаются через
  `kAudioDevicePropertyIOProcStreamUsage`, чтобы Bluetooth не переключился в HFP.

### Встроенный vinyl DSP (`src/dsp.rs`)

Модулированная задержка (wow 0.555 Гц, flutter 6.5 Гц, медленный дрейф) → HP 28 Гц → low-shelf «тепло» →
2×LP с частотой среза от `age` → мягкая сатурация `tanh` → ширина стерео (M/S) → треск и щелчки, шипение, рокот мотора.
Dry-сигнал берётся из той же линии задержки, так что при `wet < 1` гребенчатого фильтра нет. Задержка 3 мс.

Пресеты: `subtle`, `warm` (по умолчанию), `worn`, `shellac` (моно, как 78 об/мин).

## Ограничения

- GUI AU-плагина не открывается, параметры меняются из консоли (`au`, `au set`).
- iZotope Vinyl должен быть авторизован, иначе он может выдавать тишину.
- При смене устройства вывода или sample rate нужен перезапуск.
- Если программа упала, private tap и агрегат уничтожаются вместе с процессом, и звук возвращается сам.

## Разработка на не-Mac

```sh
cargo test --lib                                     # DSP и параметры
RUSTFLAGS="--cfg vinyl_check" cargo clippy --bins    # type-check macOS-кода без линковки
```
