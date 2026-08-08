use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Parser;
use rustyline::DefaultEditor;
use wasmtime::{error::Context, *};

#[derive(Parser, Debug)]
#[command(version, about = "Interactive WASM REPL runner for tocat plugins")]
struct Args {
    /// Path to the .wasm module file
    #[arg(value_name = "FILE")]
    path: PathBuf,
}

#[derive(Debug, Default)]
struct Outbox {
    emit: u32,
    bytes_ptr: u32,
    bytes_len: u32,
    bounds_ptr: u32,
    bounds_len: u32,
    flags: u32,
    message_ptr: u32,
    message_len: u32,
    pace_ns: u64,
    logs_ptr: u32,
    logs_len: u32,
}

impl Outbox {
    fn decode(bytes: &[u8; 48]) -> Self {
        Self {
            emit: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            bytes_ptr: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            bytes_len: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            bounds_ptr: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            bounds_len: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            flags: u32::from_le_bytes(bytes[20..24].try_into().unwrap()),
            message_ptr: u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            message_len: u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
            pace_ns: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
            logs_ptr: u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            logs_len: u32::from_le_bytes(bytes[44..48].try_into().unwrap()),
        }
    }

    fn emit_str(&self) -> &'static str {
        match self.emit {
            0 => "0 (drop)",
            1 => "1 (passthrough)",
            2 => "2 (buffered)",
            _ => "unknown",
        }
    }

    fn parse_flags(&self) -> String {
        let mut f = Vec::new();
        if self.flags & 1 != 0 {
            f.push("rearm");
        }
        if self.flags & 2 != 0 {
            f.push("halt");
        }
        if self.flags & 4 != 0 {
            f.push("pace");
        }
        if self.flags & 8 != 0 {
            f.push("error");
        }
        if f.is_empty() {
            "none".to_string()
        } else {
            f.join(" | ")
        }
    }
}

trait Parseable: Sized {
    fn from_radix(src: &str, radix: u32) -> Result<Self, std::num::ParseIntError>;

    fn from_str(src: &str) -> Result<Self, std::num::ParseIntError> {
        if let Some(hex_str) = src.strip_prefix("0x").or_else(|| src.strip_prefix("0X")) {
            Self::from_radix(hex_str, 16)
        } else if let Some(bin_str) = src.strip_prefix("0b").or_else(|| src.strip_prefix("0B")) {
            Self::from_radix(bin_str, 2)
        } else if let Some(oct_str) = src.strip_prefix("0b").or_else(|| src.strip_prefix("0B")) {
            Self::from_radix(oct_str, 8)
        } else {
            Self::from_radix(src, 10)
        }
    }
}

impl Parseable for usize {
    fn from_radix(src: &str, radix: u32) -> Result<Self, std::num::ParseIntError> {
        Self::from_str_radix(src, radix)
    }
}

