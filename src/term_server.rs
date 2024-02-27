use std::net::Ipv4Addr;

pub fn start_server(host: Ipv4Addr, port: u16) {
    println!("starting server {:?} : {}", host, port);
}
