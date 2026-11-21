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

use core::{fmt, str::FromStr, time::Duration};

use ratatui_image::picker::ProtocolType;

/// How long we're willing to wait for the terminal to tell us what it is. In practice we only
/// ever wait for one round-trip, since the query ends with a request every terminal answers.
pub const IDENTITY_TIMEOUT: Duration = Duration::from_secs(2);

/// The names accepted by `--protocol`, in the order we list them to the user
const PROTOCOL_NAMES: &str = "kitty, iterm2, sixel, halfblocks";

/// An image protocol the user explicitly asked for, bypassing all detection
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ForcedProtocol(pub ProtocolType);

#[derive(Debug, PartialEq, Eq)]
pub struct UnknownProtocol(String);

impl fmt::Display for UnknownProtocol {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"unknown image protocol '{}' (expected one of: {PROTOCOL_NAMES})",
			self.0
		)
	}
}

impl core::error::Error for UnknownProtocol {}

impl FromStr for ForcedProtocol {
	type Err = UnknownProtocol;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		Ok(Self(match s.to_ascii_lowercase().as_str() {
			"kitty" => ProtocolType::Kitty,
			"iterm2" | "iterm" => ProtocolType::Iterm2,
			"sixel" => ProtocolType::Sixel,
			"halfblocks" => ProtocolType::Halfblocks,
			_ => return Err(UnknownProtocol(s.to_owned()))
		}))
	}
}

/// What we send to get the terminal to identify itself:
/// - `CSI > q` (XTVERSION), which most modern terminals answer with `DCS > | name version ST`
/// - `CSI 1337 n`, iTerm2's own (and much older) version report, answered with
///   `CSI ITERM2 version n`. Everything else ignores it.
/// - `CSI 5 n`, a status report which every terminal answers with `CSI 0 n`. Since replies come
///   back in order, seeing this one means nothing more is coming and we can stop waiting.
const IDENTITY_QUERY: &[u8] = b"\x1b[>q\x1b[1337n\x1b[5n";

/// The reply to the `CSI 5 n` at the end of [`IDENTITY_QUERY`]
const STATUS_OK: &[u8] = b"\x1b[0n";

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	haystack.windows(needle.len()).position(|w| w == needle)
}

/// Pulls the terminal's name (and usually version) out of its reply to [`IDENTITY_QUERY`], e.g.
/// `iTerm2 3.6.6`, `kitty(0.42.1)` or `tmux 3.5a`. Returns `None` if the terminal didn't
/// identify itself.
#[must_use]
pub fn parse_terminal_identity(reply: &[u8]) -> Option<String> {
	const XTVERSION_START: &[u8] = b"\x1bP>|";
	const ITERM2_START: &[u8] = b"\x1b[ITERM2 ";

	let text_until = |start: usize, is_end: fn(&u8) -> bool| {
		let rest = &reply[start..];
		let end = rest.iter().position(is_end)?;
		let text = String::from_utf8_lossy(&rest[..end]);
		let text = text.trim();
		(!text.is_empty()).then(|| text.to_owned())
	};

	if let Some(start) = find(reply, XTVERSION_START) {
		// Terminated by ST (`ESC \`), though we accept a BEL as well just in case
		let name = text_until(start + XTVERSION_START.len(), |b| {
			matches!(b, b'\x1b' | b'\x07')
		});
		if name.is_some() {
			return name;
		}
	}

	find(reply, ITERM2_START)
		.and_then(|start| text_until(start + ITERM2_START.len(), |b| *b == b'n'))
		.map(|version| format!("iTerm2 {version}"))
}