fn unescape_string(s: &str) -> String {
    let mut chars = s.chars().peekable();
    let mut out = String::with_capacity(s.len());

    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some('0') => out.push('\0'),
                Some('x') => {
                    // Extract hex sequence \xHH
                    let hex: String = chars.by_ref().take(2).collect();
                    if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                        out.push(byte as char);
                    }
                }
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn main() -> Result<()> {
    let args = Args::parse();

    if !args.path.exists() {
        bail!("WASM file not found at path: {}", args.path.display());
    }

    let engine = Engine::default();
    let module = Module::from_file(&engine, &args.path)
        .with_context(|| format!("Failed to load WASM module from {}", args.path.display()))?;

    let mut store = Store::new(&engine, ());
    let linker = Linker::new(&engine);
    let instance = linker.instantiate(&mut store, &module)?;

    // Validate ABI version
    if let Ok(abi_fn) = instance.get_typed_func::<(), i32>(&mut store, "tocat_abi_version") {
        let ver = abi_fn.call(&mut store, ())?;
        if ver != 1 {
            bail!("Incompatible guest ABI version: expected 1, got {ver}");
        }
    } else {
        bail!("Module missing required 'tocat_abi_version' export");
    }

    println!("Loaded WASM plugin (ABI v1): {}", args.path.display());
    println!("\nExported Functions:");
    for export in module.exports() {
        if let Some(FuncType { .. }) = export.ty().func() {
            println!("  - {}", export.name());
        }
    }

    // Read optional configurations once
    if let Ok(f) = instance.get_typed_func::<(), i32>(&mut store, "tocat_datagram_safe") {
        println!("  • Datagram Safe : {}", f.call(&mut store, ())? != 0);
    }
    if let Ok(f) = instance.get_typed_func::<(), i64>(&mut store, "tocat_tick_interval_ns") {
        println!("  • Tick Interval : {} ns", f.call(&mut store, ())?);
    }

    println!("\nREPL Helper Commands:");
    println!(
        "  send <string>                 - Write string to guest, run on_bytes, inspect outbox"
    );
    println!("  init <json_config>            - Invoke tocat_init with a JSON string");
    println!("  tick                          - Invoke tocat_on_tick");
    println!("  eof                           - Invoke tocat_on_eof");
    println!("  outbox                        - Read and decode current outbox struct");
    println!("  peek <ptr> <len>              - Inspect guest memory bytes/utf-8");
    println!("  <func> [arg1] [arg2] ...      - Execute raw WASM exported function");
    println!("  Ctrl+C / Ctrl+D                - Exit\n");

    let mut rl = DefaultEditor::new()?;

    loop {
        let readline = rl.readline("wasm> ");
        match readline {
            Ok(line) => {
                let input = line.trim();
                if input.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(input);

                // --- Helper Commands ---
                if let Some(text) = input.strip_prefix("send ") {
                    let s = unescape_string(text);
                    if let Err(e) = run_send_pipeline(&mut store, &instance, s.as_bytes()) {
                        println!("=> Pipeline Error: {e}");
                    }
                    continue;
                }

                if let Some(json) = input.strip_prefix("init ") {
                    if let Err(e) = run_init(&mut store, &instance, json.as_bytes()) {
                        println!("=> Init Error: {e}");
                    }
                    continue;
                }

                if input == "tick" {
                    if let Err(e) = invoke_void_func(&mut store, &instance, "tocat_on_tick") {
                        println!("=> Tick Error: {e}");
                    } else {
                        let _ = print_outbox(&mut store, &instance);
                    }
                    continue;
                }

                if input == "eof" {
                    if let Err(e) = invoke_void_func(&mut store, &instance, "tocat_on_eof") {
                        println!("=> EOF Error: {e}");
                    } else {
                        let _ = print_outbox(&mut store, &instance);
                    }
                    continue;
                }

                if input == "outbox" {
                    if let Err(e) = print_outbox(&mut store, &instance) {
                        println!("=> Outbox Error: {e}");
                    }
                    continue;
                }

                if let Some(rest) = input.strip_prefix("peek ") {
                    let parts: Vec<&str> = rest.split_whitespace().collect();
                    if parts.len() == 2
                        && let (Ok(ptr), Ok(len)) =
                            (usize::from_str(parts[0]), usize::from_str(parts[1]))
                    // (parts[0].parse::<usize>(), parts[1].parse::<usize>())
                    {
                        peek_memory(&mut store, &instance, ptr, len);
                        continue;
                    }
                    println!("=> Usage: peek <ptr> <len>");
                    continue;
                }

                // --- Generic Function Invocation ---
                let mut parts = input.split_whitespace();
                let func_name = parts.next().unwrap();
                let raw_args: Vec<&str> = parts.collect();

                if let Some(func) = instance.get_func(&mut store, func_name) {
                    let func_type = func.ty(&store);
                    let expected = func_type.params().len();

                    if raw_args.len() != expected {
                        println!(
                            "=> Argument mismatch: '{func_name}' expects {expected} arg(s), received {}",
                            raw_args.len()
                        );
                        continue;
                    }

                    let mut wasm_args = Vec::new();
                    let mut parse_failed = false;

                    for (i, param_ty) in func_type.params().enumerate() {
                        match param_ty {
                            ValType::I32 => {
                                if let Ok(val) = raw_args[i].parse::<i32>() {
                                    wasm_args.push(Val::I32(val));
                                } else {
                                    println!(
                                        "=> Error: Failed to parse arg '{}' as i32",
                                        raw_args[i]
                                    );
                                    parse_failed = true;
                                    break;
                                }
                            }
                            ValType::I64 => {
                                if let Ok(val) = raw_args[i].parse::<i64>() {
                                    wasm_args.push(Val::I64(val));
                                } else {
                                    println!(
                                        "=> Error: Failed to parse arg '{}' as i64",
                                        raw_args[i]
                                    );
                                    parse_failed = true;
                                    break;
                                }
                            }
                            other => {
                                println!("=> Unsupported parameter type: {:?}", other);
                                parse_failed = true;
                                break;
                            }
                        }
                    }

                    if parse_failed {
                        continue;
                    }

                    let mut results = vec![Val::I32(0); func_type.results().len()];
                    match func.call(&mut store, &wasm_args, &mut results) {
                        Ok(()) => {
                            if !results.is_empty() {
                                println!("=> Output: {:?}", results);
                            } else {
                                println!("=> Executed successfully");
                            }
                            let _ = print_outbox(&mut store, &instance);
                        }
                        Err(err) => println!("=> Execution Trap: {err}"),
                    }
                } else {
                    println!("=> Function '{func_name}' not found in exports.");
                }
            }
            Err(_) => break,
        }
    }

    Ok(())
}

