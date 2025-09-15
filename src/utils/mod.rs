use pnet::datalink;
use std::net::Ipv4Addr;

pub fn get_ip_address() -> Option<Ipv4Addr> {
    for iface in datalink::interfaces() {
        for ip in iface.ips {
            if let pnet::ipnetwork::IpNetwork::V4(network) = ip {
                if !network.ip().is_loopback() {
                    return Some(network.ip());
                }
            }
        }
    }
    None
}

pub fn parse_u16(value: &serde_json::Value, field_name: &str) -> Result<u16, String> {
    match value {
        serde_json::Value::Number(n) if n.is_u64() => n
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .ok_or_else(|| format!("{field_name} must be a valid u16.")),
        serde_json::Value::String(s) => s
            .parse::<u16>()
            .map_err(|_| format!("{field_name} must be a valid u16 string.")),
        _ => Err(format!("{field_name} must be a number or a valid string.",)),
    }
}
