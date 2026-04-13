# iv — An Extremely Fast Image Viewer

A lean, GPU-accelerated image viewer built in Rust. Opens a folder and shows its images instantly using progressive rendering — EXIF thumbnails first, then higher quality as resources allow.

Supports JPEG, PNG, WebP, TIFF, BMP, GIF, and RAW formats (DNG, CR2, NEF, ARW, etc.) with first-class performance on both local SSDs and network shares (SMB/NFS).

## Prerequisites

### Rust Toolchain

Install Rust via [rustup](https://rustup.rs/):

**Windows (PowerShell):**
```powershell
Invoke-WebRequest -Uri https://win.rustup.rs/x86_64 -OutFile "$env:TEMP\rustup-init.exe"
& "$env:TEMP\rustup-init.exe" -y --default-toolchain stable
# Restart your shell, or manually add to PATH:
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
```

**macOS / Linux:**
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
```

Verify installation:
```bash
cargo --version   # e.g. cargo 1.94.1
rustc --version   # e.g. rustc 1.94.1
```

### Platform Dependencies

**Windows:** No additional dependencies. The MSVC build tools are installed with Visual Studio or the [VS Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/).

**macOS:** Xcode command line tools:
```bash
xcode-select --install
```

**Linux (Debian/Ubuntu):** GPU and windowing libraries:
```bash
sudo apt install -y libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
    libxkbcommon-dev libssl-dev libgtk-3-dev
```

## Building

```bash
# Debug build (fast compile, slow runtime)
cargo build

# Release build (slow compile, fast runtime — use for perf testing)
cargo build --release
```

The binary is at `target/debug/iv` (or `target/release/iv`).

## Testing

```bash
# Run all tests
cargo test

# Run with output visible
cargo test -- --nocapture

# Run a specific test
cargo test load_jpeg_basic

# Run only unit tests (skip integration tests)
cargo test --lib

# Run only integration tests
cargo test --test image_loading
```

Tests generate synthetic images at runtime — no test fixtures need to be checked in.
Temp files are cleaned up automatically.

## Running

```bash
# Open a single image
iv path/to/image.jpg

# Open a folder (coming in Phase 1)
iv path/to/photos/
```

## Development

### VS Code Setup

This project includes VS Code configuration in `.vscode/`:

- **tasks.json** — Build tasks (Ctrl+Shift+B):
  - `build debug` — Fast debug build
  - `build release` — Optimized release build
  - `clippy` — Lint check
  - `test` — Run all tests (Ctrl+Shift+T → select `test`)
  - `test (release)` — Run tests with optimizations
- **launch.json** — Debug configurations (F5):
  - `Debug iv` — Build and debug with a sample image path (edit the `args` to point to a real image)
  - `Debug iv (release)` — Debug the release build
- **settings.json** — Rust-analyzer configuration

**Recommended VS Code extensions:**
- [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer) — Language support, completion, diagnostics
- [CodeLLDB](https://marketplace.visualstudio.com/items?itemName=vadimcn.vscode-lldb) — Native debugger (required for launch configs)

### Project Structure

```
iv/
├── .vscode/           # VS Code build/debug/settings
├── src/
│   ├── main.rs        # Entry point, CLI parsing, eframe launch
│   └── app.rs         # eframe::App — image loading & display
│                      #   (pure functions + unit tests at bottom)
├── tests/
│   ├── common/
│   │   └── mod.rs     # Test helpers — synthetic image generation
│   └── image_loading.rs  # Integration tests for the load pipeline
├── Cargo.toml
├── PLAN.md            # Phased development plan
└── README.md
```

See [PLAN.md](PLAN.md) for the full development roadmap.

### Git Hooks

After cloning, activate the pre-commit format check:

```bash
git config core.hooksPath .githooks
```

This runs `cargo fmt --check` before each commit. If formatting fails, run `cargo fmt` and retry.

### Debug Mode

Set `IV_DEBUG=1` to show decode timing overlays on thumbnails:

```bash
# PowerShell
$env:IV_DEBUG = "1"
iv path/to/photos/

# Bash
IV_DEBUG=1 iv path/to/photos/
```

The overlay shows:
- **EXIF X.Xms** — Time to extract and decode the embedded EXIF thumbnail (green = used)
- **Full X.Xms** — Time for full decode + downscale (shown when EXIF was not available)
