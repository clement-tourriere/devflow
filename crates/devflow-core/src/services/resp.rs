//! A minimal RESP2 client over TCP: enough to authenticate, ping, ask for
//! `INFO`, start a background save and shut a server down. Valkey and Redis
//! speak the same protocol, so both profiles share it, and devflow needs no
//! client dependency for the handful of commands it sends.
//!
//! Commands are sent as arrays of bulk strings, so arguments (a password
//! included) are never interpreted or logged. Errors returned by the server
//! are reported with the server's own text, which never contains an argument.
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// One reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(Option<Vec<u8>>),
    Array(Option<Vec<Value>>),
}

impl Value {
    /// The reply as text (simple strings and bulk strings).
    pub fn text(&self) -> Option<String> {
        match self {
            Value::Simple(text) => Some(text.clone()),
            Value::Bulk(Some(bytes)) => Some(String::from_utf8_lossy(bytes).into_owned()),
            _ => None,
        }
    }

    /// An error reply becomes an error; anything else is returned.
    pub fn ok(self) -> Result<Value> {
        match self {
            Value::Error(message) => bail!("{message}"),
            other => Ok(other),
        }
    }
}

pub struct Conn {
    reader: BufReader<TcpStream>,
}

impl Conn {
    /// Connect to `127.0.0.1:port`, giving up after `timeout` (also the limit
    /// of every later read and write).
    pub fn connect(port: u16, timeout: Duration) -> Result<Conn> {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let stream = TcpStream::connect_timeout(&addr, timeout)
            .with_context(|| format!("cannot connect to 127.0.0.1:{port}"))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        Ok(Conn {
            reader: BufReader::new(stream),
        })
    }

    /// Send one command and read its reply.
    pub fn command(&mut self, args: &[&str]) -> Result<Value> {
        let mut frame = format!("*{}\r\n", args.len()).into_bytes();
        for arg in args {
            frame.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
            frame.extend_from_slice(arg.as_bytes());
            frame.extend_from_slice(b"\r\n");
        }
        let stream = self.reader.get_mut();
        stream.write_all(&frame).context("cannot send a command")?;
        stream.flush()?;
        read_value(&mut self.reader, 0)
    }

    /// `AUTH password` (the default user). The password is never part of an
    /// error message.
    pub fn auth(&mut self, password: &str) -> Result<()> {
        match self.command(&["AUTH", password])? {
            Value::Error(message) => bail!("authentication failed: {message}"),
            _ => Ok(()),
        }
    }

    /// `PING` must answer `PONG`.
    pub fn ping(&mut self) -> Result<()> {
        let reply = self.command(&["PING"])?.ok()?;
        anyhow::ensure!(
            reply.text().as_deref() == Some("PONG"),
            "unexpected reply to PING"
        );
        Ok(())
    }

    /// `INFO [section]` as `key -> value` (comment and blank lines dropped).
    pub fn info(&mut self, section: &str) -> Result<BTreeMap<String, String>> {
        let reply = self.command(&["INFO", section])?.ok()?;
        let text = reply.text().context("INFO did not return text")?;
        Ok(parse_info(&text))
    }

    /// `CONFIG GET name` as the value.
    pub fn config_get(&mut self, name: &str) -> Result<Option<String>> {
        match self.command(&["CONFIG", "GET", name])?.ok()? {
            Value::Array(Some(items)) if items.len() >= 2 => Ok(items[1].text()),
            _ => Ok(None),
        }
    }

    /// `SHUTDOWN NOSAVE`: the server exits without writing its dataset. The
    /// connection is closed by the server instead of answering.
    pub fn shutdown_nosave(&mut self) {
        let _ = self.command(&["SHUTDOWN", "NOSAVE"]);
    }
}

