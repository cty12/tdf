//! Encoding pages for iTerm2's inline image protocol.
//!
//! [`ratatui_image`] can do this as well, but not in a way that works in iTerm2 on a high-dpi
//! display. iTerm2 talks about sizes in two different units there: the pixel size of the window
//! that we get from the tty, and that pages are rendered to fit, is in device pixels, while the
//! cell size that it reports when queried is in points, of which there are only half as many on
//! a 2x display. [`ratatui_image`] works with the latter, so:
//!
//! - It crops images to the cells they should cover using a cell size that's half of the one
//!   the page was rendered with, which leaves only the top left quarter of the page.
//! - It tells the terminal how large to draw the image in pixels, which iTerm2 takes to mean
//!   either points or device pixels depending on how it's configured.
//!
//! So we crop with the same cell size that the page was rendered with instead, and then tell the
//! terminal how many cells the image should fill. That means the same thing no matter how the
//! display is scaled, and it gets us pages at the full resolution of the display.

use core::fmt::Write as _;
use std::io::Cursor;

use image::{ImageFormat, ImageResult, RgbImage, imageops};
use ratatui::layout::Size;
use ratatui_image::{
	FontSize,
	protocol::{Protocol, iterm2::Iterm2}
};

/// Turns rendered pages into something that iTerm2 (or another terminal which implements its
/// image protocol) can display
#[derive(Clone, Copy, Debug)]
pub struct Encoder {
	/// How many pixels of a rendered page go into each cell. This needs to be what the renderer
	/// is using, not what the terminal says its cell size is.
	pub cell_px: FontSize,
	/// Whether we need to wrap things up so that tmux passes them on to the terminal
	pub in_tmux: bool
}

impl Encoder {
	/// Wraps `img` up so that [`ratatui_image`]'s widgets draw it over exactly `cells`
	///
	/// # Errors
	///
	/// When `img` can't be encoded as a png
	pub fn protocol(&self, img: RgbImage, cells: Size) -> ImageResult<Protocol> {
		let img = crop_to_cells(img, cells, self.cell_px);
		let data = encode(&img, cells, self.in_tmux)?;

		Ok(Protocol::ITerm2(Iterm2 {
			data,
			size: cells,
			is_tmux: self.in_tmux
		}))
	}
}

/// Cuts `img` down to the part of it that fills `cells` when every cell holds `cell_px` of its
/// pixels. The renderer rounds the amount of cells that a page covers down, so there's usually a
/// sliver at the right or the bottom of it that doesn't fill a whole cell anymore.
#[must_use]
pub fn crop_to_cells(img: RgbImage, cells: Size, cell_px: FontSize) -> RgbImage {
	// If something ends up being smaller than a single cell, we still want an image with
	// something in it, since nothing can encode an empty one
	let width = (u32::from(cells.width.max(1)) * u32::from(cell_px.width)).min(img.width());
	let height = (u32::from(cells.height.max(1)) * u32::from(cell_px.height)).min(img.height());

	if (width, height) == img.dimensions() {
		return img;
	}

	imageops::crop_imm(&img, 0, 0, width, height).to_image()
}

/// Builds the escape sequence which makes the terminal stretch `img` over exactly `cells`,
/// starting at the cell that the cursor is in.
///
/// # Errors
///
/// When `img` can't be encoded as a png
pub fn encode(img: &RgbImage, cells: Size, in_tmux: bool) -> ImageResult<String> {
	let mut png = Vec::new();
	img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;

	// tmux only passes this on to the terminal it's running in if it's wrapped up like this, with
	// every escape inside of it doubled
	let (start, esc, end) = if in_tmux {
		("\x1bPtmux;", "\x1b\x1b", "\x1b\\")
	} else {
		("", "\x1b", "")
	};

	let Size { width, height } = cells;

	let mut seq = String::with_capacity(png.len().div_ceil(3) * 4 + usize::from(height) * 16 + 128);
	seq.push_str(start);

	// Erase whatever these cells were showing before, and then go back to where we started. We
	// only move down between rows (not after the last one) so that this still ends up in the
	// right place when the image reaches the bottom of the screen, where moving down does nothing.
	for row in 0..height {
		if row > 0 {
			write!(seq, "{esc}[1B").unwrap();
		}
		write!(seq, "{esc}[{width}X").unwrap();
	}
	if height > 1 {
		write!(seq, "{esc}[{}A", height - 1).unwrap();
	}

	// A width and height without a unit are in cells. `doNotMoveCursor` isn't something that
	// iTerm2 itself knows about, but some of the other terminals which implement this do.
	write!(
		seq,
		"{esc}]1337;File=inline=1;size={};width={width};height={height};preserveAspectRatio=0;doNotMoveCursor=1:",
		png.len()
	)
	.unwrap();
	base64_simd::STANDARD.encode_append(&png, &mut seq);
	seq.push('\x07');
	seq.push_str(end);

	Ok(seq)
}

