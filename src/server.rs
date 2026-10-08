//! TCP server: one thread per client, a shared store behind a mutex, and a
//! background thread that expires keys and writes snapshots.

use crate::commands::{self, After, Session};
use crate::resp::{self, Reply};
use crate::store::{self, Store};
use std::io::{self, BufReader, BufWriter, ErrorKind, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

pub struct Config {
    pub bind: String,
    pub port: u16,
    pub snapshot_path: PathBuf,
    /// Seconds between automatic snapshots (when anything changed); 0 turns
    /// them off.
    pub save_interval: u64,
}

struct Shared {
    store: Mutex<Store>,
    /// Stops two saves (e.g. SAVE racing the background save) writing the
    /// same temp file at once.
    save_lock: Mutex<()>,
    config: Config,
}

impl Shared {
    /// A panic in one client thread must not take the whole store down with
    /// it, so a poisoned lock is recovered rather than propagated.
    fn lock(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self) -> io::Result<usize> {
        let _guard = self.save_lock.lock().unwrap_or_else(|e| e.into_inner());
        let entries = self.lock().snapshot(store::now_ms());
        if let Err(e) = store::write_snapshot(&self.config.snapshot_path, &entries) {
            // The data never made it to disk, so it still needs saving.
            self.lock().mark_dirty();
            return Err(e);
        }
        Ok(entries.len())
    }
}

pub fn run(config: Config) -> io::Result<()> {
    let entries = store::read_snapshot(&config.snapshot_path)?;
    let store = Store::from_entries(entries, store::now_ms());
    println!("Loaded {} keys from {}", store.len(store::now_ms()), config.snapshot_path.display());

    let listener = TcpListener::bind((config.bind.as_str(), config.port))?;
    println!("Listening on {}", listener.local_addr()?);

    let shared = Arc::new(Shared { store: Mutex::new(store), save_lock: Mutex::new(()), config });

    {
        let shared = Arc::clone(&shared);
        thread::spawn(move || housekeeping(shared));
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let shared = Arc::clone(&shared);
                thread::spawn(move || {
                    if let Err(e) = handle_client(stream, &shared)
                        && !matches!(e.kind(), ErrorKind::ConnectionReset | ErrorKind::BrokenPipe) {
                            eprintln!("client error: {e}");
                        }
                });
            }
            Err(e) => eprintln!("connection failed: {e}"),
        }
    }
    Ok(())
}

/// Every second: drop expired keys, and snapshot if it's been long enough and
/// something has changed.
fn housekeeping(shared: Arc<Shared>) {
    let interval = Duration::from_secs(shared.config.save_interval);
    let mut last_save = Instant::now();
    loop {
        thread::sleep(Duration::from_secs(1));
        let dirty = {
            let mut store = shared.lock();
            store.purge_expired(store::now_ms());
            store.dirty()
        };
        if !interval.is_zero() && dirty > 0 && last_save.elapsed() >= interval {
            match shared.save() {
                Ok(n) => println!("Saved {n} keys ({dirty} changes since last save)"),
                Err(e) => eprintln!("background save failed: {e}"),
            }
            last_save = Instant::now();
        }
    }
}

fn handle_client(stream: TcpStream, shared: &Shared) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    let mut out = Vec::new();
    let mut session = Session::default();

    loop {
        let args = match resp::read_command(&mut reader) {
            Ok(Some(args)) => args,
            Ok(None) => return Ok(()),
            Err(e) if e.kind() == ErrorKind::InvalidData => {
                // Like Redis: report the protocol error, then hang up.
                writer.write_all(&Reply::err(format!("ERR {e}")).to_bytes())?;
                return writer.flush();
            }
            Err(e) => return Err(e),
        };
        if args.is_empty() {
            continue;
        }

        let (mut reply, after) = {
            let mut store = shared.lock();
            commands::execute(&mut store, &mut session, &args, store::now_ms())
        };

        if (after == After::Save || after == After::Shutdown)
            && let Err(e) = shared.save() {
                reply = Reply::err(format!("ERR snapshot failed: {e}"));
            }

        out.clear();
        reply.encode(&mut out, session.resp3);
        writer.write_all(&out)?;

        match after {
            After::Close => return writer.flush(),
            After::Shutdown if matches!(reply, Reply::Simple(_)) => {
                writer.flush()?;
                println!("Shutting down after SHUTDOWN command");
                std::process::exit(0);
            }
            _ => {}
        }

        // Only flush once every pipelined command already sent has been
        // answered, so a batch of commands gets written back in one go.
        if reader.buffer().is_empty() {
            writer.flush()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read};

    /// Starts a real server on a random port and talks RESP to it.
    #[test]
    fn end_to_end_over_tcp() {
        let dir = std::env::temp_dir().join(format!("chicken-kv-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(Shared {
            store: Mutex::new(Store::default()),
            save_lock: Mutex::new(()),
            config: Config {
                bind: "127.0.0.1".into(),
                port: addr.port(),
                snapshot_path: dir.join("dump.ckv"),
                save_interval: 0,
            },
        });
        {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let shared = Arc::clone(&shared);
                    thread::spawn(move || handle_client(stream.unwrap(), &shared));
                }
            });
        }

        let mut conn = TcpStream::connect(addr).unwrap();
        // Pipelined: RESP and inline commands in a single write.
        conn.write_all(
            b"*3\r\n$3\r\nSET\r\n$5\r\ncount\r\n$2\r\n41\r\n\
              INCR count\r\n\
              *2\r\n$4\r\nKEYS\r\n$2\r\nc*\r\n\
              SAVE\r\n\
              QUIT\r\n",
        )
        .unwrap();
        let mut response = String::new();
        conn.read_to_string(&mut response).unwrap();
        assert_eq!(response, "+OK\r\n:42\r\n*1\r\n$5\r\ncount\r\n+OK\r\n+OK\r\n");

        let saved = store::read_snapshot(&dir.join("dump.ckv")).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].1.value, "42");

        // A protocol error gets an error reply and the connection is closed.
        let mut conn = TcpStream::connect(addr).unwrap();
        conn.write_all(b"*1\r\n:oops\r\n").unwrap();
        let mut reader = BufReader::new(conn);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.starts_with("-ERR Protocol error"), "{line}");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
