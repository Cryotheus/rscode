//! `mcp`: the Model Context Protocol server, on stdin and stdout.
//!
//! Nothing but the protocol may be written to stdout. The tokio runtime is only built here, after
//! [`CompleteEnv`](clap_complete::CompleteEnv) had its chance to (unsafely) edit the environment while the
//! process still had a single thread.

use crate::args;
use clap::ArgMatches;
use std::io::ErrorKind;
use std::process::ExitCode;

/// Whether an error of the server comes from writing to a closed pipe (stdout, the transport).
fn is_broken_pipe(error: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(Some(error), |error| error.source()).any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == ErrorKind::BrokenPipe)
    })
}

pub(super) fn run(matches: &ArgMatches) -> anyhow::Result<ExitCode> {
    let options = args::server_options(matches)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    match runtime.block_on(rscode::mcp::serve_stdio(options)) {
        // the client left while being answered: the session is over, and nobody is left to tell
        Err(error) if is_broken_pipe(&error) => Ok(ExitCode::SUCCESS),

        result => result.map(|()| ExitCode::SUCCESS).map_err(Into::into),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An error caused by an I/O error.
    #[derive(Debug)]
    struct Wrapper(std::io::Error);

    impl std::fmt::Display for Wrapper {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("serving failed")
        }
    }

    impl std::error::Error for Wrapper {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn recognizes_a_closed_transport() {
        assert!(is_broken_pipe(&std::io::Error::from(ErrorKind::BrokenPipe)));
        assert!(is_broken_pipe(&Wrapper(ErrorKind::BrokenPipe.into())));
        assert!(!is_broken_pipe(&Wrapper(ErrorKind::NotFound.into())));
        assert!(!is_broken_pipe(&std::fmt::Error));
    }
}