/// The `key:value` lines of an `INFO` reply.
pub fn parse_info(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// Deepest reply nesting accepted; INFO and CONFIG GET are flat.
const MAX_DEPTH: usize = 8;
/// Largest bulk string accepted (an INFO reply is a few KiB).
const MAX_BULK: usize = 16 * 1024 * 1024;

fn read_line(reader: &mut impl BufRead) -> Result<String> {
    let mut line = Vec::new();
    reader
        .read_until(b'\n', &mut line)
        .context("cannot read a reply")?;
    anyhow::ensure!(!line.is_empty(), "the server closed the connection");
    anyhow::ensure!(line.ends_with(b"\r\n"), "malformed reply");
    line.truncate(line.len() - 2);
    String::from_utf8(line).context("reply is not UTF-8")
}

fn read_value(reader: &mut impl BufRead, depth: usize) -> Result<Value> {
    anyhow::ensure!(depth <= MAX_DEPTH, "reply is nested too deeply");
    let line = read_line(reader)?;
    let (kind, rest) = line.split_at(line.chars().next().map_or(0, char::len_utf8));
    Ok(match kind {
        "+" => Value::Simple(rest.to_string()),
        "-" => Value::Error(rest.to_string()),
        ":" => Value::Int(rest.parse().context("malformed integer reply")?),
        "$" => {
            let len: i64 = rest.parse().context("malformed bulk length")?;
            if len < 0 {
                Value::Bulk(None)
            } else {
                let len = len as usize;
                anyhow::ensure!(len <= MAX_BULK, "bulk reply is too large");
                let mut bytes = vec![0u8; len + 2];
                reader
                    .read_exact(&mut bytes)
                    .context("cannot read a bulk reply")?;
                anyhow::ensure!(bytes.ends_with(b"\r\n"), "malformed bulk reply");
                bytes.truncate(len);
                Value::Bulk(Some(bytes))
            }
        }
        "*" => {
            let len: i64 = rest.parse().context("malformed array length")?;
            if len < 0 {
                Value::Array(None)
            } else {
                anyhow::ensure!(len <= 4096, "array reply is too large");
                let mut items = Vec::with_capacity(len as usize);
                for _ in 0..len {
                    items.push(read_value(reader, depth + 1)?);
                }
                Value::Array(Some(items))
            }
        }
        _ => bail!("unexpected reply"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(bytes: &[u8]) -> Result<Value> {
        read_value(&mut Cursor::new(bytes.to_vec()), 0)
    }

    #[test]
    fn replies_of_every_kind_are_read() {
        assert_eq!(parse(b"+OK\r\n").unwrap(), Value::Simple("OK".into()));
        assert_eq!(
            parse(b"-ERR boom\r\n").unwrap(),
            Value::Error("ERR boom".into())
        );
        assert_eq!(parse(b":42\r\n").unwrap(), Value::Int(42));
        assert_eq!(
            parse(b"$5\r\nhello\r\n").unwrap(),
            Value::Bulk(Some(b"hello".to_vec()))
        );
        assert_eq!(parse(b"$-1\r\n").unwrap(), Value::Bulk(None));
        assert_eq!(parse(b"*-1\r\n").unwrap(), Value::Array(None));
        assert_eq!(
            parse(b"*2\r\n$4\r\nsave\r\n:3\r\n").unwrap(),
            Value::Array(Some(vec![
                Value::Bulk(Some(b"save".to_vec())),
                Value::Int(3)
            ]))
        );
        // A bulk string may contain line breaks.
        assert_eq!(
            parse(b"$7\r\na\r\nb\r\nc\r\n").unwrap().text().unwrap(),
            "a\r\nb\r\nc"
        );
    }

    #[test]
    fn malformed_and_truncated_replies_are_errors() {
        for bad in [
            &b""[..],
            b"+OK",
            b"$5\r\nhel",
            b"$5\r\nhelloXX",
            b"?what\r\n",
            b":x\r\n",
            b"$99999999999\r\n",
            b"*1\r\n",
        ] {
            assert!(parse(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        // Nesting is bounded.
        let deep = "*1\r\n".repeat(20);
        assert!(parse(deep.as_bytes()).is_err());
    }

    #[test]
    fn info_replies_become_maps() {
        let info = parse_info(
            "# Persistence\r\nloading:0\r\nrdb_saves:3\r\n\r\n# Server\r\nrun_id:abc\r\n",
        );
        assert_eq!(info["rdb_saves"], "3");
        assert_eq!(info["run_id"], "abc");
        assert!(!info.contains_key("# Persistence"));
    }

    #[test]
    fn an_error_reply_is_an_error_and_never_echoes_arguments() {
        let error = Value::Error("WRONGPASS invalid username-password pair".into())
            .ok()
            .unwrap_err()
            .to_string();
        assert!(error.contains("WRONGPASS"));
        assert!(Value::Simple("OK".into()).ok().is_ok());
    }
}
