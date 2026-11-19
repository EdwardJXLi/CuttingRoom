# CuttingRoom

CuttingRoom is a keyboard-driven desktop application for browsing, culling, tagging, and organizing photographs.

## Development

Install Rust with rustup. The repository pins the Rust toolchain.

- On Linux, install a C/C++ toolchain, `pkg-config`, and the Fontconfig development headers. Use a Wayland or X11 session.
- On macOS, install the Xcode Command Line Tools.
- On Windows, install Visual Studio Build Tools with Desktop development with C++.

Run the app from the repository root.

```sh
cargo run -p cuttingroom --locked
```

For software rendering, set `SLINT_BACKEND=winit-skia-software` before starting the app.

Run the checks from the repository root.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
```
