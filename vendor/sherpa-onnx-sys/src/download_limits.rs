use std::io::Read;

/// The largest compressed sherpa distribution archive accepted by the build.
///
/// Current pinned desktop archives are far smaller than this. Keeping the cap
/// independent from the server-provided Content-Length makes a compromised
/// release host unable to make the build process allocate an arbitrary amount
/// of memory.
pub const MAX_SHERPA_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

pub fn parse_content_length(values: &[&str], max_bytes: u64) -> Result<u64, String> {
    if values.len() != 1 {
        return Err(format!(
            "archive response must contain exactly one Content-Length header, found {}",
            values.len()
        ));
    }
    let value = values[0];
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("archive Content-Length is not a strict decimal integer".to_string());
    }
    let length = value
        .parse::<u64>()
        .map_err(|_| "archive Content-Length is not a valid unsigned integer".to_string())?;

    if length == 0 {
        return Err("archive Content-Length must be greater than zero".to_string());
    }
    if length > max_bytes {
        return Err(format!(
            "archive Content-Length {length} exceeds the {max_bytes}-byte safety limit"
        ));
    }
    if usize::try_from(length).is_err() {
        return Err(
            "archive Content-Length does not fit this platform's address space".to_string(),
        );
    }

    Ok(length)
}

pub fn read_archive_limited<R: Read>(
    reader: R,
    declared_length: u64,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    if declared_length == 0 || declared_length > max_bytes {
        return Err("archive declared length is outside the safety limit".to_string());
    }

    let capacity = usize::try_from(declared_length)
        .map_err(|_| "archive declared length does not fit this platform's address space")?;
    let limit_plus_one = declared_length
        .checked_add(1)
        .ok_or_else(|| "archive read limit overflowed".to_string())?;
    let mut buffer = Vec::with_capacity(capacity.min(64 * 1024));
    reader
        .take(limit_plus_one)
        .read_to_end(&mut buffer)
        .map_err(|err| format!("failed while reading archive response: {err}"))?;

    let actual_length = u64::try_from(buffer.len())
        .map_err(|_| "archive response length does not fit in u64".to_string())?;
    if actual_length != declared_length {
        return Err(format!(
            "archive response length mismatch: declared {declared_length}, received {actual_length}"
        ));
    }

    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};

    #[test]
    fn content_length_is_required_and_strictly_numeric() {
        assert!(parse_content_length(&[], 10).is_err());
        assert!(parse_content_length(&["5", "5"], 10).is_err());
        assert!(parse_content_length(&[""], 10).is_err());
        assert!(parse_content_length(&["-1"], 10).is_err());
        assert!(parse_content_length(&["+5"], 10).is_err());
        assert!(parse_content_length(&[" 5"], 10).is_err());
        assert!(parse_content_length(&["5 "], 10).is_err());
    }

    #[test]
    fn content_length_rejects_zero_and_limit_plus_one() {
        assert!(parse_content_length(&["0"], 10).is_err());
        assert_eq!(parse_content_length(&["10"], 10).unwrap(), 10);
        assert!(parse_content_length(&["11"], 10).is_err());
    }

    #[test]
    fn exact_body_is_accepted() {
        let body = read_archive_limited(Cursor::new(b"abcd"), 4, 8).unwrap();
        assert_eq!(body, b"abcd");
    }

    #[test]
    fn truncated_body_is_rejected() {
        let error = read_archive_limited(Cursor::new(b"abc"), 4, 8).unwrap_err();
        assert!(error.contains("declared 4, received 3"));
    }

    #[test]
    fn oversized_body_reads_only_declared_length_plus_one() {
        struct CountingReader {
            remaining: usize,
            bytes_read: usize,
        }

        impl Read for CountingReader {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                let count = output.len().min(self.remaining);
                output[..count].fill(b'x');
                self.remaining -= count;
                self.bytes_read += count;
                Ok(count)
            }
        }

        let mut reader = CountingReader {
            remaining: 1_000_000,
            bytes_read: 0,
        };
        let error = read_archive_limited(&mut reader, 4, 8).unwrap_err();
        assert!(error.contains("declared 4, received 5"));
        assert_eq!(reader.bytes_read, 5);
    }

    #[test]
    fn invalid_declared_length_fails_before_reading() {
        struct PanicReader;

        impl Read for PanicReader {
            fn read(&mut self, _output: &mut [u8]) -> io::Result<usize> {
                panic!("reader must not be touched")
            }
        }

        assert!(read_archive_limited(PanicReader, 0, 8).is_err());
        assert!(read_archive_limited(PanicReader, 9, 8).is_err());
    }

    #[test]
    fn reader_errors_are_not_treated_as_truncation() {
        struct FailingReader;

        impl Read for FailingReader {
            fn read(&mut self, _output: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("simulated failure"))
            }
        }

        let error = read_archive_limited(FailingReader, 4, 8).unwrap_err();
        assert!(error.contains("simulated failure"));
    }
}
