# acodex_server

> [!WARNING]
> **acodex_server** is currently **under development**. Some features may be incomplete or subject to change or not implemented yet. Use it with caution.

`acodex_server` is a Rust-based backend/server for the `Acodex plugin`. It provides a **lightweight**, **independent**, **secure**, and **blazingly fast** solution.

## Features

- Lightweight
- Independent (serves as a binary)
- Secure
- Blazingly fast

## Usage

To use `acodex_server`, follow these steps:

1. **Install from Source:**
   - Clone the repository.
   - Ensure that Rust is installed on your system.
   - Navigate to the project directory.
   - Build the project:
     ```bash
     cargo build --release
     ```

2. **Run the Binary:**
   - After building, the binary will be available in `/target/release/axs`.
   - Run the binary:
     ```bash
     ./target/release/axs --help
     ```

## Building from Source

To build acodex_server from source, follow these steps:

1. Clone the repository:
   ```bash
   git clone https://github.com/your-username/acodex_server.git
   ```

2. Ensure that Rust is installed on your system.

3. Navigate to the project directory:
   ```bash
   cd acodex_server
   ```

4. Build the project:
   ```bash
   cargo build --release
   ```

5. Use the generated binary located at `/target/release/axs`.
