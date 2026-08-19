//! RESP (REdis Serialization Protocol) parsing and encoding.
//!
//! Clients such as `redis-cli` send each command as an array of bulk strings:
//! `*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n`. For convenience (e.g. `nc`/`telnet`),
//! plain "inline" commands like `GET foo\r\n` are accepted too.

use std::io::{self, BufRead, Read};

/// Largest bulk string we accept, matching Redis's default `proto-max-bulk-len`.
const MAX_BULK_LEN: usize = 512 * 1024 * 1024;
/// Largest number of arguments in one command.
const MAX_ARGS: usize = 1024 * 1024;
/// Largest inline command / header line.
const MAX_LINE: usize = 64 * 1024;

/// A reply sent back to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(String),
    Nil,
    Array(Vec<Reply>),
    /// Key/value pairs. RESP3 has a real map type; RESP2 gets a flat array.
    Map(Vec<(Reply, Reply)>),
}

impl Reply {
    pub fn ok() -> Reply {
        Reply::Simple("OK".into())
    }

    pub fn err(msg: impl Into<String>) -> Reply {
        Reply::Error(msg.into())
    }

    /// Encodes the reply in RESP2, or RESP3 for clients that asked for it
    /// with `HELLO 3`. The two only differ in how null and maps look.
    pub fn encode(&self, out: &mut Vec<u8>, resp3: bool) {
        match self {
            Reply::Simple(s) => {
                out.push(b'+');
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Reply::Error(s) => {
                out.push(b'-');
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Reply::Int(n) => out.extend_from_slice(format!(":{n}\r\n").as_bytes()),
            Reply::Bulk(s) => {
                out.extend_from_slice(format!("${}\r\n", s.len()).as_bytes());
                out.extend_from_slice(s.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            Reply::Nil if resp3 => out.extend_from_slice(b"_\r\n"),
            Reply::Nil => out.extend_from_slice(b"$-1\r\n"),
            Reply::Array(items) => {
                out.extend_from_slice(format!("*{}\r\n", items.len()).as_bytes());
                for item in items {
                    item.encode(out, resp3);
                }
            }
            Reply::Map(pairs) => {
                let header = if resp3 {
                    format!("%{}\r\n", pairs.len())
                } else {
                    format!("*{}\r\n", pairs.len() * 2)
                };
                out.extend_from_slice(header.as_bytes());
                for (k, v) in pairs {
                    k.encode(out, resp3);
                    v.encode(out, resp3);
                }
            }
        }
    }

    /// Encodes as RESP2.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out, false);
        out
    }
}

fn protocol_error(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Reads one line up to and including `\n`. Returns `None` at a clean EOF.
fn read_line<R: BufRead>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let n = r.by_ref().take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n')
        && line.len() > MAX_LINE {
            return Err(protocol_error("Protocol error: too big inline request"));
        }
        // EOF in the middle of a line: treat what we have as the last line.
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    Ok(Some(line))
}

fn parse_len(bytes: &[u8], what: &str) -> io::Result<i64> {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| protocol_error(format!("Protocol error: invalid {what}")))
}

/// Reads the next command. Returns `Ok(None)` when the client disconnects, and
/// `Ok(Some(vec![]))` for an empty line (which gets no reply).
pub fn read_command<R: BufRead>(r: &mut R) -> io::Result<Option<Vec<String>>> {
    let Some(line) = read_line(r)? else {
        return Ok(None);
    };

    if line.first() != Some(&b'*') {
        // Inline command.
        let text = String::from_utf8_lossy(&line);
        return Ok(Some(text.split_whitespace().map(str::to_string).collect()));
    }

    let count = parse_len(&line[1..], "multibulk length")?;
    if count <= 0 {
        return Ok(Some(Vec::new()));
    }
    if count as usize > MAX_ARGS {
        return Err(protocol_error("Protocol error: invalid multibulk length"));
    }

    let mut args = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let header = read_line(r)?.ok_or_else(|| protocol_error("Protocol error: unexpected EOF"))?;
        if header.first() != Some(&b'$') {
            let got = header.first().map(|&b| b as char).unwrap_or(' ');
            return Err(protocol_error(format!("Protocol error: expected '$', got '{got}'")));
        }
        let len = parse_len(&header[1..], "bulk length")?;
        if len < 0 || len as usize > MAX_BULK_LEN {
            return Err(protocol_error("Protocol error: invalid bulk length"));
        }
        let mut buf = vec![0u8; len as usize + 2];
        r.read_exact(&mut buf)?;
        if &buf[len as usize..] != b"\r\n" {
            return Err(protocol_error("Protocol error: bulk string not terminated by CRLF"));
        }
        buf.truncate(len as usize);
        args.push(String::from_utf8_lossy(&buf).into_owned());
    }
    Ok(Some(args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(input: &str) -> Vec<Option<Vec<String>>> {
        let mut r = Cursor::new(input.as_bytes().to_vec());
        let mut out = Vec::new();
        loop {
            let cmd = read_command(&mut r).unwrap();
            let done = cmd.is_none();
            out.push(cmd);
            if done {
                return out;
            }
        }
    }

    fn strs(v: &[&str]) -> Option<Vec<String>> {
        Some(v.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn parses_resp_arrays() {
        let cmds = parse("*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$11\r\nhello world\r\n");
        assert_eq!(cmds, vec![strs(&["SET", "key", "hello world"]), None]);
    }

    #[test]
    fn bulk_strings_can_contain_crlf() {
        let cmds = parse("*2\r\n$4\r\nECHO\r\n$4\r\na\r\nb\r\n");
        assert_eq!(cmds[0], strs(&["ECHO", "a\r\nb"]));
    }

    #[test]
    fn parses_inline_and_pipelined_commands() {
        let cmds = parse("PING\r\nGET  foo\n*1\r\n$4\r\nPING\r\n");
        assert_eq!(cmds, vec![strs(&["PING"]), strs(&["GET", "foo"]), strs(&["PING"]), None]);
    }

    #[test]
    fn rejects_bad_bulk_header() {
        let mut r = Cursor::new(b"*1\r\n:3\r\n".to_vec());
        assert!(read_command(&mut r).is_err());
    }

    #[test]
    fn encodes_replies() {
        let reply = Reply::Array(vec![
            Reply::ok(),
            Reply::Int(-3),
            Reply::Bulk("hé".into()),
            Reply::Nil,
            Reply::err("ERR nope"),
        ]);
        assert_eq!(
            reply.to_bytes(),
            b"*5\r\n+OK\r\n:-3\r\n$3\r\nh\xc3\xa9\r\n$-1\r\n-ERR nope\r\n".to_vec()
        );
    }

    #[test]
    fn encodes_resp3_differences() {
        let reply = Reply::Map(vec![(Reply::Bulk("proto".into()), Reply::Int(3)), (Reply::Bulk("x".into()), Reply::Nil)]);
        let mut out = Vec::new();
        reply.encode(&mut out, true);
        assert_eq!(out, b"%2\r\n$5\r\nproto\r\n:3\r\n$1\r\nx\r\n_\r\n".to_vec());
        assert_eq!(reply.to_bytes(), b"*4\r\n$5\r\nproto\r\n:3\r\n$1\r\nx\r\n$-1\r\n".to_vec());
    }
}
