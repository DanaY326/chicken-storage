use chicken_esolang::Chicken;
use std::env;
use std::fs;

fn decode_chicken_output(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '&' && chars.peek() == Some(&'#') {
            chars.next(); // skip '#'
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
                chars.next(); // skip ';'
                if let Ok(code) = num.parse::<u32>() {
                    if let Some(ch) = char::from_u32(code) {
                        result.push(ch);
                        continue;
                    }
                }
            }
            // if parsing failed, just keep the original
            result.push_str(&format!("&#{};", num));
        } else {
            result.push(c);
        }
    }
    result
}

fn main() {
    let args: Vec<String> = env::args().collect();

    // Use the first argument as the filename, or default to "hello.chn"
    let filename = args.get(1).map(|s| s.as_str()).unwrap_or("main.ch");

    let code = fs::read_to_string(filename)
        .unwrap_or_else(|e| panic!("Failed to read '{}': {}", filename, e));

    let input = if args.len() >= 3 { &args[2] } else { "" };
    let debug = false;
    let backwards_compatible = true;

    let mut chicken = Chicken::new(code, &input, debug, backwards_compatible);
    let output = chicken.run();
    println!("{}", decode_chicken_output(&output));
}