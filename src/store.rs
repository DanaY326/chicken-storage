//! The in-memory keyspace: string values with optional expiry, plus
//! snapshotting to and from disk.
//!
//! Times are Unix milliseconds and are always passed in by the caller, which
//! keeps expiry logic deterministic and easy to test.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const SNAPSHOT_MAGIC: &str = "CHICKENKV1";

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub value: String,
    /// Unix time in ms after which the key no longer exists.
    pub expires_at: Option<u64>,
}

impl Entry {
    fn is_expired(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|t| t <= now)
    }
}

#[derive(Debug, Default)]
pub struct Store {
    map: HashMap<String, Entry>,
    /// Number of writes since the last snapshot.
    dirty: u64,
}

impl Store {
    /// Looks up a live key, deleting it first if it has expired.
    pub fn get(&mut self, key: &str, now: u64) -> Option<&Entry> {
        if self.map.get(key).is_some_and(|e| e.is_expired(now)) {
            self.map.remove(key);
            self.dirty += 1;
        }
        self.map.get(key)
    }

    pub fn contains(&mut self, key: &str, now: u64) -> bool {
        self.get(key, now).is_some()
    }

    pub fn set(&mut self, key: String, value: String, expires_at: Option<u64>) {
        self.map.insert(key, Entry { value, expires_at });
        self.dirty += 1;
    }

    /// Replaces the value of a key, keeping its TTL. The key must exist or the
    /// value is stored without expiry.
    pub fn set_keep_ttl(&mut self, key: String, value: String, now: u64) {
        let expires_at = self.get(&key, now).and_then(|e| e.expires_at);
        self.set(key, value, expires_at);
    }

    pub fn remove(&mut self, key: &str, now: u64) -> bool {
        let existed = self.contains(key, now);
        if existed {
            self.map.remove(key);
            self.dirty += 1;
        }
        existed
    }

    /// Sets or clears the expiry of an existing key. Returns false if the key
    /// doesn't exist.
    pub fn set_expiry(&mut self, key: &str, expires_at: Option<u64>, now: u64) -> bool {
        if !self.contains(key, now) {
            return false;
        }
        let entry = self.map.get_mut(key).expect("checked above");
        if entry.expires_at != expires_at {
            entry.expires_at = expires_at;
            self.dirty += 1;
        }
        true
    }

    pub fn live_keys(&self, now: u64) -> Vec<String> {
        self.map
            .iter()
            .filter(|(_, e)| !e.is_expired(now))
            .map(|(k, _)| k.clone())
            .collect()
    }

    pub fn len(&self, now: u64) -> usize {
        self.map.values().filter(|e| !e.is_expired(now)).count()
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.dirty += 1;
    }

    /// Drops every expired key. Returns how many were removed.
    pub fn purge_expired(&mut self, now: u64) -> usize {
        let before = self.map.len();
        self.map.retain(|_, e| !e.is_expired(now));
        let removed = before - self.map.len();
        if removed > 0 {
            self.dirty += 1;
        }
        removed
    }

    pub fn dirty(&self) -> u64 {
        self.dirty
    }

    pub fn mark_dirty(&mut self) {
        self.dirty += 1;
    }

    /// Copies out the live data so it can be written without holding a lock.
    pub fn snapshot(&mut self, now: u64) -> Vec<(String, Entry)> {
        self.dirty = 0;
        self.map
            .iter()
            .filter(|(_, e)| !e.is_expired(now))
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect()
    }

    pub fn from_entries(entries: Vec<(String, Entry)>, now: u64) -> Store {
        let map = entries.into_iter().filter(|(_, e)| !e.is_expired(now)).collect();
        Store { map, dirty: 0 }
    }
}

