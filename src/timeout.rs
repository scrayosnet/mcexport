//! This module defines and handles the timeouts for individual probing requests.
//!
//! The timeouts are defined per-request and are supplied by Prometheus. There is a global fallback that gets used if
//! the request headers don't contain the timeout value. An offset is subtracted from the timeout to account for latency
//! and global processing time, so that the Prometheus timeout never expires before ours.

use axum::http::header::ToStrError;
use axum::http::HeaderMap;
use std::num::ParseFloatError;
use std::time::Duration;
use tracing::{debug, trace};

/// The name of the request header from Prometheus, that sets the scrape timeout.
///
/// This header should always be present on requests from Prometheus and can contain floating point numbers. This time
/// is the overall scraping time allocated for this probe request. Therefore, some offset has to be subtracted from the
/// duration to account for network latency and other processing overheads, not part of the timeout.
const SCRAPE_TIMEOUT_HEADER: &str = "X-Prometheus-Scrape-Timeout-Seconds";

/// The internal error type for all errors related to the timeout parsing.
///
/// This includes errors with the expected packets, packet contents or encoding of the exchanged fields. Errors of the
/// underlying data layer (for Byte exchange) are wrapped from the underlying IO errors. Additionally, the internal
/// timeout limits also are covered as errors.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// An error occurred while trying to read the header value into an ASCII string.
    #[error("not all bytes are ascii printable: {0}")]
    InvalidChars(#[source] ToStrError),
    /// An error occurred while trying to parse the header value into a float.
    #[error("the value \"{1}\" could not be parsed into a float: {0}")]
    InvalidNumber(#[source] ParseFloatError, String),
    /// The calculated timeout value would end up negative with the offset.
    #[error("the resulting duration ended up being negative: {result:?} (original {original:?})")]
    NegativeDuration {
        /// The original value that was supplied for the calculation.
        original: f64,
        /// The resulting value that was calculated based on the offset and the original.
        result: f64,
    },
}

/// Returns the desired request timeout for the probe endpoint from the supplied headers.
///
/// If the [`SCRAPE_TIMEOUT_HEADER`] is set, the value is parsed into a floating point number, which is interpreted as
/// the duration in seconds and will be allowed for the request. Should no such header exist, the supplied default is
/// used instead. The duration is then reduced by the supplied offset to account for the network latency and processing
/// overhead that does not count into the timeout. This method never returns a negative duration and instead returns a
/// [`NegativeDuration`] error with the resulting values.
pub fn get_duration(headers: &HeaderMap, default: f64, offset: f64) -> Result<Duration, Error> {
    // try to use the header value to parse the duration
    trace!("trying to find scrape timeout header");
    let header = headers.get(SCRAPE_TIMEOUT_HEADER);
    let timeout = if let Some(raw_value) = header {
        let value = raw_value.to_str().map_err(Error::InvalidChars)?;
        let parsed_value = value
            .parse::<f64>()
            .map_err(|err| Error::InvalidNumber(err, value.to_owned()))?;

        debug!(timeout = parsed_value, "found scrape timeout");
        parsed_value
    } else {
        debug!(
            timeout = default,
            "found no scrape timeout, falling back to default"
        );
        default
    };

    // subtract the timeout offset
    let result = timeout - offset;

    // check whether the duration would be positive
    if result < 0.0 {
        return Err(Error::NegativeDuration {
            original: timeout,
            result,
        });
    }

    // wrap it into a duration object
    Ok(Duration::from_secs_f64(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DEFAULT_TIMEOUT_OFFSET_SECS, DEFAULT_TIMEOUT_SECS};
    use axum::http::HeaderValue;

    #[test]
    fn get_duration_with_header() {
        let mut headers = HeaderMap::new();
        headers.insert(SCRAPE_TIMEOUT_HEADER, HeaderValue::from_static("5"));

        assert_eq!(
            get_duration(&headers, DEFAULT_TIMEOUT_SECS, DEFAULT_TIMEOUT_OFFSET_SECS).unwrap(),
            Duration::from_secs_f64(5.0 - DEFAULT_TIMEOUT_OFFSET_SECS)
        )
    }

    #[test]
    fn get_duration_with_header_decimal() {
        let mut headers = HeaderMap::new();
        headers.insert(SCRAPE_TIMEOUT_HEADER, HeaderValue::from_static("5.5"));

        assert_eq!(
            get_duration(&headers, DEFAULT_TIMEOUT_SECS, DEFAULT_TIMEOUT_OFFSET_SECS).unwrap(),
            Duration::from_secs_f64(5.5 - DEFAULT_TIMEOUT_OFFSET_SECS)
        )
    }

    #[test]
    #[should_panic]
    fn fail_get_duration_with_invalid_header_parse() {
        let mut headers = HeaderMap::new();
        headers.insert(SCRAPE_TIMEOUT_HEADER, HeaderValue::from_static("invalid"));

        get_duration(&headers, DEFAULT_TIMEOUT_SECS, DEFAULT_TIMEOUT_OFFSET_SECS).unwrap();
    }

    #[test]
    fn get_duration_without_header() {
        let headers = HeaderMap::new();

        assert_eq!(
            get_duration(&headers, DEFAULT_TIMEOUT_SECS, DEFAULT_TIMEOUT_OFFSET_SECS).unwrap(),
            Duration::from_secs_f64(DEFAULT_TIMEOUT_SECS - DEFAULT_TIMEOUT_OFFSET_SECS)
        )
    }

    #[test]
    #[should_panic]
    fn fail_get_duration_negative_original() {
        let mut headers = HeaderMap::new();
        headers.insert(SCRAPE_TIMEOUT_HEADER, HeaderValue::from_static("-1"));

        get_duration(&headers, DEFAULT_TIMEOUT_SECS, DEFAULT_TIMEOUT_OFFSET_SECS).unwrap();
    }

    #[test]
    #[should_panic]
    fn fail_get_duration_negative_result() {
        let mut headers = HeaderMap::new();
        headers.insert(SCRAPE_TIMEOUT_HEADER, HeaderValue::from_static("0"));

        get_duration(&headers, DEFAULT_TIMEOUT_SECS, DEFAULT_TIMEOUT_OFFSET_SECS).unwrap();
    }
}
