# CuttingRoom

CuttingRoom is a keyboard-driven desktop application for browsing, culling, tagging, and organizing photographs.

## Development

Install Rust with rustup. The repository pins the Rust toolchain.
Install CMake to build the bundled JPEG codec.
On x86-64, install NASM to enable JPEG SIMD acceleration.

- On Linux, install a C/C++ toolchain, `pkg-config`, and the Fontconfig development headers. Use a Wayland or X11 session.
- On macOS, install the Xcode Command Line Tools.
- On Windows, install Visual Studio Build Tools with Desktop development with C++.

Run the app from the repository root.

```sh
cargo run -p cuttingroom --locked
```

To open an existing photo folder, pass its path at startup.

```sh
cargo run -p cuttingroom --locked -- "/path/to/photo folder"
```

For software rendering, set `SLINT_BACKEND=winit-skia-software` before starting the app.

For RAW thumbnails, install the LibRaw 0.22.3 shared library.
For HEIC thumbnails on Linux and Windows, install the libheif 1.23.7 shared library with the libde265 decoder.
For AVIF thumbnails through libheif, use a build with the dav1d AV1 decoder.
Use a libheif build with runtime plugin loading disabled.
Set `CUTTINGROOM_CODEC_DIR` to the absolute path of the directory with these libraries.
By default, the app loads the libraries from the `codecs` directory next to the executable.

### Database development

Install Diesel CLI to generate schema changes.

```sh
cargo install diesel_cli --version 2.4.0 --locked --no-default-features --features sqlite-bundled
```

Run these commands from `crates/cr-project`. Use a disposable SQLite database and your updated Diesel schema file.

```sh
diesel migration run --database-url /path/to/disposable.sqlite
diesel migration generate --database-url /path/to/disposable.sqlite --version VERSION \
  --diff-schema=/path/to/schema.rs MIGRATION_NAME
```

Review both SQL files. Run the new migration on the disposable database. Revert it to test the downgrade script.

```sh
diesel migration run --database-url /path/to/disposable.sqlite
diesel migration revert --database-url /path/to/disposable.sqlite
```

Run the checks from the repository root.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
```
