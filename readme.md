# acodex_server

`acodex_server` is a Rust-based backend/server for the `Acodex plugin`. It provides a **lightweight**, **independent**, **secure**, and **fast** solution.

## Features

- Lightweight
- Secure
- fast
- uses system pty
- automatic update checking

## Installation

To install `axs` on your system, simply use the following command:

```bash
curl -L https://raw.githubusercontent.com/bajrangCoder/acodex_server/main/install.sh | bash
```

## Update  

`axs` will automatically notify you whenever a new update is available. With a simple command:  

```sh
axs update
```  

you can easily update it without any hassle.  

> [!NOTE]
> This feature is available from `v0.2.0` onwards. For older versions, please use the installation script to update.

### Example Usage

```bash
$ axs --help
CLI/Server backend for AcodeX Acode plugin

Usage: axs [OPTIONS] [COMMAND]

Commands:
  update  Update axs server
  help    Print this message or the help of the given subcommand(s)
Options:
  -p, --port <PORT>  Port to start the server [default: 8767]
  -i, --ip           Start the server on local network (ip)
  -h, --help         Print help
  -V, --version      Print version
```

> [!NOTE]
> If you encounter any issues, please [create an issue on GitHub](https://github.com/bajrangCoder/acodex_server/issues).

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
