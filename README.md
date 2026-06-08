# utop

`utop` is a high-performance terminal system monitor for Linux and macOS, now rewritten in pure Rust.

- Zero-allocation process sampling for maximum speed.
- No third-party TUI libraries (uses ANSI escape sequences).
- Data sources are Linux `/proc` and `/sys`, and native macOS sysctl/Mach/libproc APIs.

## Features

- Real-time CPU & Memory usage.
- Process table with smooth scrolling.
- Sorting by CPU or Memory.
- Instant search/filter support.
- Adaptive layout (scales to terminal size).

## Requirements

- Linux or macOS
- Rust & Cargo
- Make (optional)

## Build

```bash
cargo build --release
# or just:
make
```

## Run

```bash
./target/release/utop
# or if installed:
utop
```

## Controls

- `q`: quit
- `j`/`k` or `↑`/`↓`: move selection
- `h`/`l` or `←`/`→`: sort by CPU or Memory
- `/`: search/filter processes
- `Esc`: clear search/filter

## License

Licensed under [UNLICENSE](LICENSE).
