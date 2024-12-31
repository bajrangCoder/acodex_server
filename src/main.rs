mod terminal;
mod utils;

use clap::Parser;
use colored::Colorize;
use std::net::Ipv4Addr;
use terminal::start_server;
use utils::get_ip_address;

const DEFAULT_PORT: u16 = 8767;
const LOCAL_IP: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

#[derive(Parser)]
#[command(name = "acodex_server(axs)",version, author = "Raunak Raj <bajrangcoders@gmail.com>", about = "CLI of AcodeX Acode plugin", long_about = None)]
struct Cli {
    /// Port to start the server
    #[arg(short, long, default_value_t = DEFAULT_PORT, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,
    /// Start the server on local network (ip)
    #[arg(short, long)]
    ip: bool,
}

fn main() {
    let cli: Cli = Cli::parse();
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

    start_server(ip, cli.port);
}
