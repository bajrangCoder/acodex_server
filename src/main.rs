mod terminal;
mod updates;
mod utils;

use clap::{Parser, Subcommand};
use colored::Colorize;
use std::net::Ipv4Addr;
use terminal::start_server;
use updates::UpdateChecker;
use utils::get_ip_address;

const DEFAULT_PORT: u16 = 8767;
const LOCAL_IP: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

#[derive(Parser)]
#[command(name = "acodex_server(axs)",version, author = "Raunak Raj <bajrangcoders@gmail.com>", about = "CLI/Server backend for AcodeX Acode plugin", long_about = None)]
struct Cli {
    /// Port to start the server
    #[arg(short, long, default_value_t = DEFAULT_PORT, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,
    /// Start the server on local network (ip)
    #[arg(short, long)]
    ip: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Update axs server
    Update,
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
            format!("Failed to check for updates: {}", e).red()
        ),
        _ => {}
    }
}

#[tokio::main]
async fn main() {
    let cli: Cli = Cli::parse();

    match cli.command {
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
        None => {
            tokio::task::spawn(check_updates_in_background());

            let ip = if cli.ip {
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

            start_server(ip, cli.port).await;
        }
    }
}
