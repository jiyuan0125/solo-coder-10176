use std::{
    fs::File,
    io::{BufRead, BufReader},
    net::IpAddr,
    str::FromStr,
};

use crate::session::Error;

use cidr_utils::cidr::IpCidr;
use lazy_regex::{Lazy, lazy_regex};
use regex::Regex;

static IPV4_RANGE_PARSER: Lazy<Regex> = lazy_regex!(r"^(\d+)\.(\d+)\.(\d+)\.(\d+)-(\d+)$");
static IPV4_RANGE_WITH_PORT_PARSER: Lazy<Regex> =
    lazy_regex!(r"^(\d+)\.(\d+)\.(\d+)\.(\d+)-(\d+):(\d+)$");
static IPV6_RANGE_PARSER: Lazy<Regex> = lazy_regex!(r"^(.+)-(\d+)$");

fn is_probably_cidr(s: &str) -> bool {
    if let Some((prefix, suffix)) = s.split_once('/') {
        if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        if let Ok(mask) = suffix.parse::<u8>() {
            let has_dot = prefix.contains('.');
            let has_colon = prefix.contains(':');
            if has_dot && mask <= 32 {
                return true;
            }
            if has_colon && mask <= 128 {
                return true;
            }
        }
    }
    false
}

fn parse_port_list(ports_str: &str) -> Result<Vec<u16>, Error> {
    let trimmed = ports_str.trim_start_matches('[').trim_end_matches(']');
    let mut ports = Vec::new();

    for part in trimmed.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let port: u16 = part
            .parse()
            .map_err(|e| format!("invalid port '{}': {}", part, e))?;
        ports.push(port);
    }

    if ports.is_empty() {
        return Err("empty port list".to_owned());
    }

    Ok(ports)
}

fn parse_ipv4_range(a: &str, b: &str, c: &str, start: &str, stop: &str) -> Result<Vec<String>, Error> {
    let a: u8 = a.parse::<u8>().map_err(|e: std::num::ParseIntError| e.to_string())?;
    let b: u8 = b.parse::<u8>().map_err(|e: std::num::ParseIntError| e.to_string())?;
    let c: u8 = c.parse::<u8>().map_err(|e: std::num::ParseIntError| e.to_string())?;
    let start: u8 = start.parse::<u8>().map_err(|e: std::num::ParseIntError| e.to_string())?;
    let stop: u8 = stop.parse::<u8>().map_err(|e: std::num::ParseIntError| e.to_string())?;

    if stop < start {
        return Err(format!(
            "invalid ip range, {} is greater than {}",
            start, stop
        ));
    }

    let mut range = Vec::new();
    for d in start..=stop {
        range.push(format!("{}.{}.{}.{}", a, b, c, d));
    }

    Ok(range)
}

fn parse_ipv6_range(prefix: &str, range_end: &str) -> Result<Vec<String>, Error> {
    let end_num: u32 = range_end.parse::<u32>().map_err(|e: std::num::ParseIntError| e.to_string())?;

    if prefix.contains("::") {
        let parts: Vec<&str> = prefix.splitn(2, "::").collect();
        let left = parts[0];
        let right = if parts.len() > 1 { parts[1] } else { "" };

        if let Ok(start_num) = right.parse::<u32>() {
            if end_num < start_num {
                return Err(format!(
                    "invalid ipv6 range, {} is greater than {}",
                    start_num, end_num
                ));
            }
            let mut results = Vec::new();
            for n in start_num..=end_num {
                if left.is_empty() {
                    results.push(format!("::{}", n));
                } else {
                    results.push(format!("{}::{}", left, n));
                }
            }
            return Ok(results);
        }
    }

    let last_colon = prefix.rfind(':');
    if let Some(pos) = last_colon {
        let prefix_part = &prefix[..=pos];
        let last_part = &prefix[pos + 1..];

        if let Ok(start_num) = last_part.parse::<u32>() {
            if end_num < start_num {
                return Err(format!(
                    "invalid ipv6 range, {} is greater than {}",
                    start_num, end_num
                ));
            }
            let mut results = Vec::new();
            for n in start_num..=end_num {
                results.push(format!("{}{}", prefix_part, n));
            }
            return Ok(results);
        }
    }

    Err(format!("invalid ipv6 range format: {}-{}", prefix, range_end))
}

