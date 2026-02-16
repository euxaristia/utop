# rtop

A balanced system monitor written in Rust, designed to fill the niche between the feature-richness of `btop` and the simplicity of `htop`.

![License](https://img.shields.io/badge/license-GPLv3-blue.svg)

## Features

- **CPU Monitoring**: Real-time usage gauge and a history sparkline.
- **Memory Tracking**: Visual representation of used vs. total memory with percentage.
- **Process Management**: Interactive list of processes sorted by CPU usage.
- **Responsive TUI**: Built with [Ratatui](https://ratatui.rs/) for a modern terminal experience.

## Installation

### From Source

Ensure you have [Rust](https://www.rust-lang.org/tools/install) installed.

```bash
git clone https://github.com/euxaristia/rtop.git
cd rtop
cargo install --path .
```

By default, this installs the binary to `~/.cargo/bin`. See [Installation to ~/.local](#installation-to-local) for other options.

## Controls

- `q`: Quit the application
- `↑` / `k`: Scroll up in the process list
- `↓` / `j`: Scroll down in the process list

## Installation to ~/.local

To install `rtop` directly to `~/.local/bin`, use the `--root` flag:

```bash
cargo install --path . --root ~/.local
```

*Note: Ensure `~/.local/bin` is in your `$PATH`.*

## License

Licensed under the [GNU General Public License v3.0](LICENSE).
