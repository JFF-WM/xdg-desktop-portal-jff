//! The parts of the portal that need no Wayland, D-Bus or PipeWire: what can be shared, the
//! picker's text protocol, and the arithmetic of a frame.
//!
//! Nothing here depends on another crate, so it can be tested on its own.

/// `types` bits of the ScreenCast portal.
pub const TYPE_MONITOR: u32 = 1;
pub const TYPE_WINDOW: u32 = 2;
/// Both kinds jff-portal can capture (`AvailableSourceTypes`).
pub const AVAILABLE_TYPES: u32 = TYPE_MONITOR | TYPE_WINDOW;

/// `cursor_mode` bits of the ScreenCast portal. Embedded (2) is not offered: a window picture
/// never has a cursor in it, and the client draws the one from the metadata.
pub const CURSOR_HIDDEN: u32 = 1;
pub const CURSOR_METADATA: u32 = 4;
/// Both modes jff-portal can serve (`AvailableCursorModes`).
pub const AVAILABLE_CURSOR_MODES: u32 = CURSOR_HIDDEN | CURSOR_METADATA;

/// Whether `mode` is exactly one of the advertised cursor modes.
pub fn cursor_mode_offered(mode: u32) -> bool {
    mode.count_ones() == 1 && mode & AVAILABLE_CURSOR_MODES != 0
}

/// Something that can be shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Monitor {
        name: String,
    },
    Window {
        /// `ext-foreign-toplevel-list` identifier (`jff-<window id>`).
        identifier: String,
        title: String,
        app_id: String,
    },
}

impl Source {
    /// The portal `types` bit of this kind of source.
    pub fn type_bit(&self) -> u32 {
        match self {
            Source::Monitor { .. } => TYPE_MONITOR,
            Source::Window { .. } => TYPE_WINDOW,
        }
    }

    /// The `source_type` value reported for a stream (same numbers as the `types` bits).
    pub fn source_type(&self) -> u32 {
        self.type_bit()
    }

    fn describe(&self) -> String {
        match self {
            Source::Monitor { name } => format!("monitor: {}", clean(name)),
            Source::Window { title, app_id, .. } => {
                let title = clean(title);
                let app_id = clean(app_id);
                match (title.is_empty(), app_id.is_empty()) {
                    (false, false) => format!("window: {title} ({app_id})"),
                    (false, true) => format!("window: {title}"),
                    (true, false) => format!("window: ({app_id})"),
                    (true, true) => "window: (untitled)".to_string(),
                }
            }
        }
    }
}

/// A title is client text: no line breaks (they would split a picker line in two) and no other
/// control characters.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// The `types` a request means: bit 0 and 1 only (virtual monitors, bit 2, aren't offered), and
/// a request that names none means monitors, as the portal documents.
pub fn effective_types(requested: Option<u32>) -> u32 {
    match requested {
        None | Some(0) => TYPE_MONITOR,
        Some(bits) => bits & AVAILABLE_TYPES,
    }
}

/// What the picker is shown: the sources of the wanted `types`, each as `(index into
/// `sources`, line)`. The line starts with `[index]`, so the choice maps back to a source by
/// number and never by title.
pub fn picker_entries(sources: &[Source], types: u32) -> Vec<(usize, String)> {
    sources
        .iter()
        .enumerate()
        .filter(|(_, s)| s.type_bit() & types != 0)
        .map(|(i, s)| (i, format!("[{i}] {}", s.describe())))
        .collect()
}

/// Why a picker answer was not accepted.
#[derive(Debug, PartialEq, Eq)]
pub enum ChoiceError {
    /// Nothing chosen: the user cancelled.
    Cancelled,
    /// More than one line while only one source may be picked.
    TooMany,
    /// A line that is none of the lines offered.
    Unknown(String),
    /// The same source twice.
    Duplicate(usize),
}

