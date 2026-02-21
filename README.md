# rtop (Swift)

`rtop` is now a pure Swift terminal system monitor for Linux.

- No third-party libraries.
- Data source is Linux `/proc`.
- Rendering is ANSI terminal output.

## Features

- Real-time CPU usage
- Memory usage (`MemTotal` / `MemAvailable`)
- Network throughput (auto-selects busiest non-loopback interface)
- Process table sorted by CPU%
- Keyboard navigation for process selection

## Requirements

- Linux
- Swift toolchain (SwiftPM)

## Build

```bash
swift build -c release
```

## Run

```bash
swift run -c release rtop
```

## Controls

- `q`: quit
- `j` / `↓`: move selection down
- `k` / `↑`: move selection up

## Notes

- This implementation intentionally uses only Swift stdlib/Foundation and Linux system APIs.
- No external package dependencies are used.

## License

Licensed under [GNU GPL v3.0](LICENSE).
