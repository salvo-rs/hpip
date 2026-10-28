use std::fs::File;
use std::io::{self, BufReader, BufWriter, Seek, Write};
use std::path::Path;

use brotli::enc::BrotliCompress as brotli_compress;
use brotli::enc::backward_references::BrotliEncoderParams;
use flate2::Compression;
use flate2::write::{GzEncoder, ZlibEncoder};

use crate::config::EncodingType;

pub fn identity_acceptable(accept: &str) -> bool {
    let mut identity = None;
    let mut wildcard = None;
    for part in accept.split(',') {
        let mut fields = part.split(';');
        let name = fields.next().unwrap_or("").trim();
        let mut quality = 1.0_f32;
        for parameter in fields {
            if let Some((key, value)) = parameter.trim().split_once('=')
                && key.trim().eq_ignore_ascii_case("q")
            {
                quality = value
                    .trim()
                    .parse::<f32>()
                    .ok()
                    .filter(|q| q.is_finite() && (0.0..=1.0).contains(q))
                    .unwrap_or(0.0);
            }
        }
        if name.eq_ignore_ascii_case("identity") {
            identity = Some(quality);
        }
        if name == "*" {
            wildcard = Some(quality);
        }
    }
    identity.unwrap_or_else(|| if wildcard == Some(0.0) { 0.0 } else { 1.0 }) > 0.0
}

/// Find best supported encoding to use from Accept-Encoding header value, or None for identity.
pub fn response_encoding(accept: &str) -> Option<EncodingType> {
    let preferences: Vec<_> = accept
        .split(',')
        .map(|part| {
            let mut fields = part.split(';');
            let name = fields.next().unwrap_or("").trim().to_ascii_lowercase();
            let mut quality = 1.0_f32;
            for parameter in fields {
                if let Some((key, value)) = parameter.trim().split_once('=')
                    && key.trim().eq_ignore_ascii_case("q")
                {
                    quality = value
                        .trim()
                        .parse::<f32>()
                        .ok()
                        .filter(|q| q.is_finite() && (0.0..=1.0).contains(q))
                        .unwrap_or(0.0);
                }
            }
            (name, quality)
        })
        .collect();
    let wildcard = preferences
        .iter()
        .find(|(name, _)| name == "*")
        .map(|(_, q)| *q);
    // Keep server preference only for equal quality values; explicit exclusions beat '*'.
    let mut best = None;
    for (name, encoding) in [
        ("br", EncodingType::Brotli),
        ("gzip", EncodingType::Gzip),
        ("deflate", EncodingType::Deflate),
    ] {
        let quality = preferences
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, q)| *q)
            .or(wildcard)
            .unwrap_or(0.0);
        if quality > 0.0 && best.as_ref().is_none_or(|(_, q)| quality > *q) {
            best = Some((encoding, quality));
        }
    }
    let identity = preferences
        .iter()
        .find(|(n, _)| n == "identity")
        .map(|(_, q)| *q);
    best.filter(|(_, q)| identity.is_none_or(|identity| *q >= identity))
        .map(|(enc, _)| enc)
}

/// Encode a string slice using a specified encoding.
pub fn encode_str(dt: &str, enc: EncodingType) -> Option<Vec<u8>> {
    match enc {
        EncodingType::Gzip => encode_str_gzip(dt),
        EncodingType::Deflate => encode_str_deflate(dt),
        EncodingType::Brotli => encode_str_brotli(dt),
    }
}

