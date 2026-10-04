//! Command dispatch: turns a parsed command into a reply.

use crate::chicken;
use crate::resp::Reply;
use crate::store::Store;

/// What the connection should do after sending the reply.
#[derive(Debug, PartialEq, Eq)]
pub enum After {
    Continue,
    Close,
    /// Save and shut the server down.
    Shutdown,
    /// Write a snapshot now (the caller owns the snapshot path).
    Save,
}

/// Per-connection state.
#[derive(Debug, Default)]
pub struct Session {
    /// Set by `HELLO 3`: reply using RESP3 instead of RESP2.
    pub resp3: bool,
    /// Commands queued since MULTI, if a transaction is open.
    pub queued: Option<Vec<Vec<String>>>,
}

fn wrong_args(name: &str) -> Reply {
    Reply::err(format!("ERR wrong number of arguments for '{}' command", name.to_lowercase()))
}

fn not_integer() -> Reply {
    Reply::err("ERR value is not an integer or out of range")
}

/// Parses an integer the way Redis does: no leading '+', spaces or zeros.
fn parse_int(s: &str) -> Option<i64> {
    let n = s.parse::<i64>().ok()?;
    (n.to_string() == s).then_some(n)
}

/// Runs one command against the store. `now` is the current Unix time in ms.
pub fn execute(store: &mut Store, session: &mut Session, args: &[String], now: u64) -> (Reply, After) {
    let Some(name) = args.first() else {
        return (Reply::ok(), After::Continue);
    };
    let name = name.to_ascii_uppercase();
    let a = &args[1..];

    if let Some(queue) = &mut session.queued
        && !matches!(name.as_str(), "MULTI" | "EXEC" | "DISCARD" | "QUIT") {
            queue.push(args.to_vec());
            return (Reply::Simple("QUEUED".into()), After::Continue);
        }

    let reply = match (name.as_str(), a.len()) {
        ("PING", 0) => Reply::Simple("PONG".into()),
        ("PING", 1) => Reply::Bulk(a[0].clone()),
        ("ECHO", 1) => Reply::Bulk(a[0].clone()),
        ("QUIT", _) => return (Reply::ok(), After::Close),
        ("SAVE", 0) => return (Reply::ok(), After::Save),
        ("SHUTDOWN", _) => return (Reply::ok(), After::Shutdown),

        ("SET", n) if n >= 2 => set(store, a, now),
        ("GET", 1) => match store.get(&a[0], now) {
            Some(e) => Reply::Bulk(e.value.clone()),
            None => Reply::Nil,
        },
        ("DEL", n) if n >= 1 => Reply::Int(a.iter().filter(|k| store.remove(k, now)).count() as i64),
        ("EXISTS", n) if n >= 1 => Reply::Int(a.iter().filter(|k| store.contains(k, now)).count() as i64),

        ("INCR", 1) => incr_by(store, &a[0], 1, now),
        ("DECR", 1) => incr_by(store, &a[0], -1, now),
        ("INCRBY", 2) => match parse_int(&a[1]) {
            Some(d) => incr_by(store, &a[0], d, now),
            None => not_integer(),
        },
        ("DECRBY", 2) => match parse_int(&a[1]).and_then(i64::checked_neg) {
            Some(d) => incr_by(store, &a[0], d, now),
            None => not_integer(),
        },

        ("EXPIRE", 2) => expire(store, a, 1000, now),
        ("PEXPIRE", 2) => expire(store, a, 1, now),
        ("TTL", 1) => ttl(store, &a[0], 1000, now),
        ("PTTL", 1) => ttl(store, &a[0], 1, now),
        ("PERSIST", 1) => {
            let had_ttl = store.get(&a[0], now).is_some_and(|e| e.expires_at.is_some());
            if had_ttl {
                store.set_expiry(&a[0], None, now);
            }
            Reply::Int(had_ttl as i64)
        }

        ("KEYS", 1) => keys(store, &a[0], now),
        ("DBSIZE", 0) => Reply::Int(store.len(now) as i64),
        ("FLUSHDB" | "FLUSHALL", _) => {
            store.clear();
            Reply::ok()
        }

        // Transactions. The caller holds the store lock for the whole
        // command, so EXEC runs every queued command with nothing in between.
        ("MULTI", 0) if session.queued.is_some() => Reply::err("ERR MULTI calls can not be nested"),
        ("MULTI", 0) => {
            session.queued = Some(Vec::new());
            Reply::ok()
        }
        ("DISCARD", 0) => match session.queued.take() {
            Some(_) => Reply::ok(),
            None => Reply::err("ERR DISCARD without MULTI"),
        },
        ("EXEC", 0) => match session.queued.take() {
            Some(queue) => return exec(store, session, queue, now),
            None => Reply::err("ERR EXEC without MULTI"),
        },

        // Connection setup that client libraries send automatically.
        ("HELLO", _) => hello(session, a),
        ("CLIENT", n) if n >= 1 => match a[0].to_ascii_uppercase().as_str() {
            "SETNAME" | "SETINFO" => Reply::ok(),
            "GETNAME" => Reply::Nil,
            _ => Reply::err(format!("ERR unknown subcommand '{}'", a[0])),
        },

        // redis-cli and redis-benchmark ask about these on startup; an empty
        // answer tells them there's nothing extra to know.
        ("COMMAND", _) => Reply::Array(Vec::new()),
        ("CONFIG", n) if n >= 1 && a[0].eq_ignore_ascii_case("GET") => Reply::Array(Vec::new()),
        ("SELECT", 1) if a[0] == "0" => Reply::ok(),
        ("SELECT", 1) => Reply::err("ERR DB index is out of range"),

        (
            "PING" | "ECHO" | "SAVE" | "SET" | "GET" | "DEL" | "EXISTS" | "INCR" | "DECR" | "INCRBY"
            | "DECRBY" | "EXPIRE" | "PEXPIRE" | "TTL" | "PTTL" | "PERSIST" | "KEYS" | "DBSIZE" | "CONFIG"
            | "SELECT" | "CLIENT" | "MULTI" | "EXEC" | "DISCARD",
            _,
        ) => wrong_args(&name),
        _ => {
            let shown: String = args[0].chars().take(64).collect();
            Reply::err(format!("ERR unknown command '{shown}'"))
        }
    };
    (reply, After::Continue)
}