/// Turn what the picker printed into indexes into `sources`. Every line has to be exactly one
/// of the `offered` lines: an answer the picker invented or altered is rejected, never passed on.
pub fn parse_choice(
    output: &str,
    offered: &[(usize, String)],
    multiple: bool,
) -> Result<Vec<usize>, ChoiceError> {
    let mut chosen: Vec<usize> = Vec::new();
    for line in output.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Some((index, _)) = offered.iter().find(|(_, text)| text == line) else {
            return Err(ChoiceError::Unknown(line.to_string()));
        };
        if chosen.contains(index) {
            return Err(ChoiceError::Duplicate(*index));
        }
        chosen.push(*index);
    }
    match chosen.len() {
        0 => Err(ChoiceError::Cancelled),
        1 => Ok(chosen),
        _ if multiple => Ok(chosen),
        _ => Err(ChoiceError::TooMany),
    }
}

/// A pixel layout a frame can have in memory. Both are 4 bytes a pixel, B first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// `wl_shm` `Argb8888`: blue, green, red, alpha.
    Bgra,
    /// `wl_shm` `Xrgb8888`: blue, green, red, unused.
    Bgrx,
}

/// `wl_shm` format codes (`wl_shm.format`).
pub const SHM_ARGB8888: u32 = 0;
pub const SHM_XRGB8888: u32 = 1;

/// The format to ask jff for out of those it offered: `Argb8888` (windows have an alpha
/// channel), else `Xrgb8888`. Nothing else: the 10-bit formats are not understood here.
pub fn pick_shm_format(offered: &[u32]) -> Option<(u32, PixelFormat)> {
    if offered.contains(&SHM_ARGB8888) {
        Some((SHM_ARGB8888, PixelFormat::Bgra))
    } else if offered.contains(&SHM_XRGB8888) {
        Some((SHM_XRGB8888, PixelFormat::Bgrx))
    } else {
        None
    }
}

/// Largest side accepted for a source, so a bogus size can't ask for gigabytes.
pub const MAX_SIDE: u32 = 16384;

/// Whether `width` by `height` is a size to allocate for.
pub fn valid_size(width: u32, height: u32) -> bool {
    width > 0 && height > 0 && width <= MAX_SIDE && height <= MAX_SIDE
}

/// Bytes in a row of `width` pixels.
pub fn stride(width: u32) -> u32 {
    width * 4
}

/// Bytes in a whole `width` by `height` picture.
pub fn frame_len(width: u32, height: u32) -> usize {
    stride(width) as usize * height as usize
}

/// Copy `rows` rows of `row_bytes` bytes from `src` (rows `src_stride` apart) into `dst` (rows
/// `dst_stride` apart). Rows that would fall outside either slice are skipped. Returns the number
/// of rows copied.
pub fn copy_rows(
    src: &[u8],
    src_stride: usize,
    dst: &mut [u8],
    dst_stride: usize,
    row_bytes: usize,
    rows: usize,
) -> usize {
    if src_stride < row_bytes || dst_stride < row_bytes || row_bytes == 0 {
        return 0;
    }
    let mut done = 0;
    for y in 0..rows {
        let s = y * src_stride;
        let d = y * dst_stride;
        let (Some(from), Some(to)) = (src.get(s..s + row_bytes), dst.get_mut(d..d + row_bytes))
        else {
            break;
        };
        to.copy_from_slice(from);
        done += 1;
    }
    done
}

/// Set the fourth byte of each of the first `rows` rows of `width` 4-byte pixels to 0xff.
/// Rows that would fall outside `buf` are skipped.
pub fn force_opaque(buf: &mut [u8], stride: usize, width: usize, rows: usize) {
    if stride < width * 4 {
        return;
    }
    for y in 0..rows {
        let Some(row) = buf.get_mut(y * stride..y * stride + width * 4) else {
            break;
        };
        for px in row.chunks_exact_mut(4) {
            px[3] = 0xff;
        }
    }
}