/// Encode the already opened input snapshot, never reopening its path.
pub fn encode_open_file(mut inf: File, op: &Path, enc: EncodingType) -> bool {
    if inf.rewind().is_err() {
        return false;
    }
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

/// Hash the same opened input that will be compressed, then reset its cursor.
pub fn file_hash_reader(source: &mut File) -> Result<blake3::Hash, io::Error> {
    source.rewind()?;
    let mut ctx = blake3::Hasher::new();
    io::copy(
        &mut BufReader::with_capacity(1024 * 1024, &mut *source),
        &mut ctx,
    )?;
    source.rewind()?;
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
    write_gzip(inf, outf).is_ok()
}

fn write_gzip(input: impl io::Read, output: impl Write) -> io::Result<()> {
    let mut output = BufWriter::with_capacity(1024 * 1024, output);
    let mut encoder = GzEncoder::new(&mut output, Compression::default());
    io::copy(
        &mut BufReader::with_capacity(1024 * 1024, input),
        &mut encoder,
    )?;
    encoder.finish()?;
    // Finishing the encoder does not flush the outer writer. Drop ignores errors.
    output.flush()
}

// Deflate
fn encode_str_deflate(dt: &str) -> Option<Vec<u8>> {
    let mut cmp = ZlibEncoder::new(Vec::new(), Compression::default());
    cmp.write_all(dt.as_bytes())
        .ok()
        .and_then(|_| cmp.finish().ok())
}

fn encode_file_deflate(inf: File, outf: File) -> bool {
    write_deflate(inf, outf).is_ok()
}

fn write_deflate(input: impl io::Read, output: impl Write) -> io::Result<()> {
    let mut output = BufWriter::with_capacity(1024 * 1024, output);
    let mut encoder = ZlibEncoder::new(&mut output, Compression::default());
    io::copy(
        &mut BufReader::with_capacity(1024 * 1024, input),
        &mut encoder,
    )?;
    encoder.finish()?;
    output.flush()
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
    write_brotli(inf, outf).is_ok()
}

fn write_brotli(input: impl io::Read, output: impl Write) -> io::Result<()> {
    let mut output = BufWriter::with_capacity(1024 * 1024, output);
    brotli_compress(
        &mut BufReader::with_capacity(1024 * 1024, input),
        &mut output,
        &brotli_params(),
    )?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    struct FailedDisk;

    impl Write for FailedDisk {
        fn write(&mut self, _data: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("simulated full disk"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn file_encoders_propagate_the_final_buffered_write_failure() {
        let input = b"small compressible content that fits the output buffer";
        for result in [
            write_gzip(&input[..], FailedDisk),
            write_deflate(&input[..], FailedDisk),
            write_brotli(&input[..], FailedDisk),
        ] {
            assert_eq!(result.unwrap_err().to_string(), "simulated full disk");
        }
    }

    #[test]
    fn buffered_file_encodings_are_complete_and_decodable() {
        let input = b"complete encoded fixture";
        let mut encoded = Vec::new();
        write_gzip(&input[..], &mut encoded).unwrap();
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(&encoded[..])
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, input);
        encoded.clear();
        decoded.clear();
        write_deflate(&input[..], &mut encoded).unwrap();
        flate2::read::ZlibDecoder::new(&encoded[..])
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, input);
        encoded.clear();
        decoded.clear();
        write_brotli(&input[..], &mut encoded).unwrap();
        brotli::Decompressor::new(&encoded[..], 4096)
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, input);
    }

    #[test]
    fn respects_quality_before_server_preference() {
        assert_eq!(
            response_encoding("br;q=0.9, gzip;q=1"),
            Some(EncodingType::Gzip)
        );
        assert_eq!(
            response_encoding("GZIP ; q = 0.7, br;q=0.1"),
            Some(EncodingType::Gzip)
        );
        assert_eq!(
            response_encoding("br;q=0, *;q=0.5"),
            Some(EncodingType::Gzip)
        );
        assert_eq!(response_encoding("gzip;q=NaN,br;q=2,deflate;q=-1"), None);
        assert_eq!(response_encoding("gzip;q=0.1,identity;q=1"), None);
    }

    #[test]
    fn deflate_is_a_zlib_wrapped_stream() {
        let compressed = encode_str_deflate("HTTP deflate fixture").unwrap();
        let mut output = String::new();
        flate2::read::ZlibDecoder::new(&compressed[..])
            .read_to_string(&mut output)
            .unwrap();
        assert_eq!(output, "HTTP deflate fixture");
    }
}