/// Runs a transaction's queued commands. A SAVE or SHUTDOWN inside the
/// transaction takes effect once the whole transaction has run.
fn exec(store: &mut Store, session: &mut Session, queue: Vec<Vec<String>>, now: u64) -> (Reply, After) {
    let mut replies = Vec::with_capacity(queue.len());
    let mut after = After::Continue;
    for args in queue {
        let (reply, a) = execute(store, session, &args, now);
        replies.push(reply);
        match a {
            After::Shutdown => after = After::Shutdown,
            After::Save if after == After::Continue => after = After::Save,
            _ => {}
        }
    }
    (Reply::Array(replies), after)
}

/// HELLO [protover [AUTH user pass] [SETNAME name]]: picks RESP2 or RESP3
/// and describes the server.
fn hello(session: &mut Session, a: &[String]) -> Reply {
    let mut resp3 = session.resp3;
    if let Some(v) = a.first() {
        resp3 = match parse_int(v) {
            Some(2) => false,
            Some(3) => true,
            Some(_) => return Reply::err("NOPROTO unsupported protocol version"),
            None => return Reply::err("ERR Protocol version is not an integer or out of range"),
        };
    }
    let mut opts = a.iter().skip(1);
    while let Some(opt) = opts.next() {
        match opt.to_ascii_uppercase().as_str() {
            "SETNAME" if opts.next().is_some() => {}
            "AUTH" => return Reply::err("ERR AUTH is not supported: this server has no passwords"),
            _ => return Reply::err(format!("ERR Syntax error in HELLO option '{opt}'")),
        }
    }
    session.resp3 = resp3;

    let s = |v: &str| Reply::Bulk(v.to_string());
    Reply::Map(vec![
        (s("server"), s("chicken-kv")),
        (s("version"), s(env!("CARGO_PKG_VERSION"))),
        (s("proto"), Reply::Int(if resp3 { 3 } else { 2 })),
        (s("mode"), s("standalone")),
        (s("role"), s("master")),
        (s("modules"), Reply::Array(Vec::new())),
    ])
}

