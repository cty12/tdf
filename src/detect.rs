//! Working out which image protocol to use beyond what [`ratatui_image`] detects on its own.
//!
//! `ratatui_image` decides on a protocol by probing the terminal for capabilities, and treats a
//! positive answer to the kitty graphics probe as authoritative. That's wrong for terminals which
//! answer the probe but only implement part of the protocol. iTerm2 is the main example: it says
//! yes, but then replies to image transmissions with ids we never asked for (see
//! <https://github.com/itsjunetime/tdf/issues/149>), while its own native protocol works well.
//!
//! Environment variables like `TERM_PROGRAM` can't be relied on to catch this since they don't
//! survive an ssh hop, so instead we ask the terminal itself what it is.

use core::str::FromStr;

use ratatui_image::picker::ProtocolType;

/// An image protocol the user explicitly asked for, bypassing all detection
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ForcedProtocol(pub ProtocolType);

impl FromStr for ForcedProtocol {
	type Err = String;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		Ok(Self(match s.to_ascii_lowercase().as_str() {
			"kitty" => ProtocolType::Kitty,
			"iterm2" => ProtocolType::Iterm2,
			"sixel" => ProtocolType::Sixel,
			"halfblocks" => ProtocolType::Halfblocks,
			_ =>
				return Err(format!(
					"unknown image protocol '{s}' (expected one of: kitty, iterm2, sixel, halfblocks)"
				)),
		}))
	}
}

/// What the terminal says it is when we ask it
#[derive(Clone, Copy, Debug, PartialEq)]
enum Terminal {
	Iterm2,
	Tmux,
	Other,
	/// It didn't tell us
	Unknown
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
	haystack.windows(needle.len()).any(|w| w == needle)
}

/// Works out what the terminal is from its reply to the query in [`query_terminal`]
fn identify(reply: &[u8]) -> Terminal {
	if contains(reply, b"\x1bP>|iTerm2") || contains(reply, b"\x1b[ITERM2 ") {
		Terminal::Iterm2
	} else if contains(reply, b"\x1bP>|tmux") {
		Terminal::Tmux
	} else if contains(reply, b"\x1bP>|") {
		Terminal::Other
	} else {
		Terminal::Unknown
	}
}

/// Asks the terminal on stdin/stdout to identify itself and returns whatever it replied with
#[cfg(unix)]
fn query_terminal() -> Vec<u8> {
	use core::time::Duration;
	use std::{
		io::{IsTerminal as _, Write as _},
		os::fd::AsFd as _,
		time::Instant
	};

	use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
	use nix::{
		errno::Errno,
		poll::{PollFd, PollFlags, PollTimeout, poll}
	};

	// - `CSI > q` (XTVERSION), which most modern terminals answer with `DCS > | name version ST`
	// - `CSI 1337 n`, iTerm2's own (and much older) version report, answered with
	//   `CSI ITERM2 version n`. Everything else ignores it.
	// - `CSI 5 n`, a status report which every terminal answers with `CSI 0 n`. Since replies come
	//   back in order, seeing this one means nothing more is coming and we can stop waiting.
	const QUERY: &[u8] = b"\x1b[>q\x1b[1337n\x1b[5n";
	const STATUS_OK: &[u8] = b"\x1b[0n";
	// In practice we only ever wait for one round-trip, since every terminal answers the last
	// part of the query
	const TIMEOUT: Duration = Duration::from_secs(2);

	let stdin = std::io::stdin();
	let mut stdout = std::io::stdout();
	let mut reply = Vec::with_capacity(64);

	// The reply shows up as if it were typed, so without raw mode it would be echoed to the screen
	// and held back until a newline
	if !stdin.is_terminal() || !stdout.is_terminal() || enable_raw_mode().is_err() {
		return reply;
	}

	if stdout
		.write_all(QUERY)
		.and_then(|()| stdout.flush())
		.is_ok()
	{
		// We read straight from the fd instead of through `Stdin` so that nothing gets left behind
		// in std's buffer, where `poll` couldn't see it and later readers wouldn't find it
		let fd = stdin.as_fd();
		let deadline = Instant::now() + TIMEOUT;
		let mut chunk = [0u8; 64];

		while !contains(&reply, STATUS_OK) {
			let remaining = deadline.saturating_duration_since(Instant::now());
			let Ok(timeout) = PollTimeout::try_from(remaining) else {
				break;
			};

			match poll(&mut [PollFd::new(fd, PollFlags::POLLIN)], timeout) {
				Ok(1..) => (),
				Err(Errno::EINTR) => continue,
				// Either we timed out, so the terminal isn't going to answer, or something's broken
				_ => break
			}

			match nix::unistd::read(fd, &mut chunk) {
				Ok(0) => break,
				Ok(n) => reply.extend_from_slice(&chunk[..n]),
				Err(Errno::EINTR | Errno::EAGAIN) => (),
				Err(_) => break
			}
		}
	}

	_ = disable_raw_mode();

	log::debug!(
		"terminal replied to identity query with {:?}",
		String::from_utf8_lossy(&reply)
	);

	reply
}