fn parse_multiple_targets_atom(expression: &str) -> Result<Vec<String>, Error> {
    if let Some(path) = expression.strip_prefix('@') {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let reader = BufReader::new(file);

        Ok(reader
            .lines()
            .map(|l| l.unwrap_or_default())
            .filter(|s| !s.trim().is_empty())
            .collect())
    } else if let Some(caps) = IPV4_RANGE_WITH_PORT_PARSER.captures(expression) {
        let ips = parse_ipv4_range(
            caps.get(1).unwrap().as_str(),
            caps.get(2).unwrap().as_str(),
            caps.get(3).unwrap().as_str(),
            caps.get(4).unwrap().as_str(),
            caps.get(5).unwrap().as_str(),
        )?;
        let port = caps.get(6).unwrap().as_str();
        Ok(ips.iter().map(|ip| format!("{}:{}", ip, port)).collect())
    } else if let Some(caps) = IPV4_RANGE_PARSER.captures(expression) {
        parse_ipv4_range(
            caps.get(1).unwrap().as_str(),
            caps.get(2).unwrap().as_str(),
            caps.get(3).unwrap().as_str(),
            caps.get(4).unwrap().as_str(),
            caps.get(5).unwrap().as_str(),
        )
    } else if expression.starts_with('[') {
        let (addr_part, ports_part) = expression
            .split_once("]:")
            .ok_or_else(|| format!("invalid ipv6 target: {}", expression))?;
        let addr = addr_part.strip_prefix('[').unwrap();

        if let Ok(cidr) = IpCidr::from_str(addr) {
            let ports = parse_port_list(ports_part)?;
            let mut results = Vec::new();
            for ip in cidr.iter() {
                for port in &ports {
                    results.push(format!("[{}]:{}", ip.address(), port));
                }
            }
            Ok(results)
        } else if let Ok(_) = addr.parse::<std::net::Ipv6Addr>() {
            let ports = parse_port_list(ports_part)?;
            let mut results = Vec::new();
            for port in &ports {
                results.push(format!("[{}]:{}", addr, port));
            }
            Ok(results)
        } else {
            Err(format!("invalid ipv6 address or cidr: {}", addr))
        }
    } else if expression.contains(":[") && expression.ends_with(']') {
        let (cidr_part, ports_part) = expression.split_once(":[").unwrap();
        let ports_str = ports_part.trim_end_matches(']');
        let ports = parse_port_list(ports_str)?;

        if let Ok(cidr) = IpCidr::from_str(cidr_part) {
            let is_ipv6 = cidr_part.contains(':');
            let mut results = Vec::new();
            for ip in cidr.iter() {
                for port in &ports {
                    if is_ipv6 {
                        results.push(format!("[{}]:{}", ip.address(), port));
                    } else {
                        results.push(format!("{}:{}", ip.address(), port));
                    }
                }
            }
            Ok(results)
        } else {
            Err(format!("invalid cidr: {}", cidr_part))
        }
    } else if expression.contains('/') && is_probably_cidr(expression) {
        if let Ok(cidr) = IpCidr::from_str(expression) {
            Ok(cidr.iter().map(|ip| ip.address().to_string()).collect())
        } else {
            Err(format!("invalid cidr: {}", expression))
        }
    } else {
        if let Some(caps) = IPV6_RANGE_PARSER.captures(expression) {
            let prefix = caps.get(1).unwrap().as_str();
            let end = caps.get(2).unwrap().as_str();
            if prefix.contains(':') {
                return parse_ipv6_range(prefix, end);
            }
        }

        if expression.is_empty() {
            return Err("invalid target: empty string".to_owned());
        }

        if let Ok(ip) = expression.parse::<IpAddr>() {
            return Ok(vec![ip.to_string()]);
        }

        if let Some((host_part, port_part)) = expression.rsplit_once(':') {
            if port_part.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(_) = host_part.parse::<IpAddr>() {
                    return Ok(vec![expression.to_owned()]);
                }
                let host = host_part.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host_part);
                if let Ok(_) = host.parse::<IpAddr>() {
                    return Ok(vec![expression.to_owned()]);
                }
                if !host.is_empty() && !host.contains(':') && !host.contains('-') {
                    return Ok(vec![expression.to_owned()]);
                }
            }
        }

        if expression.contains("://") {
            return Ok(vec![expression.to_owned()]);
        }

        if !expression.contains(':')
            && !expression.contains('-')
            && !expression.chars().all(|c| c.is_ascii_digit() || c == '.')
        {
            return Ok(vec![expression.to_owned()]);
        }

        if let Some(colon_idx) = expression.find(':') {
            let before = &expression[..colon_idx];
            let after = &expression[colon_idx + 1..];
            if before.chars().all(|c| c.is_ascii_digit() || c == '.')
                && !after.is_empty()
                && !after.contains(':')
                && !before.contains('-')
            {
                if let Ok(_) = before.parse::<std::net::Ipv4Addr>() {
                    return Ok(vec![expression.to_owned()]);
                }
            }
        }

        if !expression.contains(':')
            && !expression.contains('-')
        {
            if let Ok(_) = expression.parse::<std::net::Ipv4Addr>() {
                return Ok(vec![expression.to_owned()]);
            }
        }

        Err(format!("invalid target expression: {}", expression))
    }
}

