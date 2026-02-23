# stop (Rust)

`stop` is a pure Rust terminal system monitor for Linux.

- No third-party TUI libraries (uses `libc` for terminal control).
- Data source is Linux `/proc` and `/sys`.
- Rendering is ANSI terminal output.

## Features

- Real-time CPU usage & Temperature
- Memory & Swap usage
- GPU usage (NVIDIA, AMD, Intel, VideoCore, Adreno)
- Network throughput
- Process table with sorting (CPU/Memory)
- Search/Filter support

## Requirements

- Linux
- Rust (Cargo)

## Build

```bash
cargo build --release
```

## Run

```bash
./target/release/stop
```

## Controls

- `q`: quit
- `j`/`k` or `↑`/`↓`: move selection
- `h`/`l` or `←`/`→`: sort by CPU or Memory
- `/`: search/filter processes
- `Esc`: clear search/filter

## License

Licensed under [GNU GPL v3.0](LICENSE).
