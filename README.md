<p align="center">
  <img src="logo.png" alt="RYS logo" width="440">
</p>

# The RYS Programming Language

**RYS** (Rapid Yet Safe) is a programming language with Python-style syntax, implemented in Rust. This repository contains the RYS lexer, parser, bytecode compiler, virtual machine, verifier, and bytecode cache.

[Quick start](#quick-start) · [Building from source](#building-from-source) · [Usage](#usage) · [Contributing](#contributing)

## Quick start

Create a file named `hello.rys`:

```python
print("Hello, world!")
```

Build RYS as described below, then run:

```sh
./target/release/rys hello.rys
```

Running RYS without a file starts the interactive REPL. Type `exit` to quit.

## Building from source

You'll need the Rust toolchain and Cargo. Clone the repository and build a release binary:

```sh
git clone https://github.com/Amogrotex/rys-language.git
cd rys-language
cargo build --release
```

The binary is created at `target/release/rys` (or `target/release/rys.exe` on Windows). The project declares no external Cargo dependencies. The repository also includes a prebuilt Linux x86-64 binary from the original project archive; build from source for other platforms.

## Usage

```sh
# Start the REPL
./target/release/rys

# Run a program
./target/release/rys program.rys

# Display compiled bytecode
./target/release/rys --dump program.rys

# Display the version
./target/release/rys --version
```

RYS can cache compiled bytecode in `.rysc` files. Set `RYS_NO_CACHE=1` to disable the cache:

```sh
RYS_NO_CACHE=1 ./target/release/rys program.rys
```

## Repository layout

- `src/main.rs` — lexer, parser, built-ins, and command-line interface
- `src/bytecode.rs` — bytecode compiler and virtual machine
- `src/verify.rs` — bytecode verifier
- `src/cache.rs` — compiled-bytecode cache

## Contributing

Issues and pull requests are welcome. Please describe the problem or proposed change and include a minimal example when reporting a bug. Before submitting code, build the project and run `cargo test`.

## License

A license has not yet been specified for this repository. Please contact the repository owner for permission before reusing or redistributing the code.