#[cfg(test)]
mod tests {
	use image::Rgb;

	use super::*;

	const RETINA_CELL: FontSize = FontSize {
		width: 16,
		height: 32
	};

	fn page(width: u32, height: u32) -> RgbImage {
		RgbImage::from_pixel(width, height, Rgb([255, 255, 255]))
	}

	/// Pulls the png back out of an escape sequence and gets its dimensions
	fn png_dimensions(seq: &str) -> (u32, u32) {
		let (_, rest) = seq.split_once(':').unwrap();
		let (payload, _) = rest.split_once('\x07').unwrap();
		let png = base64_simd::STANDARD.decode_to_vec(payload).unwrap();

		image::load_from_memory_with_format(&png, ImageFormat::Png)
			.unwrap()
			.into_rgb8()
			.dimensions()
	}

	#[test]
	fn crops_to_whole_cells() {
		// This is what a US letter page comes out as when it's fit into 32 rows of retina cells:
		// 49.5 cells wide, which the renderer rounds down to 49.
		let cropped = crop_to_cells(page(792, 1024), Size::new(49, 32), RETINA_CELL);
		assert_eq!(cropped.dimensions(), (784, 1024));
	}

	#[test]
	fn keeps_whole_page_at_the_cell_size_it_was_rendered_with() {
		// The bug this module exists for: cropping with the cell size in points (8x16) would
		// leave us with 392x512, the top left quarter of the page.
		let cropped = crop_to_cells(page(784, 1024), Size::new(49, 32), RETINA_CELL);
		assert_eq!(cropped.dimensions(), (784, 1024));
	}

	#[test]
	fn never_crops_to_more_than_there_is_or_to_nothing() {
		let cropped = crop_to_cells(page(100, 100), Size::new(49, 32), RETINA_CELL);
		assert_eq!(cropped.dimensions(), (100, 100));

		let cropped = crop_to_cells(page(10, 100), Size::new(0, 2), RETINA_CELL);
		assert_eq!(cropped.dimensions(), (10, 64));
	}

	#[test]
	fn sizes_image_in_cells() {
		let seq = encode(&page(784, 1024), Size::new(49, 32), false).unwrap();

		assert!(seq.contains(";width=49;height=32;preserveAspectRatio=0;"));
		assert!(!seq.contains("px"));
		assert!(seq.ends_with('\x07'));
		assert_eq!(png_dimensions(&seq), (784, 1024));
	}

	#[test]
	fn erases_cells_without_leaving_them() {
		let seq = encode(&page(16, 96), Size::new(1, 3), false).unwrap();
		let erase = "\x1b[1X\x1b[1B\x1b[1X\x1b[1B\x1b[1X\x1b[2A\x1b]1337;";
		assert!(seq.starts_with(erase));

		// With a single row, there's no reason to move at all
		let seq = encode(&page(64, 32), Size::new(4, 1), false).unwrap();
		assert!(seq.starts_with("\x1b[4X\x1b]1337;"));
	}

	#[test]
	fn wraps_for_tmux() {
		let seq = encode(&page(32, 32), Size::new(2, 1), true).unwrap();

		assert!(seq.starts_with("\x1bPtmux;\x1b\x1b[2X\x1b\x1b]1337;File=inline=1;"));
		assert!(seq.ends_with("\x07\x1b\\"));
	}

	#[test]
	fn protocol_covers_the_cells_it_was_given() {
		let encoder = Encoder {
			cell_px: RETINA_CELL,
			in_tmux: false
		};

		let Protocol::ITerm2(protocol) = encoder
			.protocol(page(792, 1024), Size::new(49, 32))
			.unwrap()
		else {
			panic!("the iterm2 encoder made something for another protocol");
		};

		assert_eq!(protocol.size, Size::new(49, 32));
		assert_eq!(png_dimensions(&protocol.data), (784, 1024));
	}
}
