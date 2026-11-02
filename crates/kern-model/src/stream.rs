//! Shared line framing for provider event streams (ARCHITECTURE.md §29).
//!
//! Every streaming adapter faces the same three problems: chunks arrive split
//! on arbitrary byte boundaries (including mid UTF-8 character), a server may
//! omit the trailing newline, and the body must be size-bounded. `LineReader`
//! solves them once so each adapter only translates its own event shapes.

use crate::error::ModelError;

/// Guard against unbounded provider streams.
pub const MAX_STREAM_BYTES: u64 = 10 * 1024 * 1024;

/// Reads a provider's response body line by line.
///
/// Lines are decoded only once complete, so a chunk boundary in the middle of a
/// multi-byte character is harmless. A final unterminated line is still
/// delivered, since some servers end the body without a newline.
pub struct LineReader {
    response: reqwest::Response,
    buffer: Vec<u8>,
    /// Bytes consumed so far, for the size bound.
    total: usize,
    provider: &'static str,
    limit: u64,
}

impl LineReader {
    pub fn new(response: reqwest::Response, provider: &'static str) -> Self {
        Self {
            response,
            buffer: Vec::new(),
            total: 0,
            provider,
            limit: MAX_STREAM_BYTES,
        }
    }

    /// Test knob: a tighter byte budget.
    pub fn with_limit(mut self, limit: u64) -> Self {
        self.limit = limit;
        self
    }

    /// The next line without its terminator, or `None` at end of body.
    pub async fn next_line(&mut self) -> Result<Option<String>, ModelError> {
        loop {
            if let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
                let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                line.pop(); // the newline
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }

            match self.response.chunk().await.map_err(|e| {
                ModelError::Unavailable(format!("{} stream transport error: {e}", self.provider))
            })? {
                Some(chunk) => {
                    self.total += chunk.len();
                    if self.total as u64 > self.limit {
                        return Err(ModelError::InvalidResponse(format!(
                            "{} stream exceeds {} bytes",
                            self.provider, self.limit
                        )));
                    }
                    self.buffer.extend_from_slice(&chunk);
                }
                None => {
                    // Body ended: flush an unterminated final line once.
                    if self.buffer.is_empty() {
                        return Ok(None);
                    }
                    let mut line: Vec<u8> = std::mem::take(&mut self.buffer);
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serve `body` once and return the live response stream.
    async fn response(body: &'static str) -> reqwest::Response {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/stream")
            .with_status(200)
            .with_body(body)
            .create();
        reqwest::get(format!("{}/stream", server.url()))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn yields_lines_and_flushes_an_unterminated_tail() {
        let mut reader = LineReader::new(response("a\nb\r\nc").await, "test");
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("a"));
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("b"));
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("c"));
        assert!(reader.next_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_empty_body_yields_no_lines() {
        let mut reader = LineReader::new(response("").await, "test");
        assert!(reader.next_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oversized_stream_is_rejected() {
        let mut reader = LineReader::new(response("0123456789\n").await, "test").with_limit(4);
        let err = reader.next_line().await.unwrap_err();
        assert!(matches!(err, ModelError::InvalidResponse(_)), "{err:?}");
    }
}