/// Asks the terminal on stdin/stdout to identify itself and returns the name it gave, if any.
///
/// Like [`ratatui_image::picker::Picker::from_query_stdio`], this must be called before anything
/// else starts reading terminal events, as it reads the reply directly off of stdin.
#[cfg(unix)]
#[must_use]
pub fn query_terminal_identity(timeout: Duration) -> Option<String> {
	use std::{
		io::{IsTerminal as _, Write as _},
		os::fd::AsFd as _,
		time::Instant
	};

	use crossterm::terminal::{disable_raw_mode, enable_raw_mode, is_raw_mode_enabled};
	use nix::{
		errno::Errno,
		poll::{PollFd, PollFlags, PollTimeout, poll}
	};

	let stdin = std::io::stdin();
	let mut stdout = std::io::stdout();
	if !stdin.is_terminal() || !stdout.is_terminal() {
		return None;
	}

	// The reply shows up as if it were typed, so without raw mode it would be echoed to the screen
	// and held back until a newline
	struct RawModeGuard;
	impl Drop for RawModeGuard {
		fn drop(&mut self) {
			_ = disable_raw_mode();
		}
	}
	let _guard = if is_raw_mode_enabled().unwrap_or(false) {
		None
	} else {
		enable_raw_mode().ok()?;
		Some(RawModeGuard)
	};

	stdout.write_all(IDENTITY_QUERY).ok()?;
	stdout.flush().ok()?;

	// We read straight from the fd instead of through `Stdin` so that nothing gets left behind
	// in std's buffer, where `poll` couldn't see it and later readers wouldn't find it
	let fd = stdin.as_fd();
	let deadline = Instant::now() + timeout;
	let mut reply = Vec::with_capacity(64);
	let mut chunk = [0u8; 64];

	while find(&reply, STATUS_OK).is_none() {
		let remaining = deadline.saturating_duration_since(Instant::now());
		let Ok(poll_timeout) = PollTimeout::try_from(remaining) else {
			break;
		};

		match poll(&mut [PollFd::new(fd, PollFlags::POLLIN)], poll_timeout) {
			// Timed out; the terminal isn't going to answer.
			Ok(0) => break,
			Ok(_) => (),
			Err(Errno::EINTR) => continue,
			Err(_) => break
		}

		match nix::unistd::read(fd, &mut chunk) {
			Ok(0) => break,
			Ok(n) => reply.extend_from_slice(&chunk[..n]),
			Err(Errno::EINTR | Errno::EAGAIN) => (),
			Err(_) => break
		}
	}

	log::debug!(
		"terminal replied to identity query with {:?}",
		String::from_utf8_lossy(&reply)
	);

	parse_terminal_identity(&reply)
}

/// We only know how to do this with `poll`; elsewhere we just rely on the environment and
/// `--protocol`.
#[cfg(not(unix))]
#[must_use]
pub fn query_terminal_identity(_timeout: Duration) -> Option<String> {
	None
}

/// Whether the environment suggests we're running inside iTerm2. This is only a hint: these
/// variables are missing over ssh (unless explicitly forwarded) and can be stale inside of a
/// multiplexer session which was started from one terminal and attached to from another.
#[must_use]
pub fn env_hints_iterm2(var: impl Fn(&str) -> Option<String>) -> bool {
	let contains_iterm = |name| var(name).is_some_and(|v| v.contains("iTerm"));

	contains_iterm("TERM_PROGRAM")
		|| contains_iterm("LC_TERMINAL")
		|| var("ITERM_SESSION_ID").is_some_and(|v| !v.is_empty())
}

/// Whether we're running inside of tmux, which only passes images on to the terminal it's
/// running in if they're wrapped up for it. This checks the same things that [`ratatui_image`]
/// does, since it doesn't tell us what it decided.
#[must_use]
pub fn in_tmux(var: impl Fn(&str) -> Option<String>) -> bool {
	var("TERM").is_some_and(|term| term.starts_with("tmux"))
		|| var("TERM_PROGRAM").is_some_and(|program| program == "tmux")
}