// Read and display the guest's 48-byte struct at tocat_outbox()
fn print_outbox(store: &mut Store<()>, instance: &Instance) -> Result<()> {
    let outbox_fn = instance
        .get_typed_func::<(), i32>(&mut *store, "tocat_outbox")
        .context("Missing 'tocat_outbox' export")?;

    let memory = instance
        .get_memory(&mut *store, "memory")
        .context("Missing 'memory' export")?;

    let ptr = outbox_fn.call(&mut *store, ())? as usize;

    let mut buf = [0u8; 48];
    memory.read(&mut *store, ptr, &mut buf)?;
    let ob = Outbox::decode(&buf);

    println!("\n--- Outbox State (ptr: 0x{ptr:x}) ---");
    println!("  emit         : {}", ob.emit_str());
    println!(
        "  bytes        : ptr=0x{:x}, len={}",
        ob.bytes_ptr, ob.bytes_len
    );
    println!(
        "  bounds       : ptr=0x{:x}, len={}",
        ob.bounds_ptr, ob.bounds_len
    );
    println!("  flags        : {}", ob.parse_flags());

    if ob.message_len > 0 {
        let mut msg_bytes = vec![0u8; ob.message_len as usize];
        if memory
            .read(&mut *store, ob.message_ptr as usize, &mut msg_bytes)
            .is_ok()
        {
            println!(
                "  message      : \"{}\"",
                String::from_utf8_lossy(&msg_bytes)
            );
        }
    }

    if ob.pace_ns > 0 {
        println!("  pace_ns      : {} ns", ob.pace_ns);
    }

    if ob.logs_len > 0 {
        println!("  logs ({})   :", ob.logs_len);
        let mut log_ptr = ob.logs_ptr as usize;
        for i in 0..ob.logs_len {
            let mut record = [0u8; 12];
            if memory.read(&mut *store, log_ptr, &mut record).is_ok() {
                let level = u32::from_le_bytes(record[0..4].try_into().unwrap());
                let p = u32::from_le_bytes(record[4..8].try_into().unwrap()) as usize;
                let l = u32::from_le_bytes(record[8..12].try_into().unwrap()) as usize;

                let mut txt = vec![0u8; l];
                let log_str = if memory.read(&mut *store, p, &mut txt).is_ok() {
                    String::from_utf8_lossy(&txt).to_string()
                } else {
                    "<invalid string>".to_string()
                };

                let lvl_str = match level {
                    0 => "TRACE",
                    1 => "DEBUG",
                    2 => "INFO",
                    3 => "WARN",
                    4 => "ERROR",
                    _ => "UNK",
                };
                println!("    [{i}] {lvl_str}: {log_str}");
            }
            log_ptr += 12;
        }
    }

    if ob.emit == 2 && ob.bytes_len > 0 {
        let mut emitted = vec![0u8; ob.bytes_len as usize];
        if memory
            .read(&mut *store, ob.bytes_ptr as usize, &mut emitted)
            .is_ok()
        {
            println!(
                "  [Emitted Content]: \"{}\"",
                String::from_utf8_lossy(&emitted)
            );
        }
    }
    println!("--------------------------------\n");

    Ok(())
}

