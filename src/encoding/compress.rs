use std::fs::File;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::Path;

use brotli::enc::BrotliCompress as brotli_compress;
use brotli::enc::backward_references::BrotliEncoderParams;
use flate2::Compression;
use flate2::write::{DeflateEncoder, GzEncoder};

use crate::config::EncodingType;

/// Find best supported encoding to use from Accept-Encoding header value, or None for identity.
pub fn response_encoding(accept: &str) -> Option<EncodingType> {
    // Parse Accept-Encoding header, pick best supported encoding
    // Priority: brotli > gzip > deflate (when quality allows)
    let mut best: Option<(EncodingType, f32)> = None;

    for part in accept.split(',') {
        let part = part.trim();
        let (enc_name, quality) = if let Some((name, q_str)) = part.split_once(";q=") {
            (name.trim(), q_str.trim().parse::<f32>().unwrap_or(1.0))
        } else {
            (part, 1.0)
        };

        if quality == 0.0 {
            continue;
        }

        let enc_type = match enc_name {
            "br" => Some(EncodingType::Brotli),
            "gzip" => Some(EncodingType::Gzip),
            "deflate" => Some(EncodingType::Deflate),
            _ => None,
        };

        if let Some(enc) = enc_type {
            let priority = match enc {
                EncodingType::Brotli => 3.0,
                EncodingType::Gzip => 2.0,
                EncodingType::Deflate => 1.0,
            };
            let score = quality * 10.0 + priority;
            if best.as_ref().is_none_or(|b| {
                score
                    > b.1 * 10.0
                        + match b.0 {
                            EncodingType::Brotli => 3.0,
                            EncodingType::Gzip => 2.0,
                            EncodingType::Deflate => 1.0,
                        }
            }) {
                best = Some((enc, quality));
            }
        }
    }

    best.map(|(enc, _)| enc)
}

/// Encode a string slice using a specified encoding.
pub fn encode_str(dt: &str, enc: EncodingType) -> Option<Vec<u8>> {
    match enc {
        EncodingType::Gzip => encode_str_gzip(dt),
        EncodingType::Deflate => encode_str_deflate(dt),
        EncodingType::Brotli => encode_str_brotli(dt),
    }
}

/// Encode the file at `p` into the file at `op` using the given encoding. Returns false on failure.
pub fn encode_file(p: &Path, op: &Path, enc: EncodingType) -> bool {
    let inf = match File::open(p) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let outf = match File::create(op) {
        Ok(f) => f,
        Err(_) => return false,
    };

    match enc {
        EncodingType::Gzip => encode_file_gzip(inf, outf),
        EncodingType::Deflate => encode_file_deflate(inf, outf),
        EncodingType::Brotli => encode_file_brotli(inf, outf),
    }
}

/// Encoding extension for encoded files.
pub fn encoding_extension(enc: EncodingType) -> &'static str {
    match enc {
        EncodingType::Gzip => "gz",
        EncodingType::Deflate => "dflt",
        EncodingType::Brotli => "br",
    }
}

/// Encoding name for Content-Encoding header.
pub fn encoding_name(enc: EncodingType) -> &'static str {
    match enc {
        EncodingType::Gzip => "gzip",
        EncodingType::Deflate => "deflate",
        EncodingType::Brotli => "br",
    }
}

/// Return the 256-bit BLAKE3 hash of the file.
pub fn file_hash(p: &Path) -> Result<blake3::Hash, io::Error> {
    let mut ctx = blake3::Hasher::new();
    io::copy(
        &mut BufReader::with_capacity(1024 * 1024, File::open(p)?),
        &mut ctx,
    )?;
    Ok(ctx.finalize())
}

// Gzip
fn encode_str_gzip(dt: &str) -> Option<Vec<u8>> {
    let mut cmp = GzEncoder::new(Vec::new(), Compression::default());
    cmp.write_all(dt.as_bytes())
        .ok()
        .and_then(|_| cmp.finish().ok())
}

fn encode_file_gzip(inf: File, outf: File) -> bool {
    let mut cmp = GzEncoder::new(
        BufWriter::with_capacity(1024 * 1024, outf),
        Compression::default(),
    );
    io::copy(&mut BufReader::with_capacity(1024 * 1024, inf), &mut cmp)
        .and_then(|_| cmp.finish())
        .is_ok()
}

// Deflate
fn encode_str_deflate(dt: &str) -> Option<Vec<u8>> {
    let mut cmp = DeflateEncoder::new(Vec::new(), Compression::default());
    cmp.write_all(dt.as_bytes())
        .ok()
        .and_then(|_| cmp.finish().ok())
}

fn encode_file_deflate(inf: File, outf: File) -> bool {
    let mut cmp = DeflateEncoder::new(
        BufWriter::with_capacity(1024 * 1024, outf),
        Compression::default(),
    );
    io::copy(&mut BufReader::with_capacity(1024 * 1024, inf), &mut cmp)
        .and_then(|_| cmp.finish())
        .is_ok()
}

// Brotli
fn brotli_params() -> BrotliEncoderParams {
    BrotliEncoderParams {
        quality: 9,
        ..Default::default()
    }
}

fn encode_str_brotli(dt: &str) -> Option<Vec<u8>> {
    let mut ret = Vec::new();
    brotli_compress(&mut dt.as_bytes(), &mut ret, &brotli_params())
        .ok()
        .map(|_| ret)
}

fn encode_file_brotli(inf: File, outf: File) -> bool {
    brotli_compress(
        &mut BufReader::with_capacity(1024 * 1024, inf),
        &mut BufWriter::with_capacity(1024 * 1024, outf),
        &brotli_params(),
    )
    .is_ok()
}
