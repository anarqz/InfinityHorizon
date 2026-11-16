//! The reply envelope: a 12-byte header of three big-endian u32 `[status][code][message length]`.
//! Status 0 is success and the `_Results` follow the header (the game ignores `code` and the
//! length); status 2 is an error the game reports as HRESULT `0x80510000 | code`, followed by the
//! message, which it logs; any other status makes it report `0x8050000e`.

use crate::error::XtswError;

/// Length of the reply header.
pub const HEADER_LEN: usize = 12;
/// Header status of a success reply.
pub const STATUS_OK: u32 = 0;
/// Header status of an error reply.
pub const STATUS_ERROR: u32 = 2;
/// Longest error message [`error_envelope`] writes (longer ones are cut at a character boundary).
pub const MAX_ERROR_MESSAGE: usize = 0x2000;

/// A parsed reply envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Envelope<'a> {
    /// Status 0: the `_Results` bytes after the header.
    Ok {
        /// The encoded `_Results` (empty for methods without one).
        results: &'a [u8],
    },
    /// Status 2: the game reports HRESULT `0x80510000 | code`.
    Error {
        /// The error code of the header.
        code: u32,
        /// The message the game logs.
        message: &'a str,
    },
}

/// A success reply: the 12-byte header with status, code and length 0, then the `_Results`.
pub fn ok_envelope(results: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + results.len());
    out.extend_from_slice(&[0; HEADER_LEN]);
    out.extend_from_slice(results);
    out
}

/// An error reply: status 2, `code`, and the message (at most [`MAX_ERROR_MESSAGE`] bytes).
pub fn error_envelope(code: u32, message: &str) -> Vec<u8> {
    let mut end = message.len().min(MAX_ERROR_MESSAGE);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let message = &message.as_bytes()[..end];
    let mut out = Vec::with_capacity(HEADER_LEN + message.len());
    out.extend_from_slice(&STATUS_ERROR.to_be_bytes());
    out.extend_from_slice(&code.to_be_bytes());
    // end <= MAX_ERROR_MESSAGE < u32::MAX
    out.extend_from_slice(&(end as u32).to_be_bytes());
    out.extend_from_slice(message);
    out
}

/// Parses a reply as the game does, strictly: a short header, a message longer than the reply,
/// a message that is not UTF-8, bytes after an error message or a status other than 0 and 2 are
/// errors. A success reply's `code` and length words are ignored, like the game ignores them.
pub fn parse_envelope(bytes: &[u8]) -> Result<Envelope<'_>, XtswError> {
    if bytes.len() < HEADER_LEN {
        return Err(XtswError::Truncated {
            path: "envelope".into(),
            offset: 0,
            needed: HEADER_LEN,
            available: bytes.len(),
        });
    }
    let word = |i: usize| u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let (status, code, len) = (word(0), word(4), word(8));
    match status {
        STATUS_OK => Ok(Envelope::Ok {
            results: &bytes[HEADER_LEN..],
        }),
        STATUS_ERROR => {
            let available = bytes.len() - HEADER_LEN;
            let len = usize::try_from(len).unwrap_or(usize::MAX);
            if len > available {
                return Err(XtswError::Truncated {
                    path: "envelope.message".into(),
                    offset: HEADER_LEN,
                    needed: len,
                    available,
                });
            }
            if len < available {
                return Err(XtswError::TrailingBytes {
                    ty: "error envelope".into(),
                    count: available - len,
                });
            }
            let message =
                std::str::from_utf8(&bytes[HEADER_LEN..]).map_err(|_| XtswError::InvalidUtf8 {
                    path: "envelope.message".into(),
                    offset: HEADER_LEN,
                })?;
            Ok(Envelope::Error { code, message })
        }
        status => Err(XtswError::UnknownStatus { status }),
    }
}