/// Decides whether to swap the protocol `ratatui_image` detected for iTerm2's native one.
///
/// `identity` is what the terminal called itself (see [`query_terminal_identity`]) and is trusted
/// over the environment, unless it's a multiplexer answering on behalf of a terminal we can't
/// see, in which case the environment is the best information we have.
#[must_use]
pub fn should_use_iterm2(
	detected: ProtocolType,
	identity: Option<&str>,
	env_hints_iterm2: bool
) -> bool {
	// Halfblocks means detection didn't get far enough to know the font size, which we'd need
	// for real images; and if it's already iterm2 there's nothing to do.
	if !matches!(detected, ProtocolType::Kitty | ProtocolType::Sixel) {
		return false;
	}

	match identity.map(str::to_ascii_lowercase) {
		Some(name) if name.starts_with("tmux") => env_hints_iterm2,
		Some(name) => name.starts_with("iterm"),
		None => env_hints_iterm2
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_xtversion() {
		let id = |reply: &[u8]| parse_terminal_identity(reply);

		assert_eq!(
			id(b"\x1bP>|iTerm2 3.6.6\x1b\\\x1b[0n").as_deref(),
			Some("iTerm2 3.6.6")
		);
		assert_eq!(
			id(b"\x1bP>|kitty(0.42.1)\x1b\\\x1b[0n").as_deref(),
			Some("kitty(0.42.1)")
		);
		assert_eq!(
			id(b"\x1bP>|tmux 3.5a\x1b\\\x1b[0n").as_deref(),
			Some("tmux 3.5a")
		);
		// leading garbage (e.g. a keypress that snuck in) shouldn't matter
		assert_eq!(
			id(b"q\x1bP>|WezTerm 20240203\x07\x1b[0n").as_deref(),
			Some("WezTerm 20240203")
		);
	}

	#[test]
	fn parses_iterm2_proprietary_report() {
		// only the proprietary report was answered
		assert_eq!(
			parse_terminal_identity(b"\x1b[ITERM2 3.5.0n\x1b[0n").as_deref(),
			Some("iTerm2 3.5.0")
		);
		// both answered; they agree, and we take the XTVERSION one
		assert_eq!(
			parse_terminal_identity(b"\x1bP>|iTerm2 3.6.6\x1b\\\x1b[ITERM2 3.6.6n\x1b[0n")
				.as_deref(),
			Some("iTerm2 3.6.6")
		);
	}

	#[test]
	fn no_identity_when_terminal_does_not_say() {
		assert_eq!(parse_terminal_identity(b"\x1b[0n"), None);
		assert_eq!(parse_terminal_identity(b""), None);
		// unterminated or empty reports aren't identities
		assert_eq!(parse_terminal_identity(b"\x1bP>|iTerm2 3.6"), None);
		assert_eq!(parse_terminal_identity(b"\x1bP>|\x1b\\\x1b[0n"), None);
	}

	#[test]
	fn iterm2_replaces_kitty_and_sixel_only() {
		let iterm = Some("iTerm2 3.6.6");

		assert!(should_use_iterm2(ProtocolType::Kitty, iterm, false));
		assert!(should_use_iterm2(ProtocolType::Sixel, iterm, false));
		assert!(!should_use_iterm2(ProtocolType::Halfblocks, iterm, true));
		assert!(!should_use_iterm2(ProtocolType::Iterm2, iterm, true));
	}

	#[test]
	fn identity_beats_environment() {
		// a real kitty with stale iTerm2 variables in the environment stays on kitty
		assert!(!should_use_iterm2(
			ProtocolType::Kitty,
			Some("kitty(0.42.1)"),
			true
		));
		// nothing in the environment over ssh, but the terminal says it's iTerm2
		assert!(should_use_iterm2(
			ProtocolType::Kitty,
			Some("iTerm2 3.6.6"),
			false
		));
	}

	#[test]
	fn environment_used_when_identity_unavailable() {
		assert!(should_use_iterm2(ProtocolType::Kitty, None, true));
		assert!(!should_use_iterm2(ProtocolType::Kitty, None, false));
		// tmux answers for itself, not for the terminal it's running in
		assert!(should_use_iterm2(
			ProtocolType::Kitty,
			Some("tmux 3.5a"),
			true
		));
		assert!(!should_use_iterm2(
			ProtocolType::Kitty,
			Some("tmux 3.5a"),
			false
		));
	}

	#[test]
	fn environment_hints() {
		let env = |pairs: &'static [(&'static str, &'static str)]| {
			move |name: &str| {
				pairs
					.iter()
					.find(|(k, _)| *k == name)
					.map(|(_, v)| (*v).to_owned())
			}
		};

		assert!(env_hints_iterm2(env(&[("TERM_PROGRAM", "iTerm.app")])));
		assert!(env_hints_iterm2(env(&[("LC_TERMINAL", "iTerm2")])));
		assert!(env_hints_iterm2(env(&[(
			"ITERM_SESSION_ID",
			"w0t0p0:1234"
		)])));
		assert!(!env_hints_iterm2(env(&[
			("TERM_PROGRAM", "tmux"),
			("TERM", "xterm-256color")
		])));
		assert!(!env_hints_iterm2(env(&[("ITERM_SESSION_ID", "")])));

		assert!(in_tmux(env(&[("TERM", "tmux-256color")])));
		assert!(in_tmux(env(&[
			("TERM", "xterm-256color"),
			("TERM_PROGRAM", "tmux")
		])));
		assert!(!in_tmux(env(&[
			("TERM", "xterm-256color"),
			("TERM_PROGRAM", "iTerm.app")
		])));
	}

	#[test]
	fn protocol_flag_parsing() {
		let parse = |s: &str| s.parse::<ForcedProtocol>().map(|p| p.0);

		assert_eq!(parse("kitty"), Ok(ProtocolType::Kitty));
		assert_eq!(parse("iterm2"), Ok(ProtocolType::Iterm2));
		assert_eq!(parse("iTerm2"), Ok(ProtocolType::Iterm2));
		assert_eq!(parse("sixel"), Ok(ProtocolType::Sixel));
		assert_eq!(parse("halfblocks"), Ok(ProtocolType::Halfblocks));
		assert_eq!(parse("kity"), Err(UnknownProtocol("kity".to_owned())));
	}
}