pub(crate) fn parse_multiple_targets(expression: &str) -> Result<Vec<String>, Error> {
    let mut all = vec![];
    let mut bracket_depth = 0i32;
    let mut current = String::new();

    for ch in expression.chars() {
        match ch {
            '[' => {
                bracket_depth += 1;
                current.push(ch);
            }
            ']' => {
                bracket_depth -= 1;
                current.push(ch);
            }
            ',' if bracket_depth == 0 => {
                let trimmed = current.trim().to_owned();
                if !trimmed.is_empty() {
                    all.extend(parse_multiple_targets_atom(&trimmed)?);
                }
                current.clear();
            }
            _ => {
                current.push(ch);
            }
        }
    }

    let trimmed = current.trim().to_owned();
    if !trimmed.is_empty() {
        all.extend(parse_multiple_targets_atom(&trimmed)?);
    }

    Ok(all)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;

    use super::parse_multiple_targets;

    #[test]
    fn can_parse_single() {
        let expected = vec!["127.0.0.1:22".to_owned()];
        let res = parse_multiple_targets("127.0.0.1:22").unwrap();
        assert_eq!(res, expected);

        let expected = vec!["http://www.something.it:8000".to_owned()];
        let res = parse_multiple_targets("http://www.something.it:8000").unwrap();
        assert_eq!(res, expected);

        let expected = vec!["host:1234".to_owned()];
        let res = parse_multiple_targets(",,host:1234,,,").unwrap();
        assert_eq!(res, expected);
    }

    #[test]
    fn can_parse_from_file() {
        let num_items = 5;
        let tmpdir = tempfile::tempdir().unwrap();
        let tmppath = tmpdir.path().join("targets.txt");
        let mut tmptargets = File::create(&tmppath).unwrap();
        let mut expected = vec![];

        for i in 0..num_items {
            writeln!(tmptargets, "127.0.0.1:{}", i).unwrap();
            expected.push(format!("127.0.0.1:{}", i));
        }
        tmptargets.flush().unwrap();
        drop(tmptargets);

        let res = parse_multiple_targets(&format!("@{}", tmppath.to_str().unwrap())).unwrap();
        assert_eq!(res, expected);
    }

    #[test]
    fn returns_error_for_wrong_filename() {
        let res = parse_multiple_targets("@i-do-not-exist.lol");
        assert!(res.is_err());
    }

    #[test]
    fn can_parse_comma_separated() {
        let expected = Ok(vec![
            "127.0.0.1:22".to_owned(),
            "www.google.com".to_owned(),
            "cnn.com".to_owned(),
            "8.8.8.8:4444".to_owned(),
        ]);
        let res = parse_multiple_targets("127.0.0.1:22, www.google.com, cnn.com,, 8.8.8.8:4444");
        assert_eq!(res, expected);
    }

    #[test]
    fn can_parse_ip_range_without_port() {
        let expected = Ok(vec![
            "192.168.1.1".to_owned(),
            "192.168.1.2".to_owned(),
            "192.168.1.3".to_owned(),
            "192.168.1.4".to_owned(),
            "192.168.1.5".to_owned(),
        ]);
        let res = parse_multiple_targets("192.168.1.1-5");
        assert_eq!(res, expected);
    }

    #[test]
    fn can_parse_ip_range_with_port() {
        let expected = Ok(vec![
            "192.168.1.1:1234".to_owned(),
            "192.168.1.2:1234".to_owned(),
            "192.168.1.3:1234".to_owned(),
            "192.168.1.4:1234".to_owned(),
            "192.168.1.5:1234".to_owned(),
        ]);
        let res = parse_multiple_targets("192.168.1.1-5:1234");
        assert_eq!(res, expected);
    }

    #[test]
    fn can_parse_ipv4_cidr_without_port() {
        let expected = Ok(vec![
            "192.168.1.0".to_owned(),
            "192.168.1.1".to_owned(),
            "192.168.1.2".to_owned(),
            "192.168.1.3".to_owned(),
        ]);
        let res = parse_multiple_targets("192.168.1.0/30");
        assert_eq!(res, expected);
    }

    #[test]
    fn can_parse_ipv4_cidr_with_port() {
        let expected = Ok(vec![
            "192.168.1.0:1234".to_owned(),
            "192.168.1.1:1234".to_owned(),
            "192.168.1.2:1234".to_owned(),
            "192.168.1.3:1234".to_owned(),
        ]);
        let res = parse_multiple_targets("192.168.1.0/30:[1234]");
        assert_eq!(res, expected);
    }

    #[test]
    fn can_parse_ipv6_cidr_without_port() {
        let expected = Ok(vec![
            "2001:4f8:3:ba:2e0:81ff:fe22:0".to_owned(),
            "2001:4f8:3:ba:2e0:81ff:fe22:1".to_owned(),
            "2001:4f8:3:ba:2e0:81ff:fe22:2".to_owned(),
            "2001:4f8:3:ba:2e0:81ff:fe22:3".to_owned(),
        ]);
        let res = parse_multiple_targets("2001:4f8:3:ba:2e0:81ff:fe22:0/126");
        assert_eq!(res, expected);
    }

    #[test]
    fn can_parse_ipv6_cidr_with_port() {
        let expected = Ok(vec![
            "[2001:4f8:3:ba:2e0:81ff:fe22:0]:1234".to_owned(),
            "[2001:4f8:3:ba:2e0:81ff:fe22:1]:1234".to_owned(),
            "[2001:4f8:3:ba:2e0:81ff:fe22:2]:1234".to_owned(),
            "[2001:4f8:3:ba:2e0:81ff:fe22:3]:1234".to_owned(),
        ]);
        let res = parse_multiple_targets("2001:4f8:3:ba:2e0:81ff:fe22:0/126:[1234]");
        assert_eq!(res, expected);
    }

    #[test]
    fn can_parse_combined() {
        let num_items = 5;
        let tmpdir = tempfile::tempdir().unwrap();
        let tmppath = tmpdir.path().join("targets.txt");
        let mut tmptargets = File::create(&tmppath).unwrap();
        let expected = vec![
            "192.168.1.1",
            "127.0.0.1:0",
            "127.0.0.1:1",
            "127.0.0.1:2",
            "127.0.0.1:3",
            "127.0.0.1:4",
            "8.8.8.8",
            "8.8.8.9",
            "8.8.8.10",
            "8.8.8.11",
        ];

        for i in 0..num_items {
            writeln!(tmptargets, "127.0.0.1:{}", i).unwrap();
        }
        tmptargets.flush().unwrap();
        drop(tmptargets);

        let res = parse_multiple_targets(&format!(
            "192.168.1.1, @{}, 8.8.8.8/30",
            tmppath.to_str().unwrap()
        ))
        .unwrap();
        assert_eq!(res, expected);
    }

    #[test]
    fn can_parse_ipv6_range() {
        let expected = Ok(vec![
            "2001:db8::1".to_owned(),
            "2001:db8::2".to_owned(),
            "2001:db8::3".to_owned(),
            "2001:db8::4".to_owned(),
            "2001:db8::5".to_owned(),
        ]);
        let res = parse_multiple_targets("2001:db8::1-5");
        assert_eq!(res, expected);
    }

    #[test]
    fn can_parse_ipv6_with_brackets_and_port() {
        let expected = Ok(vec!["[2001:db8::1]:8080".to_owned()]);
        let res = parse_multiple_targets("[2001:db8::1]:8080");
        assert_eq!(res, expected);
    }

    #[test]
    fn can_parse_ipv4_cidr_with_port_list() {
        let expected = Ok(vec![
            "192.168.1.0:80".to_owned(),
            "192.168.1.0:443".to_owned(),
            "192.168.1.0:8080".to_owned(),
            "192.168.1.1:80".to_owned(),
            "192.168.1.1:443".to_owned(),
            "192.168.1.1:8080".to_owned(),
            "192.168.1.2:80".to_owned(),
            "192.168.1.2:443".to_owned(),
            "192.168.1.2:8080".to_owned(),
            "192.168.1.3:80".to_owned(),
            "192.168.1.3:443".to_owned(),
            "192.168.1.3:8080".to_owned(),
        ]);
        let res = parse_multiple_targets("192.168.1.0/30:[80,443,8080]");
        assert_eq!(res, expected);
    }

    #[test]
    fn can_parse_ipv4_full_octet_range() {
        let res = parse_multiple_targets("192.168.1.0-255").unwrap();
        assert_eq!(res.len(), 256);
        assert_eq!(res[0], "192.168.1.0");
        assert_eq!(res[255], "192.168.1.255");
    }

    #[test]
    fn returns_error_for_invalid_targets() {
        let cases = vec![
            "10.0.0.1-2-3",
            "host::bad",
            "192.168.1.5.6",
        ];
        for case in cases {
            let res = parse_multiple_targets(case);
            assert!(res.is_err(), "expected error for '{}', got {:?}", case, res);
            let err = res.unwrap_err();
            assert!(err.contains("invalid"), "error '{}' should contain 'invalid'", err);
        }
    }
}