/// We only know how to do this with `poll`; elsewhere we just rely on the environment and
/// `--protocol`.
#[cfg(not(unix))]
fn query_terminal() -> Vec<u8> {
	Vec::new()
}

/// Whether the environment suggests we're running inside iTerm2. This is only a hint: these
/// variables are missing over ssh (unless explicitly forwarded) and can be stale inside of a
/// multiplexer session which was started from one terminal and attached to from another.
fn env_hints_iterm2() -> bool {
	let contains_iterm = |name| std::env::var(name).is_ok_and(|v| v.contains("iTerm"));

	contains_iterm("TERM_PROGRAM")
		|| contains_iterm("LC_TERMINAL")
		|| std::env::var("ITERM_SESSION_ID").is_ok_and(|v| !v.is_empty())
}

/// Whether the terminal we're running in is iTerm2, going by what it says it is.
///
/// Like [`ratatui_image::picker::Picker::from_query_stdio`], this must be called before anything
/// else starts reading terminal events, as it reads the terminal's reply directly off of stdin.
/// It also needs the terminal to not be in raw mode yet.
#[must_use]
pub fn is_iterm2() -> bool {
	let terminal = identify(&query_terminal());
	log::debug!("terminal identifies itself as {terminal:?}");

	match terminal {
		Terminal::Iterm2 => true,
		Terminal::Other => false,
		// tmux answers for itself and not for the terminal it's running in, so just like when we
		// get no answer at all, the environment is the best information we have
		Terminal::Tmux | Terminal::Unknown => env_hints_iterm2()
	}
}

/// Whether we're running inside of tmux, which only passes images on to the terminal it's
/// running in if they're wrapped up for it. This checks the same things that [`ratatui_image`]
/// does, since it doesn't tell us what it decided.
#[must_use]
pub fn in_tmux() -> bool {
	std::env::var("TERM").is_ok_and(|term| term.starts_with("tmux"))
		|| std::env::var("TERM_PROGRAM").is_ok_and(|program| program == "tmux")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn identifies_terminals() {
		assert_eq!(
			identify(b"\x1bP>|iTerm2 3.6.6\x1b\\\x1b[ITERM2 3.6.6n\x1b[0n"),
			Terminal::Iterm2
		);
		// older versions only answer their own report
		assert_eq!(identify(b"\x1b[ITERM2 3.4.0n\x1b[0n"), Terminal::Iterm2);
		assert_eq!(identify(b"\x1bP>|tmux 3.5a\x1b\\\x1b[0n"), Terminal::Tmux);
		assert_eq!(
			identify(b"\x1bP>|kitty(0.42.1)\x1b\\\x1b[0n"),
			Terminal::Other
		);
		// leading garbage (e.g. a keypress that snuck in) shouldn't matter
		assert_eq!(
			identify(b"q\x1bP>|WezTerm 20240203\x07\x1b[0n"),
			Terminal::Other
		);
		assert_eq!(identify(b"\x1b[0n"), Terminal::Unknown);
		assert_eq!(identify(b""), Terminal::Unknown);
	}

	#[test]
	fn protocol_flag_parsing() {
		let parse = |s: &str| s.parse::<ForcedProtocol>().map(|p| p.0);

		assert_eq!(parse("kitty"), Ok(ProtocolType::Kitty));
		assert_eq!(parse("iTerm2"), Ok(ProtocolType::Iterm2));
		assert_eq!(parse("sixel"), Ok(ProtocolType::Sixel));
		assert_eq!(parse("halfblocks"), Ok(ProtocolType::Halfblocks));
		assert_eq!(parse("kity").ok(), None);
	}
}