fn run_send_pipeline(store: &mut Store<()>, instance: &Instance, input_bytes: &[u8]) -> Result<()> {
    let memory = instance
        .get_memory(&mut *store, "memory")
        .context("Missing 'memory' export")?;

    let alloc_fn = instance
        .get_typed_func::<i32, i32>(&mut *store, "tocat_alloc")
        .context("Missing 'tocat_alloc'")?;

    let on_bytes_fn = instance
        .get_typed_func::<(i32, i32), ()>(&mut *store, "tocat_on_bytes")
        .context("Missing 'tocat_on_bytes'")?;

    let ptr = alloc_fn.call(&mut *store, input_bytes.len() as i32)?;
    if ptr == 0 {
        bail!("Guest refused allocation size {}", input_bytes.len());
    }

    memory.write(&mut *store, ptr as usize, input_bytes)?;
    on_bytes_fn.call(&mut *store, (ptr, input_bytes.len() as i32))?;

    print_outbox(store, instance)?;
    Ok(())
}

fn run_init(store: &mut Store<()>, instance: &Instance, json_bytes: &[u8]) -> Result<()> {
    let memory = instance
        .get_memory(&mut *store, "memory")
        .context("Missing 'memory' export")?;

    let alloc_fn = instance
        .get_typed_func::<i32, i32>(&mut *store, "tocat_alloc")
        .context("Missing 'tocat_alloc'")?;

    let init_fn = instance
        .get_typed_func::<(i32, i32), ()>(&mut *store, "tocat_init")
        .context("Plugin does not export 'tocat_init'")?;

    let ptr = alloc_fn.call(&mut *store, json_bytes.len() as i32)?;
    if ptr == 0 {
        bail!("Guest refused allocation for init JSON");
    }

    memory.write(&mut *store, ptr as usize, json_bytes)?;
    init_fn.call(&mut *store, (ptr, json_bytes.len() as i32))?;

    print_outbox(store, instance)?;
    Ok(())
}

fn invoke_void_func(store: &mut Store<()>, instance: &Instance, name: &str) -> Result<()> {
    let func = instance
        .get_typed_func::<(), ()>(&mut *store, name)
        .with_context(|| format!("Plugin does not export '{name}'"))?;
    func.call(&mut *store, ())?;
    Ok(())
}

fn peek_memory(store: &mut Store<()>, instance: &Instance, ptr: usize, len: usize) {
    if let Some(memory) = instance.get_memory(&mut *store, "memory") {
        let mut buf = vec![0u8; len];
        if memory.read(&mut *store, ptr, &mut buf).is_ok() {
            println!("=> Raw Bytes [0x{ptr:x}..0x{:x}]: {:?}", ptr + len, buf);
            println!("=> String    : \"{}\"", String::from_utf8_lossy(&buf));
        } else {
            println!("=> Error: Memory read out of bounds.");
        }
    }
}