/// SET key value [NX | XX] [EX seconds | PX milliseconds | KEEPTTL]
fn set(store: &mut Store, a: &[String], now: u64) -> Reply {
    let (key, value) = (&a[0], &a[1]);
    let mut nx = false;
    let mut xx = false;
    let mut keep_ttl = false;
    let mut expires_at = None;

    let mut opts = a[2..].iter();
    while let Some(opt) = opts.next() {
        match opt.to_ascii_uppercase().as_str() {
            "NX" if !xx => nx = true,
            "XX" if !nx => xx = true,
            "KEEPTTL" if expires_at.is_none() => keep_ttl = true,
            unit @ ("EX" | "PX") if expires_at.is_none() && !keep_ttl => {
                let Some(n) = opts.next().and_then(|s| parse_int(s)) else {
                    return not_integer();
                };
                let scale = if unit == "EX" { 1000 } else { 1 };
                match n.checked_mul(scale).filter(|&ms| ms > 0) {
                    Some(ms) => expires_at = Some(now.saturating_add(ms as u64)),
                    None => return Reply::err("ERR invalid expire time in 'set' command"),
                }
            }
            _ => return Reply::err("ERR syntax error"),
        }
    }

    let exists = store.contains(key, now);
    if (nx && exists) || (xx && !exists) {
        return Reply::Nil;
    }
    if keep_ttl {
        store.set_keep_ttl(key.clone(), value.clone(), now);
    } else {
        store.set(key.clone(), value.clone(), expires_at);
    }
    Reply::ok()
}

/// INCR / DECR / INCRBY / DECRBY. The addition itself runs in Chicken.
fn incr_by(store: &mut Store, key: &str, delta: i64, now: u64) -> Reply {
    let current = match store.get(key, now) {
        None => 0,
        Some(e) => match parse_int(&e.value) {
            Some(n) => n,
            None => return not_integer(),
        },
    };
    if current.checked_add(delta).is_none() {
        return Reply::err("ERR increment or decrement would overflow");
    }
    match chicken::add(current, delta) {
        Ok(n) => {
            store.set_keep_ttl(key.to_string(), n.to_string(), now);
            Reply::Int(n)
        }
        Err(e) => Reply::err(format!("ERR {e}")),
    }
}

fn expire(store: &mut Store, a: &[String], scale: i64, now: u64) -> Reply {
    let Some(n) = parse_int(&a[1]) else {
        return not_integer();
    };
    let Some(ms) = n.checked_mul(scale) else {
        return Reply::err("ERR invalid expire time in 'expire' command");
    };
    if !store.contains(&a[0], now) {
        return Reply::Int(0);
    }
    if ms <= 0 {
        // A non-positive TTL deletes the key straight away, like Redis.
        store.remove(&a[0], now);
    } else {
        store.set_expiry(&a[0], Some(now.saturating_add(ms as u64)), now);
    }
    Reply::Int(1)
}

fn ttl(store: &mut Store, key: &str, scale: u64, now: u64) -> Reply {
    match store.get(key, now) {
        None => Reply::Int(-2),
        Some(e) => match e.expires_at {
            None => Reply::Int(-1),
            // Round up so a key with 1500 ms left reports TTL 2, like Redis.
            Some(t) => Reply::Int(((t - now).div_ceil(scale)) as i64),
        },
    }
}

