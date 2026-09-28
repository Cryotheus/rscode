//! Reporting input that is not JSON.
//!
//! `rmcp` ignores lines of input that are not JSON: there is no request id to answer, and answering could make a
//! peer that echoes errors back loop forever (the other official MCP SDKs ignore them too). A client whose request
//! got mangled would then wait for an answer without any clue why, so such lines are reported (on stderr), while the
//! input passes through unchanged.

use serde::de::IgnoredAny;
use std::io;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use tokio::io::AsyncRead;
use tokio::io::ReadBuf;

/// Lines are checked up to this length; longer lines are not buffered (their syntax errors go unreported).
const MAX_CHECKED_LINE: usize = 64 * 1024 * 1024;

/// At most this many characters of a line are quoted in a report.
const QUOTED_CHARS: usize = 200;

/// Input that reports the lines that `rmcp` ignores because they are not JSON.
pub(crate) struct CheckedInput<R> {
	inner: R,

	/// The current line so far (unless it is too long to check).
	line: Vec<u8>,

	/// Whether the current line is too long to check.
	too_long: bool,

	report: Box<dyn FnMut(String) + Send>,
}

impl<R> CheckedInput<R> {
	/// Passes `inner` through, calling `report` with a message for every line that is not JSON.
	pub(crate) fn new(inner: R, report: impl FnMut(String) + Send + 'static) -> Self {
		Self { inner, line: Vec::new(), too_long: false, report: Box::new(report) }
	}

	/// Checks every line that the bytes complete.
	fn observe(&mut self, bytes: &[u8]) {
		for piece in bytes.split_inclusive(|&byte| byte == b'\n') {
			if !self.too_long && self.line.len() + piece.len() <= MAX_CHECKED_LINE {
				self.line.extend_from_slice(piece);
			} else {
				self.too_long = true;
				self.line.clear();
			}

			if piece.ends_with(b"\n") {
				if !self.too_long
					&& let Some(message) = check(&self.line)
				{
					(self.report)(message);
				}

				self.line.clear();
				self.too_long = false;
			}
		}
	}
}

impl<R: AsyncRead + Unpin> AsyncRead for CheckedInput<R> {
	fn poll_read(mut self: Pin<&mut Self>, context: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
		let before = buf.filled().len();
		let poll = Pin::new(&mut self.inner).poll_read(context, buf);

		if let Poll::Ready(Ok(())) = poll {
			self.observe(&buf.filled()[before..]);
		}

		poll
	}
}

/// A report for a line (with its line break) that `rmcp` ignores because it is not JSON, like `rmcp` reads lines:
/// without a trailing `\r`, and with a leading byte order mark ignored.
fn check(line: &[u8]) -> Option<String> {
	let line = line.strip_suffix(b"\n").unwrap_or(line);
	let line = line.strip_suffix(b"\r").unwrap_or(line);
	let line = line.strip_prefix("\u{feff}".as_bytes()).unwrap_or(line);

	if line.is_empty() {
		return None;
	}

	let error = serde_json::from_slice::<IgnoredAny>(line).err()?;

	if !(error.is_syntax() || error.is_eof()) {
		return None;
	}

	let text = String::from_utf8_lossy(line);
	let mut quoted: String = text.chars().take(QUOTED_CHARS).collect();

	if quoted.len() < text.len() {
		quoted.push_str("...");
	}

	Some(format!("rscode mcp: ignored a line of input that is not JSON ({error}), so nothing answers it: {quoted}"))
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::collections::VecDeque;
	use std::sync::Arc;
	use std::sync::Mutex;
	use tokio::io::AsyncReadExt;

	/// A reader returning (at most) one chunk per read.
	struct Chunks(VecDeque<Vec<u8>>);

	impl AsyncRead for Chunks {
		fn poll_read(mut self: Pin<&mut Self>, _: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
			if let Some(mut chunk) = self.0.pop_front() {
				let len = chunk.len().min(buf.remaining());

				buf.put_slice(&chunk[..len]);

				if len < chunk.len() {
					self.0.push_front(chunk.split_off(len));
				}
			}

			Poll::Ready(Ok(()))
		}
	}

	/// Reads `chunks` through a [`CheckedInput`]: what comes out, and the reports.
	async fn read(chunks: &[&[u8]]) -> (Vec<u8>, Vec<String>) {
		let chunks = Chunks(chunks.iter().map(|chunk| chunk.to_vec()).collect());
		let reports = Arc::new(Mutex::new(Vec::new()));
		let sink = reports.clone();
		let mut input = CheckedInput::new(chunks, move |message| sink.lock().unwrap().push(message));
		let mut output = Vec::new();

		input.read_to_end(&mut output).await.unwrap();

		let reports = reports.lock().unwrap().clone();

		(output, reports)
	}

	#[tokio::test]
	async fn reports_lines_that_are_not_json() {
		let input: &[&[u8]] =
			&[b"{\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n{this is not", b" json\n\n\r\n[1, 2]\r\n\xef\xbb\xbf{}\n"];
		let (output, reports) = read(input).await;

		assert_eq!(output, input.concat());
		assert_eq!(reports.len(), 1, "{reports:?}");
		assert!(reports[0].starts_with("rscode mcp: ignored a line of input that is not JSON (key must be a string"));
		assert!(reports[0].ends_with("so nothing answers it: {this is not json"), "{}", reports[0]);
	}

	#[tokio::test]
	async fn quotes_long_lines_in_part() {
		let line = format!("{}\n", "x".repeat(1000));
		let (_, reports) = read(&[line.as_bytes()]).await;

		assert_eq!(reports.len(), 1);
		assert!(reports[0].ends_with(&format!("{}...", "x".repeat(QUOTED_CHARS))), "{}", reports[0]);
	}

	#[tokio::test]
	async fn ignores_an_unfinished_last_line() {
		let (output, reports) = read(&[b"{\"a\":", b" 1}\n{\"b\""]).await;

		assert_eq!(output, b"{\"a\": 1}\n{\"b\"");
		assert!(reports.is_empty(), "{reports:?}");
	}
}
