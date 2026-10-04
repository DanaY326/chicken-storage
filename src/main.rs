mod chicken;
mod resp;
mod store;
mod commands;
use chicken_esolang::Chicken;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

fn decode_chicken_output(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '&' && chars.peek() == Some(&'#') {
            chars.next();
            let mut num = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    num.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            if chars.peek() == Some(&';') {
                chars.next();
                if let Ok(code) = num.parse::<u32>() {
                    if let Some(ch) = char::from_u32(code) {
                        result.push(ch);
                        continue;
                    }
                }
            }
            result.push_str(&format!("&#{};", num));
        } else {
            result.push(c);
        }
    }
    result
}

fn run_chicken_script(path: &str, input: &str) -> Result<String, String> {
    let code = fs::read_to_string(path).map_err(|e| format!("failed to read '{}': {}", path, e))?;

    let debug = false;
    let backwards_compatible = true;
    let result = std::panic::catch_unwind(|| {
        let mut chicken = Chicken::new(code, input, debug, backwards_compatible);
        chicken.run()
    });

    match result {
        Ok(output) => Ok(decode_chicken_output(&output)),
        Err(_) => Err("chicken script panicked".to_string()),
    }
}

fn handle_command(command: &str, store: &Mutex<HashMap<String, String>>, script_path: &str) -> String {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return "+OK\r\n".to_string();
    }

    let mut parts = trimmed.split_whitespace();
    let verb = parts.next().unwrap_or_default().to_ascii_uppercase();

    match verb.as_str() {
        "PING" => "+PONG\r\n".to_string(),
        "ECHO" => {
            let payload = parts.collect::<Vec<_>>().join(" ");
            format!("${}\r\n{}\r\n", payload.len(), payload)
        }
        "SET" => {
            let key = parts.next().unwrap_or_default().to_string();
            let value = parts.collect::<Vec<_>>().join(" ");
            if key.is_empty() {
                "-ERR SET requires a key and value\r\n".to_string()
            } else {
                store.lock().unwrap_or_else(|e| e.into_inner()).insert(key, value);
                "+OK\r\n".to_string()
            }
        }
        "GET" => {
            let key = parts.next().unwrap_or_default().to_string();
            let store_lock = store.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(value) = store_lock.get(&key) {
                format!("${}\r\n{}\r\n", value.len(), value)
            } else {
                "$-1\r\n".to_string()
            }
        }
        "DEL" => {
            let key = parts.next().unwrap_or_default().to_string();
            let removed = store.lock().unwrap_or_else(|e| e.into_inner()).remove(&key).is_some();
            format!(":{}\r\n", if removed { 1 } else { 0 })
        }
        "CHICKEN" => {
            let payload = parts.collect::<Vec<_>>().join(" ");
            match run_chicken_script(script_path, &payload) {
                Ok(output) => format!("+{}\r\n", output),
                Err(e) => format!("-ERR {}\r\n", e),
            }
        }
        _ => "-ERR unknown command\r\n".to_string(),
    }
}

fn handle_connection(mut stream: TcpStream, script_path: String, store: Arc<Mutex<HashMap<String, String>>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let response = handle_command(&line, &store, &script_path);
                if stream.write_all(response.as_bytes()).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

fn run_server(port: u16, script_path: &str) {
    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .unwrap_or_else(|e| panic!("Failed to bind to port {port}: {e}"));

    println!("Listening on 127.0.0.1:{port}");

    let store = Arc::new(Mutex::new(HashMap::new()));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let script_path = script_path.to_string();
                let store = Arc::clone(&store);
                thread::spawn(move || handle_connection(stream, script_path, store));
            }
            Err(e) => eprintln!("Connection failed: {e}"),
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let server_mode = args.iter().any(|arg| arg == "--server");
    let port = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--port="))
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(6379);

    if server_mode {
        run_server(port, "main.ch");
    } else {
        let filename = args.get(1).map(|s| s.as_str()).unwrap_or("main.ch");
        let input = if args.len() >= 3 { &args[2] } else { "" };
        match run_chicken_script(filename, input) {
            Ok(output) => println!("{}", output),
            Err(e) => eprintln!("{}", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_returns_pong() {
        let store = Mutex::new(HashMap::new());
        let response = handle_command("PING\n", &store, "main.ch");
        assert_eq!(response, "+PONG\r\n");
    }

    #[test]
    fn set_and_get_round_trip() {
        let store = Mutex::new(HashMap::new());
        let ok = handle_command("SET demo value\n", &store, "main.ch");
        assert_eq!(ok, "+OK\r\n");

        let fetched = handle_command("GET demo\n", &store, "main.ch");
        assert_eq!(fetched, "$5\r\nvalue\r\n");
    }

    #[test]
    fn del_removes_key() {
        let store = Mutex::new(HashMap::new());
        handle_command("SET demo value\n", &store, "main.ch");

        let removed = handle_command("DEL demo\n", &store, "main.ch");
        assert_eq!(removed, ":1\r\n");

        let missing = handle_command("GET demo\n", &store, "main.ch");
        assert_eq!(missing, "$-1\r\n");

        let removed_again = handle_command("DEL demo\n", &store, "main.ch");
        assert_eq!(removed_again, ":0\r\n");
    }

    #[test]
    fn chicken_error_does_not_poison_store() {
        let store = Mutex::new(HashMap::new());
        let response = handle_command("CHICKEN hi\n", &store, "/no/such/script.ch");
        assert!(response.starts_with("-ERR"));

        // The store must still be usable after a failed script run.
        let ok = handle_command("SET demo value\n", &store, "main.ch");
        assert_eq!(ok, "+OK\r\n");
    }
}