/// Debug only: what a picture holds, sampled (about 4096 pixels). Tells a black picture
/// (rgb all 0) from a transparent one (alpha all 0).
pub fn pixel_stats(buf: &[u8], stride: usize, width: usize, rows: usize) -> String {
    if stride < width * 4 || width == 0 || rows == 0 {
        return "empty/invalid".into();
    }
    let step_y = (rows / 64).max(1);
    let step_x = (width / 64).max(1);
    let (mut n, mut rgb_nonzero, mut a0, mut a255, mut amax) = (0u32, 0u32, 0u32, 0u32, 0u8);
    let mut first = [0u8; 4];
    let mut mid = [0u8; 4];
    let mut y = 0;
    while y < rows {
        let mut x = 0;
        while x < width {
            let o = y * stride + x * 4;
            if let Some(px) = buf.get(o..o + 4) {
                if n == 0 {
                    first.copy_from_slice(px);
                }
                if y >= rows / 2 && mid == [0; 4] {
                    mid.copy_from_slice(px);
                }
                n += 1;
                if px[0] | px[1] | px[2] != 0 {
                    rgb_nonzero += 1;
                }
                if px[3] == 0 {
                    a0 += 1;
                }
                if px[3] == 255 {
                    a255 += 1;
                }
                amax = amax.max(px[3]);
            }
            x += step_x;
        }
        y += step_y;
    }
    format!(
        "sampled={n} rgb_nonzero={rgb_nonzero} alpha0={a0} alpha255={a255} alpha_max={amax} first_px(BGRA)={first:?} mid_px(BGRA)={mid:?}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: &str, title: &str, app: &str) -> Source {
        Source::Window {
            identifier: id.into(),
            title: title.into(),
            app_id: app.into(),
        }
    }

    fn mon(name: &str) -> Source {
        Source::Monitor { name: name.into() }
    }

    #[test]
    fn only_hidden_and_metadata_are_offered() {
        assert!(cursor_mode_offered(CURSOR_HIDDEN));
        assert!(cursor_mode_offered(CURSOR_METADATA));
        // Embedded is not served, and a mode is one bit, not a set.
        assert!(!cursor_mode_offered(2));
        assert!(!cursor_mode_offered(5));
        assert!(!cursor_mode_offered(0));
    }

    #[test]
    fn no_types_means_monitors() {
        assert_eq!(effective_types(None), TYPE_MONITOR);
        assert_eq!(effective_types(Some(0)), TYPE_MONITOR);
    }

    #[test]
    fn virtual_monitors_are_not_offered() {
        assert_eq!(effective_types(Some(7)), 3);
        assert_eq!(effective_types(Some(4)), 0);
        assert_eq!(effective_types(Some(2)), TYPE_WINDOW);
    }

    #[test]
    fn entries_follow_the_types_and_keep_source_indexes() {
        let sources = [mon("DP-1"), win("jff-1", "Term", "foot"), mon("HDMI-A-1")];
        let windows = picker_entries(&sources, TYPE_WINDOW);
        assert_eq!(windows, vec![(1, "[1] window: Term (foot)".to_string())]);
        let all = picker_entries(&sources, AVAILABLE_TYPES);
        assert_eq!(all.len(), 3);
        assert_eq!(all[2], (2, "[2] monitor: HDMI-A-1".to_string()));
        assert!(picker_entries(&sources, 0).is_empty());
    }

    #[test]
    fn titles_cannot_split_or_fake_a_line() {
        let sources = [win("jff-1", "a\n[7] monitor: X", "x\ty")];
        let entries = picker_entries(&sources, TYPE_WINDOW);
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].1.contains('\n'));
        assert!(!entries[0].1.contains('\t'));
    }

    #[test]
    fn missing_title_or_app_id_still_make_a_line() {
        assert_eq!(win("i", "", "").describe(), "window: (untitled)");
        assert_eq!(win("i", "T", "").describe(), "window: T");
        assert_eq!(win("i", "", "app").describe(), "window: (app)");
    }

    #[test]
    fn an_offered_line_is_accepted() {
        let sources = [mon("DP-1"), win("jff-1", "Term", "foot")];
        let offered = picker_entries(&sources, AVAILABLE_TYPES);
        assert_eq!(
            parse_choice("[1] window: Term (foot)\n", &offered, false),
            Ok(vec![1])
        );
    }

    #[test]
    fn nothing_chosen_is_a_cancel() {
        let offered = picker_entries(&[mon("DP-1")], TYPE_MONITOR);
        assert_eq!(
            parse_choice("", &offered, false),
            Err(ChoiceError::Cancelled)
        );
        assert_eq!(
            parse_choice("\n  \n", &offered, true),
            Err(ChoiceError::Cancelled)
        );
    }

    #[test]
    fn an_unlisted_or_altered_line_is_rejected() {
        let offered = picker_entries(&[mon("DP-1"), win("jff-1", "Term", "foot")], TYPE_WINDOW);
        // A monitor the window-only request did not offer.
        assert!(matches!(
            parse_choice("[0] monitor: DP-1", &offered, false),
            Err(ChoiceError::Unknown(_))
        ));
        // A drun-style entry, or the right index with other text.
        assert!(matches!(
            parse_choice("Firefox", &offered, false),
            Err(ChoiceError::Unknown(_))
        ));
        assert!(matches!(
            parse_choice("[1] window: Other (foot)", &offered, false),
            Err(ChoiceError::Unknown(_))
        ));
        assert!(matches!(
            parse_choice("[99] window: Term (foot)", &offered, false),
            Err(ChoiceError::Unknown(_))
        ));
    }

    #[test]
    fn several_lines_need_multiple() {
        let offered = picker_entries(&[mon("A"), mon("B")], TYPE_MONITOR);
        let both = "[0] monitor: A\n[1] monitor: B\n";
        assert_eq!(
            parse_choice(both, &offered, false),
            Err(ChoiceError::TooMany)
        );
        assert_eq!(parse_choice(both, &offered, true), Ok(vec![0, 1]));
    }

    #[test]
    fn the_same_source_twice_is_rejected() {
        let offered = picker_entries(&[mon("A")], TYPE_MONITOR);
        assert_eq!(
            parse_choice("[0] monitor: A\n[0] monitor: A", &offered, true),
            Err(ChoiceError::Duplicate(0))
        );
    }

    #[test]
    fn argb_is_preferred_over_xrgb() {
        assert_eq!(
            pick_shm_format(&[SHM_XRGB8888, SHM_ARGB8888]),
            Some((SHM_ARGB8888, PixelFormat::Bgra))
        );
        assert_eq!(
            pick_shm_format(&[SHM_XRGB8888]),
            Some((SHM_XRGB8888, PixelFormat::Bgrx))
        );
    }

    #[test]
    fn ten_bit_formats_alone_are_not_usable() {
        // Xrgb2101010 is 0x30335258, Argb2101010 0x30334241.
        assert_eq!(pick_shm_format(&[0x30335258, 0x30334241]), None);
        assert_eq!(pick_shm_format(&[]), None);
    }

    #[test]
    fn sizes_are_bounded() {
        assert!(valid_size(1920, 1080));
        assert!(!valid_size(0, 10));
        assert!(!valid_size(10, 0));
        assert!(!valid_size(MAX_SIDE + 1, 10));
    }

    #[test]
    fn frame_math() {
        assert_eq!(stride(1920), 7680);
        assert_eq!(frame_len(2, 3), 24);
    }

    #[test]
    fn rows_are_copied_between_different_strides() {
        let src = [1, 2, 3, 4, 9, 9, 5, 6, 7, 8, 9, 9];
        let mut dst = [0u8; 8];
        assert_eq!(copy_rows(&src, 6, &mut dst, 4, 4, 2), 2);
        assert_eq!(dst, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn rows_outside_a_slice_are_skipped() {
        let src = [1u8; 8];
        let mut dst = [0u8; 4];
        assert_eq!(copy_rows(&src, 4, &mut dst, 4, 4, 2), 1);
        assert_eq!(copy_rows(&src, 2, &mut dst, 4, 4, 1), 0);
    }

    #[test]
    fn opaque_touches_only_the_alpha_byte_of_each_row() {
        let mut buf = [
            1, 2, 3, 0, 4, 5, 6, 0, 9, 9, 9, 9, 7, 8, 9, 0, 1, 1, 1, 0, 9, 9, 9, 9,
        ];
        force_opaque(&mut buf, 12, 2, 2);
        assert_eq!(
            buf,
            [1, 2, 3, 255, 4, 5, 6, 255, 9, 9, 9, 9, 7, 8, 9, 255, 1, 1, 1, 255, 9, 9, 9, 9]
        );
        // Too few bytes for a second row: it is skipped, not a panic.
        let mut short = [0u8; 8];
        force_opaque(&mut short, 8, 2, 3);
        assert_eq!(short, [0, 0, 0, 255, 0, 0, 0, 255]);
    }
}
