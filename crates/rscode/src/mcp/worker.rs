//! Running each tool call on a fresh thread.
//!
//! `proc_macro2` (which `syn` parses with) keeps span locations in a thread-local source map that only ever grows.
//! A long-running server that parsed on its own threads would therefore leak every file it ever parsed. A thread
//! that exits frees its source map, so every call runs on a new thread, and the async side awaits its result.

use futures::channel::oneshot;
use std::any::Any;
use std::panic::AssertUnwindSafe;

/// Stack size of the worker threads: parsing (and dropping) deeply nested code recurses deeply, and a stack
/// overflow would abort the whole server. Only address space is reserved up front.
const STACK_SIZE: usize = 64 * 1024 * 1024;

/// The message of a panic payload (`panic!` with a message makes a `&str` or a `String`).
fn panic_message(payload: &(dyn Any + Send)) -> &str {
	if let Some(message) = payload.downcast_ref::<&str>() {
		return message;
	}

	match payload.downcast_ref::<String>() {
		Some(message) => message,
		None => "a panic without a message",
	}
}

/// Runs `job` on a new thread named `rscode-<name>` and waits for its result without blocking the async runtime.
///
/// A panicking job is reported as an error message rather than taking the server down.
pub(crate) async fn run<T: Send + 'static>(name: &str, job: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
	let (sender, receiver) = oneshot::channel();

	std::thread::Builder::new()
		.name(format!("rscode-{name}"))
		.stack_size(STACK_SIZE)
		.spawn(move || {
			let result = std::panic::catch_unwind(AssertUnwindSafe(job));

			// the receiver is gone when the request was dropped: nobody wants the result anymore
			let _ = sender.send(result);
		})
		.map_err(|error| format!("internal error: failed to start a worker thread: {error}"))?;

	match receiver.await {
		Ok(Ok(value)) => Ok(value),
		Ok(Err(payload)) => Err(format!("internal error: {}", panic_message(payload.as_ref()))),
		Err(oneshot::Canceled) => Err("internal error: the worker thread stopped without a result".to_owned()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn deep_recursion_fits_on_the_stack() {
		fn depth(n: u32) -> u32 {
			let padding = [n; 64];

			match n {
				0 => padding[0],
				n => 1 + depth(n - 1) + padding[63] - n,
			}
		}

		// more than the default 2 MiB of a spawned thread
		assert_eq!(run("deep", || depth(20_000)).await.unwrap(), 20_000);
	}

	#[tokio::test]
	async fn reports_panics_as_errors() {
		let message = run("panic", || -> u8 { panic!("boom") }).await.unwrap_err();

		assert_eq!(message, "internal error: boom");

		let message = run("panic", || -> u8 { panic!("{} {}", "formatted", 1) }).await.unwrap_err();

		assert_eq!(message, "internal error: formatted 1");

		let message = run("panic", || -> u8 { std::panic::panic_any(7_u32) }).await.unwrap_err();

		assert_eq!(message, "internal error: a panic without a message");
	}

	#[tokio::test]
	async fn runs_on_a_fresh_named_thread() {
		let caller = std::thread::current().id();
		let (id, name) = run("probe", || {
			let thread = std::thread::current();

			(thread.id(), thread.name().map(str::to_owned))
		})
		.await
		.unwrap();

		assert_ne!(id, caller);
		assert_eq!(name.as_deref(), Some("rscode-probe"));

		// every call gets its own thread
		let second = run("probe", || std::thread::current().id()).await.unwrap();

		assert_ne!(second, id);
	}
}
