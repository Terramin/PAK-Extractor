use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use brotli::Decompressor;
use bzip2::read::BzDecoder;
use clap::Parser;
use rayon::prelude::*;

#[derive(Parser, Debug)]
#[command(author = "Terramin", version = "1.0", about = ".PAK unpacker")]
struct Args {
    #[arg(value_name = "FILE", required = false)]
    input: Option<PathBuf>,

    #[arg(short, long, value_name = "DIR")]
    output: Option<PathBuf>,

    #[arg(short, long, default_value_t = 0)]
    threads: usize,
}

#[derive(Debug)]
struct PakEntry {
    offset: u32,
    packed_size: u32,
    unpacked_size: u32,
    compression: u8,
    filename: String,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();

    let input = args.input.unwrap_or_else(|| {
        if let Some(path) = std::env::args_os().nth(1) {
            PathBuf::from(path)
        } else {
            eprintln!(
                "Usage: {} <input.pak> [options]",
                std::env::args().next().unwrap_or_default()
            );
            eprintln!("   or drag .pak file onto the .exe");
            std::process::exit(1);
        }
    });

    let output = match args.output {
        Some(dir) => dir,
        None => {
            if let Some(stem) = input.file_stem() {
                let mut new_dir = input.with_file_name(stem);
                new_dir.set_file_name(format!("{}_unpak", stem.to_string_lossy()));
                new_dir
            } else {
                PathBuf::from("extracted")
            }
        }
    };

    if args.threads > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(args.threads)
            .build_global()
            .unwrap();
    }

    fs::create_dir_all(&output)?;

    println!("Reading: {}", input.display());
    println!("Extracting to: {}", output.display());

    let mut file = File::open(&input)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;

    let data = Arc::new(buffer);
    let mut cursor = io::Cursor::new(data.as_ref());

    let mut magic = [0u8; 4];
    cursor.read_exact(&mut magic)?;
    if &magic != b"MDPK" {
        eprintln!("Error: Invalid .PAK file");
        std::process::exit(1);
    }

    let version = cursor.read_u16_le()?;
    println!("PAK Version: {}", version);

    let file_count = if version < 5 {
        cursor.read_u16_le()? as u32
    } else {
        cursor.read_u32_le()?
    };
    println!("Total files: {}", file_count);

    let mut timestamp = [0u8; 16];
    cursor.read_exact(&mut timestamp)?;
    if let Ok(ts) = std::str::from_utf8(&timestamp) {
        println!("Timestamp: {}", ts.trim_end_matches('\0'));
    }

    let mut entries = Vec::with_capacity(file_count as usize);
    for _ in 0..file_count {
        let offset = cursor.read_u32_le()?;
        let packed_size = cursor.read_u32_le()?;
        let unpacked_size = cursor.read_u32_le()?;
        let compression = cursor.read_u8()?;

        let mut name_buf = [0u8; 55];
        cursor.read_exact(&mut name_buf)?;

        let filename = name_buf
            .iter()
            .take_while(|&&b| b != 0)
            .copied()
            .collect::<Vec<u8>>();
        let filename = String::from_utf8_lossy(&filename).to_string();

        entries.push(PakEntry {
            offset,
            packed_size,
            unpacked_size,
            compression,
            filename,
        });
    }

    let output = Arc::new(output);
    let data_ref = data.clone();
    let processed = AtomicUsize::new(0);
    let total = entries.len();

    println!(
        "\nStarting parallel extraction with {} threads...\n",
        if args.threads == 0 {
            rayon::current_num_threads()
        } else {
            args.threads
        }
    );

    entries.par_iter().for_each(|entry| {
        let idx = processed.fetch_add(1, Ordering::SeqCst) + 1;
        let output = output.clone();
        let data = data_ref.clone();

        if let Err(e) = extract_file(idx, total, entry, &data, &output) {
            eprintln!(
                "\n[{}/{}] Error extracting {}: {}",
                idx, total, entry.filename, e
            );
        }
    });

    println!("\n\nExtraction completed successfully!");
    Ok(())
}

fn extract_file(
    index: usize,
    total: usize,
    entry: &PakEntry,
    data: &[u8],
    output_dir: &PathBuf,
) -> io::Result<()> {
    print!("\r[{:3}/{}] {} ... ", index, total, entry.filename);
    let _ = io::stdout().flush();

    let mut cursor = io::Cursor::new(data);
    cursor.seek(SeekFrom::Start(entry.offset as u64))?;

    let mut packed_data = vec![0u8; entry.packed_size as usize];
    cursor.read_exact(&mut packed_data)?;

    let decompressed = match entry.compression {
        0 => packed_data,
        1 => {
            let mut decoder = BzDecoder::new(&packed_data[..]);
            let mut out = Vec::new();
            decoder.read_to_end(&mut out)?;
            out
        }
        2 => {
            let mut decoder = Decompressor::new(&packed_data[..], 4096);
            let mut out = Vec::new();
            decoder.read_to_end(&mut out)?;
            out
        }
        _ => {
            eprintln!("\nUnknown compression: {}", entry.compression);
            return Ok(());
        }
    };

    if decompressed.len() != entry.unpacked_size as usize {
        eprintln!("\nWarning: size mismatch for {}", entry.filename);
    }

    let output_path = output_dir.join(&entry.filename);
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut out_file = File::create(&output_path)?;
    out_file.write_all(&decompressed)?;

    print!("\r[{:3}/{}] {} OK", index, total, entry.filename);
    let _ = io::stdout().flush();
    Ok(())
}

trait ReadLeExt: Read {
    fn read_u16_le(&mut self) -> io::Result<u16> {
        let mut buf = [0u8; 2];
        self.read_exact(&mut buf)?;
        Ok(u16::from_le_bytes(buf))
    }

    fn read_u32_le(&mut self) -> io::Result<u32> {
        let mut buf = [0u8; 4];
        self.read_exact(&mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    fn read_u8(&mut self) -> io::Result<u8> {
        let mut buf = [0u8; 1];
        self.read_exact(&mut buf)?;
        Ok(buf[0])
    }
}

impl<R: Read + ?Sized> ReadLeExt for R {}
