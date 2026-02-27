# utop

`utop` is a high-performance terminal system monitor for Linux, written in pure C.

- Zero-allocation process sampling for maximum speed.
- No third-party TUI libraries (uses ANSI escape sequences).
- Data source is Linux `/proc` and `/sys`.

## Features

- Real-time CPU & Memory usage.
- Process table with smooth scrolling.
- Sorting by CPU or Memory.
- Instant search/filter support.
- Adaptive layout (scales to terminal size).

## Requirements

- Linux
- A C compiler (Clang recommended)
- Make

## Build

```bash
make
```

## Run

```bash
./utop
```

## Controls

- `q`: quit
- `j`/`k` or `↑`/`↓`: move selection
- `h`/`l` or `←`/`→`: sort by CPU or Memory
- `/`: search/filter processes
- `Esc`: clear search/filter

## License

Licensed under [GNU GPL v3.0](LICENSE).
