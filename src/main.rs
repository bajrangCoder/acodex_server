mod lsp;
mod terminal;
mod updates;
mod utils;

use clap::{Parser, Subcommand};
use colored::Colorize;
use lsp::{start_lsp_server, LspBridgeConfig};
use std::net::Ipv4Addr;
use terminal::{set_default_command, start_server};
use updates::UpdateChecker;
use utils::get_ip_address;

const DEFAULT_PORT: u16 = 8767;
const LOCAL_IP: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

#[derive(Parser)]
#[command(name = "acodex_server(axs)",version, author = "Raunak Raj <bajrangcoders@gmail.com>", about = "CLI/Server backend to serve pty over socket", long_about = None)]
struct Cli {
    /// Port to start the server
    #[arg(short, long, default_value_t = DEFAULT_PORT, value_parser = clap::value_parser!(u16).range(1..), global = true)]
    port: u16,
    /// Start the server on local network (ip)
    #[arg(short, long, global = true)]
    ip: bool,
    /// Custom command or shell for interactive PTY (e.g. "/usr/bin/bash")
    #[arg(short = 'c', long = "command")]
    command_override: Option<String>,
    /// Allow all origins for CORS (dangerous). By default only https://localhost is allowed.
    #[arg(long = "allow-any-origin", global = true)]
    allow_any_origin: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Update axs server
    Update,
    /// Start a WebSocket LSP bridge for a stdio language server
    Lsp {
        /// Session ID for port discovery (allows multiple instances of same server)
        #[arg(short = 's', long)]
        session: Option<String>,
        /// The language server binary to run (e.g. "rust-analyzer")
        server: String,
        /// Additional arguments to forward to the language server
        #[arg(trailing_var_arg = true)]
        server_args: Vec<String>,
    },
}

fn print_update_available(current_version: &str, new_version: &str) {
    println!("\n{}", "═".repeat(40).yellow());
    println!("{}", "  🎉  Update Available!".bright_yellow().bold());
    println!("  Current version: {}", current_version.bright_red());
    println!("  Latest version:  {}", new_version.bright_green());
    println!("  To update, run: {} {}", "axs".cyan(), "update".cyan());
    println!("{}\n", "═".repeat(40).yellow());
}

async fn check_updates_in_background() {
    let checker = UpdateChecker::new(env!("CARGO_PKG_VERSION"));
    match checker.check_update().await {
        Ok(Some(version)) => {
            print_update_available(env!("CARGO_PKG_VERSION"), &version);
        }
        Err(e) => eprintln!(
            "{} {}",
            "⚠️".yellow(),
            format!("Failed to check for updates: {e}").red()
        ),
        _ => {}
    }
}

#[tokio::main]
async fn main() {
    let cli: Cli = Cli::parse();

    let Cli {
        port,
        ip,
        command_override,
        allow_any_origin,
        command,
    } = cli;

    match command {
        Some(Commands::Update) => {
            println!("{} {}", "⟳".blue().bold(), "Checking for updates...".blue());

            let checker = UpdateChecker::new(env!("CARGO_PKG_VERSION"));

            match checker.check_update().await {
                Ok(Some(version)) => {
                    println!(
                        "{} Found new version: {}",
                        "↓".bright_green(),
                        version.green()
                    );
                    println!(
                        "{} {}",
                        "⟳".blue(),
                        "Downloading and installing update...".blue()
                    );

                    match checker.update().await {
                        Ok(()) => {
                            println!(
                                "\n{} {}",
                                "✓".bright_green().bold(),
                                "Update successful! Please restart axs.".green().bold()
                            );
                        }
                        Err(e) => {
                            eprintln!(
                                "\n{} {} {}",
                                "✗".red().bold(),
                                "Update failed:".red().bold(),
                                e
                            );
                            std::process::exit(1);
                        }
                    }
                }
                Ok(None) => {
                    println!(
                        "{} {}",
                        "✓".bright_green().bold(),
                        "You're already on the latest version!".green().bold()
                    );
                }
                Err(e) => {
                    eprintln!(
                        "{} {} {}",
                        "✗".red().bold(),
                        "Failed to check for updates:".red().bold(),
                        e
                    );
                    std::process::exit(1);
                }
            }
        }
        Some(Commands::Lsp {
            session,
            server,
            server_args,
        }) => {
            let host = if ip {
                get_ip_address().unwrap_or_else(|| {
                    println!(
                        "{} localhost.",
                        "Error: IP address not found. Starting server on"
                            .red()
                            .bold()
                    );
                    LOCAL_IP
                })
            } else {
                LOCAL_IP
            };

            let config = LspBridgeConfig {
                program: server,
                args: server_args,
            };

            // Use specified port if not default, otherwise auto-select
            let lsp_port = if port != DEFAULT_PORT {
                Some(port)
            } else {
                None
            };

            start_lsp_server(host, lsp_port, session, allow_any_origin, config).await;
        }
        None => {
            tokio::task::spawn(check_updates_in_background());

            if let Some(cmd) = command_override {
                // Set custom default command for interactive terminals
                set_default_command(cmd);
            }

            let ip = if ip {
                get_ip_address().unwrap_or_else(|| {
                    println!(
                        "{} localhost.",
                        "Error: IP address not found. Starting server on"
                            .red()
                            .bold()
                    );
                    LOCAL_IP
                })
            } else {
                LOCAL_IP
            };

            start_server(ip, port, allow_any_origin).await;
        }
    }
}