/// KEYS pattern. The matching runs in Chicken, one key at a time.
fn keys(store: &mut Store, pattern: &str, now: u64) -> Reply {
    let mut matched = Vec::new();
    for key in store.live_keys(now) {
        match chicken::glob_match(pattern, &key) {
            Ok(true) => matched.push(key),
            Ok(false) => {}
            Err(e) => return Reply::err(format!("ERR {e}")),
        }
    }
    matched.sort();
    Reply::Array(matched.into_iter().map(Reply::Bulk).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Db {
        store: Store,
        now: u64,
    }

    impl Db {
        fn new() -> Db {
            Db { store: Store::default(), now: 1_000_000 }
        }

        fn run(&mut self, cmd: &str) -> Reply {
            let args: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
            execute(&mut self.store, &mut Session::default(), &args, self.now).0
        }
    }

    fn bulk(s: &str) -> Reply {
        Reply::Bulk(s.into())
    }

    #[test]
    fn ping_and_echo() {
        let mut db = Db::new();
        assert_eq!(db.run("PING"), Reply::Simple("PONG".into()));
        assert_eq!(db.run("ping hi"), bulk("hi"));
        assert_eq!(db.run("ECHO hello"), bulk("hello"));
    }

    #[test]
    fn set_get_del_exists() {
        let mut db = Db::new();
        assert_eq!(db.run("SET demo value"), Reply::ok());
        assert_eq!(db.run("GET demo"), bulk("value"));
        assert_eq!(db.run("EXISTS demo nope demo"), Reply::Int(2));
        assert_eq!(db.run("DEL demo nope"), Reply::Int(1));
        assert_eq!(db.run("GET demo"), Reply::Nil);
        assert_eq!(db.run("DEL demo"), Reply::Int(0));
    }

    #[test]
    fn set_nx_xx() {
        let mut db = Db::new();
        assert_eq!(db.run("SET k 1 XX"), Reply::Nil);
        assert_eq!(db.run("SET k 1 NX"), Reply::ok());
        assert_eq!(db.run("SET k 2 NX"), Reply::Nil);
        assert_eq!(db.run("SET k 3 XX"), Reply::ok());
        assert_eq!(db.run("GET k"), bulk("3"));
        assert_eq!(db.run("SET k 4 NX XX"), Reply::err("ERR syntax error"));
    }

    #[test]
    fn expiry() {
        let mut db = Db::new();
        db.run("SET k v EX 10");
        assert_eq!(db.run("TTL k"), Reply::Int(10));
        db.now += 1_500;
        assert_eq!(db.run("TTL k"), Reply::Int(9));
        assert_eq!(db.run("PTTL k"), Reply::Int(8_500));
        db.now += 8_500;
        assert_eq!(db.run("GET k"), Reply::Nil);
        assert_eq!(db.run("TTL k"), Reply::Int(-2));

        db.run("SET k v");
        assert_eq!(db.run("TTL k"), Reply::Int(-1));
        assert_eq!(db.run("EXPIRE k 5"), Reply::Int(1));
        assert_eq!(db.run("PERSIST k"), Reply::Int(1));
        assert_eq!(db.run("PERSIST k"), Reply::Int(0));
        assert_eq!(db.run("TTL k"), Reply::Int(-1));
        assert_eq!(db.run("EXPIRE k 0"), Reply::Int(1));
        assert_eq!(db.run("EXISTS k"), Reply::Int(0));
        assert_eq!(db.run("EXPIRE missing 5"), Reply::Int(0));
        assert_eq!(db.run("SET k v EX 0"), Reply::err("ERR invalid expire time in 'set' command"));
    }

    #[test]
    fn set_overwrites_ttl_unless_keepttl() {
        let mut db = Db::new();
        db.run("SET k v PX 5000");
        db.run("SET k v2 KEEPTTL");
        assert_eq!(db.run("PTTL k"), Reply::Int(5000));
        db.run("SET k v3");
        assert_eq!(db.run("TTL k"), Reply::Int(-1));
    }

    #[test]
    fn incr_family() {
        let mut db = Db::new();
        assert_eq!(db.run("INCR n"), Reply::Int(1));
        assert_eq!(db.run("INCRBY n 41"), Reply::Int(42));
        assert_eq!(db.run("DECR n"), Reply::Int(41));
        assert_eq!(db.run("DECRBY n 50"), Reply::Int(-9));
        assert_eq!(db.run("GET n"), bulk("-9"));

        db.run("SET s hello");
        assert_eq!(db.run("INCR s"), not_integer());
        db.run("SET s +5");
        assert_eq!(db.run("INCR s"), not_integer());
        assert_eq!(db.run("INCRBY n abc"), not_integer());

        db.run(&format!("SET big {}", i64::MAX));
        assert_eq!(db.run("INCR big"), Reply::err("ERR increment or decrement would overflow"));
        assert_eq!(db.run(&format!("DECRBY n {}", i64::MIN)), not_integer());
    }

    #[test]
    fn incr_keeps_ttl() {
        let mut db = Db::new();
        db.run("SET n 1 EX 100");
        db.run("INCR n");
        assert_eq!(db.run("TTL n"), Reply::Int(100));
    }

    #[test]
    fn keys_and_dbsize() {
        let mut db = Db::new();
        for k in ["user:1", "user:2", "user:10", "session:1"] {
            db.run(&format!("SET {k} x"));
        }
        db.run("SET old x PX 1");
        db.now += 10;
        assert_eq!(db.run("DBSIZE"), Reply::Int(4));
        assert_eq!(
            db.run("KEYS user:*"),
            Reply::Array(vec![bulk("user:1"), bulk("user:10"), bulk("user:2")])
        );
        assert_eq!(db.run("KEYS user:?"), Reply::Array(vec![bulk("user:1"), bulk("user:2")]));
        assert_eq!(db.run("KEYS *"), Reply::Array(vec![
            bulk("session:1"), bulk("user:1"), bulk("user:10"), bulk("user:2"),
        ]));
        assert_eq!(db.run("FLUSHALL"), Reply::ok());
        assert_eq!(db.run("DBSIZE"), Reply::Int(0));
    }

    #[test]
    fn errors() {
        let mut db = Db::new();
        assert_eq!(db.run("GET"), Reply::err("ERR wrong number of arguments for 'get' command"));
        assert_eq!(db.run("NOPE x"), Reply::err("ERR unknown command 'NOPE'"));
        assert_eq!(db.run("SET k v EX"), not_integer());
        assert_eq!(db.run("SET k v BOGUS"), Reply::err("ERR syntax error"));
    }

    #[test]
    fn connection_control() {
        let mut db = Db::new();
        let mut session = Session::default();
        let args = |s: &str| vec![s.to_string()];
        assert_eq!(execute(&mut db.store, &mut session, &args("QUIT"), 0).1, After::Close);
        assert_eq!(execute(&mut db.store, &mut session, &args("SAVE"), 0).1, After::Save);
        assert_eq!(execute(&mut db.store, &mut session, &args("SHUTDOWN"), 0).1, After::Shutdown);
    }

    #[test]
    fn transactions() {
        let mut store = Store::default();
        let mut session = Session::default();
        let mut run = |cmd: &str| {
            let args: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
            execute(&mut store, &mut session, &args, 0).0
        };
        assert_eq!(run("EXEC"), Reply::err("ERR EXEC without MULTI"));
        assert_eq!(run("MULTI"), Reply::ok());
        assert_eq!(run("MULTI"), Reply::err("ERR MULTI calls can not be nested"));
        assert_eq!(run("INCR n"), Reply::Simple("QUEUED".into()));
        assert_eq!(run("INCRBY n 9"), Reply::Simple("QUEUED".into()));
        assert_eq!(run("GET n"), Reply::Simple("QUEUED".into()));
        assert_eq!(run("GET n"), Reply::Simple("QUEUED".into()));
        assert_eq!(run("EXEC"), Reply::Array(vec![Reply::Int(1), Reply::Int(10), Reply::Bulk("10".into()), Reply::Bulk("10".into())]));
        assert_eq!(run("MULTI"), Reply::ok());
        run("SET n 0");
        assert_eq!(run("DISCARD"), Reply::ok());
        assert_eq!(run("GET n"), Reply::Bulk("10".into()));
    }

    #[test]
    fn hello_switches_protocol() {
        let mut store = Store::default();
        let mut session = Session::default();
        let mut run = |cmd: &str, session: &mut Session| {
            let args: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
            execute(&mut store, session, &args, 0).0
        };

        assert_eq!(run("HELLO 4", &mut session), Reply::err("NOPROTO unsupported protocol version"));
        assert!(!session.resp3);
        let Reply::Map(info) = run("HELLO 3", &mut session) else { panic!("HELLO should return a map") };
        assert!(info.contains(&(Reply::Bulk("proto".into()), Reply::Int(3))));
        assert!(session.resp3);
        assert!(matches!(run("HELLO", &mut session), Reply::Map(_)));
        assert!(session.resp3, "HELLO with no version keeps the current protocol");
        assert!(matches!(run("HELLO 2 AUTH u p", &mut session), Reply::Error(_)));
        assert!(session.resp3, "a failed HELLO doesn't change the protocol");
        assert!(matches!(run("HELLO 2 SETNAME me", &mut session), Reply::Map(_)));
        assert!(!session.resp3);
        assert_eq!(run("CLIENT SETINFO LIB-NAME x", &mut session), Reply::ok());
    }
}