/// Writes a snapshot atomically: to a temp file first, then renamed over the
/// old one, so a crash mid-write never leaves a half-written snapshot.
///
/// Format: a magic line, then for each key a header line
/// `<key bytes> <value bytes> <expiry ms or -1>` followed by the raw key,
/// the raw value and a newline. Lengths make it safe for any content.
pub fn write_snapshot(path: &Path, entries: &[(String, Entry)]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut w = BufWriter::new(fs::File::create(&tmp)?);
        writeln!(w, "{SNAPSHOT_MAGIC}")?;
        for (key, entry) in entries {
            let expiry = entry.expires_at.map_or(-1, |t| t as i64);
            writeln!(w, "{} {} {}", key.len(), entry.value.len(), expiry)?;
            w.write_all(key.as_bytes())?;
            w.write_all(entry.value.as_bytes())?;
            w.write_all(b"\n")?;
        }
        w.flush()?;
        w.get_ref().sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// Reads a snapshot. A missing file is not an error: it yields no entries.
pub fn read_snapshot(path: &Path) -> io::Result<Vec<(String, Entry)>> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let bad = |msg: &str| io::Error::new(io::ErrorKind::InvalidData, format!("corrupt snapshot: {msg}"));
    let mut r = BufReader::new(file);

    let mut line = String::new();
    r.read_line(&mut line)?;
    if line.trim_end() != SNAPSHOT_MAGIC {
        return Err(bad("unrecognised header"));
    }

    let mut entries = Vec::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Ok(entries);
        }
        let mut parts = line.split_whitespace();
        let mut next_num = |what: &str| -> io::Result<i64> {
            parts.next().and_then(|p| p.parse().ok()).ok_or_else(|| bad(what))
        };
        let key_len = next_num("key length")?;
        let val_len = next_num("value length")?;
        let expiry = next_num("expiry")?;
        if key_len < 0 || val_len < 0 {
            return Err(bad("negative length"));
        }

        let mut buf = vec![0u8; key_len as usize + val_len as usize + 1];
        r.read_exact(&mut buf)?;
        if buf.pop() != Some(b'\n') {
            return Err(bad("missing record terminator"));
        }
        let value = buf.split_off(key_len as usize);
        let key = String::from_utf8(buf).map_err(|_| bad("key is not UTF-8"))?;
        let value = String::from_utf8(value).map_err(|_| bad("value is not UTF-8"))?;
        let expires_at = (expiry >= 0).then_some(expiry as u64);
        entries.push((key, Entry { value, expires_at }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_keys_disappear() {
        let mut s = Store::default();
        s.set("a".into(), "1".into(), Some(1_000));
        s.set("b".into(), "2".into(), None);
        assert!(s.contains("a", 999));
        assert!(!s.contains("a", 1_000));
        assert_eq!(s.len(1_000), 1);
        assert_eq!(s.live_keys(1_000), vec!["b".to_string()]);
    }

    #[test]
    fn purge_removes_only_expired() {
        let mut s = Store::default();
        s.set("a".into(), "1".into(), Some(10));
        s.set("b".into(), "2".into(), Some(20));
        s.set("c".into(), "3".into(), None);
        assert_eq!(s.purge_expired(15), 1);
        assert_eq!(s.len(15), 2);
    }

    #[test]
    fn set_keep_ttl_preserves_expiry() {
        let mut s = Store::default();
        s.set("a".into(), "1".into(), Some(100));
        s.set_keep_ttl("a".into(), "2".into(), 50);
        assert_eq!(s.get("a", 50).unwrap().expires_at, Some(100));
    }

    #[test]
    fn snapshot_round_trip() {
        let dir = std::env::temp_dir().join(format!("chicken-kv-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dump.ckv");

        let mut s = Store::default();
        s.set("plain".into(), "value".into(), None);
        s.set("tricky key".into(), "line1\nline2 \r\n 3 4 5".into(), Some(5_000));
        s.set("".into(), "".into(), None);
        s.set("gone".into(), "x".into(), Some(10));
        assert!(s.dirty() > 0);

        write_snapshot(&path, &s.snapshot(100)).unwrap();
        assert_eq!(s.dirty(), 0);

        let mut loaded = Store::from_entries(read_snapshot(&path).unwrap(), 100);
        assert_eq!(loaded.len(100), 3);
        assert_eq!(loaded.get("tricky key", 100).unwrap().value, "line1\nline2 \r\n 3 4 5");
        assert_eq!(loaded.get("tricky key", 100).unwrap().expires_at, Some(5_000));
        assert_eq!(loaded.get("", 100).unwrap().value, "");
        assert!(loaded.get("gone", 100).is_none());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_snapshot_is_empty() {
        let entries = read_snapshot(Path::new("/definitely/not/here.ckv")).unwrap();
        assert!(entries.is_empty());
    }
}
