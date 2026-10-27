# chicken-kv

A Redis-compatible key-value server written in Rust, where parts of the
database logic are written in [Chicken](https://esolangs.org/wiki/Chicken), an
esoteric language whose only keyword is `chicken`.

It speaks the real Redis protocol (RESP2 and RESP3), so standard clients such
as `redis-cli`, `redis-benchmark` and `redis-py` work with it unchanged.

## What's written in Chicken

| Feature | Chicken source | What it does |
| --- | --- | --- |
| `KEYS pattern` | [`chicken/glob.chasm`](chicken/glob.chasm) | Glob matching with `*` and `?` (a backtracking wildcard matcher) |
| `INCR`, `DECR`, `INCRBY`, `DECRBY` | [`chicken/add.chasm`](chicken/add.chasm) | Parses two signed decimal numbers character by character and adds them |

Chicken programs are one instruction per line, where the instruction is the
number of times `chicken` appears on that line. Writing that by hand doesn't
scale, so this repo includes **chasm** ([`tools/chasm.py`](tools/chasm.py)), a
small assembler that turns readable mnemonics, labels and named variables into
`.ch` files:

```
get k          ; push variable k
loadin         ; push the k-th character of the input
pushchar 1     ; push the separator character
eq
jnz found_sep  ; jump if they're equal
```

The assembled `.ch` files are compiled into the binary with `include_str!`.
The Rust side ([`src/chicken.rs`](src/chicken.rs)) runs them on the
[`chicken_esolang`](https://crates.io/crates/chicken_esolang) VM and turns VM
panics into ordinary errors. The tests check the Chicken glob matcher against
a Rust reference implementation on every pattern/key pair up to length 4
(about 10,000 cases), and the adder at the `i64` limits.

## Features

- **Protocol:** RESP arrays from real clients, plus inline commands for
  `nc`/`telnet`; `HELLO 2|3` to switch protocol versions; pipelining (replies
  to a batch of commands are written back together).
- **Commands:** `GET`, `SET` (with `EX`, `PX`, `NX`, `XX`, `KEEPTTL`), `DEL`,
  `EXISTS`, `INCR`, `DECR`, `INCRBY`, `DECRBY`, `EXPIRE`, `PEXPIRE`, `TTL`,
  `PTTL`, `PERSIST`, `KEYS`, `DBSIZE`, `FLUSHDB`/`FLUSHALL`, `MULTI`/`EXEC`/
  `DISCARD`, `SAVE`, `SHUTDOWN`, `PING`, `ECHO`, `QUIT`.
- **Expiry:** keys are expired lazily on access and swept every second in the
  background.
- **Persistence:** snapshots to `dump.ckv` every 60 seconds when something has
  changed, on `SAVE` and on `SHUTDOWN`. Snapshots are written to a temp file
  and renamed, so a crash never leaves a half-written file. They're loaded on
  startup.
- **Concurrency:** one thread per client, sharing the keyspace behind a mutex.
  `MULTI`/`EXEC` holds the lock for the whole transaction, so it's atomic.

## Running it

Start the server on the default address, 127.0.0.1:6379:

```sh
cargo run --release
```

Or pick the port, snapshot file and save interval (see `--help`):

```sh
cargo run --release -- --port 7000 --snapshot /tmp/dump.ckv --save-interval 10
```

Then, from another terminal, talk to it with `redis-cli`. It comes with Redis;
on macOS, install it with `brew install redis`. Don't start the Redis server
that Homebrew also installs (`brew services start redis`), because it would
take port 6379. If you started chicken-kv on another port, add `-p <port>`,
e.g. `redis-cli -p 7000 PING`.

```console
$ redis-cli SET user:1 ada
OK
$ redis-cli GET user:1
"ada"
$ redis-cli INCRBY visits 41
(integer) 41
$ redis-cli GET visits
"41"
$ redis-cli KEYS 'user:*'
1) "user:1"
```

No `redis-cli`? Inline commands work too: `printf 'PING\r\n' | nc localhost 6379`.

You can also run a standalone Chicken program:

```sh
cargo run --release -- run chicken/demo.ch 72
```

## Development

Run the unit tests and an end-to-end TCP test:

```sh
cargo test
```

After editing a `.chasm` file, reassemble it:

```sh
python3 tools/chasm.py chicken/glob.chasm
```

| Path | |
| --- | --- |
| `src/resp.rs` | RESP parser and encoder |
| `src/commands.rs` | Command implementations |
| `src/store.rs` | Keyspace, expiry and snapshot format |
| `src/server.rs` | TCP server and background snapshot/expiry thread |
| `src/chicken.rs` | Runs the Chicken programs |
| `chicken/` | Chicken programs: `.chasm` source and assembled `.ch` |
| `tools/chasm.py` | The Chicken assembler |
| `tools/compiler.py`, `tools/decompiler.py` | Convert between opcode lists and `.ch` files |

## Performance

On an Apple M4, release build, with `redis-py` sending 50,000 pipelined commands:

| | ops/sec |
| --- | --- |
| `SET` | ~245,000 |
| `GET` | ~380,000 |
| `INCR` (runs Chicken) | ~31,000 |

`KEYS` costs about 34 µs per key because each key runs through the Chicken
VM. Like in Redis, `KEYS` blocks the server while it runs, so it's for
debugging, not production.

## Limitations

- Only string values; no lists, hashes or sets yet.
- Values are stored as UTF-8 text, so invalid UTF-8 bytes are replaced.
- No `SCAN`, authentication, replication or append-only file.
- `KEYS` patterns support `*` and `?` but not `[abc]` classes or `\` escapes.
  Keys containing the byte `0x01` (used as a separator when calling Chicken)
  never match.
