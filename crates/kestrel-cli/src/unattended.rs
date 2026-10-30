//! Running a program with nobody at the controls: scripted button presses
//! (`--press`) and the last frame saved as a PNG (`--save-frame`), for
//! the standard homebrew examples in CI and for looking at what a program
//! drew.

use kestrel_hle::display::{Frame, Screen};
use kestrel_hle::input::{button, Controller, Pad, Source};
use std::cell::RefCell;
use std::rc::Rc;

/// hid samples the controller every 5 ms of guest time, 200 a second.
const SAMPLES_PER_SECOND: f64 = 200.0;
/// How long a scripted press holds its button: 20 samples, 100 ms
/// (kestrel's choice, long enough for a program reading once a frame).
const HOLD: u64 = 20;

/// A press: `button` (a Switchbrew name) at `seconds` of the controller's
/// samples, written `Plus@2.5`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Press {
    pub buttons: u64,
    pub sample: u64,
}

impl std::str::FromStr for Press {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let (name, at) = s
            .split_once('@')
            .ok_or_else(|| format!("--press {s}: expected <button>@<seconds>, as Plus@2"))?;
        let buttons = button::NAMES
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|&(_, b)| b)
            .ok_or_else(|| format!("--press {s}: no button named {name}"))?;
        let seconds: f64 = at
            .parse()
            .ok()
            .filter(|t: &f64| t.is_finite() && *t >= 0.0)
            .ok_or_else(|| format!("--press {s}: {at} is not a time in seconds"))?;
        Ok(Press {
            buttons,
            sample: (seconds * SAMPLES_PER_SECOND).round() as u64,
        })
    }
}

/// A Pro Controller pressing the scripted buttons and nothing else.
pub struct Scripted(pub Vec<Press>);

impl Source for Scripted {
    fn controller(&self) -> Controller {
        Controller::ProController
    }

    fn sample(&mut self, n: u64) -> Pad {
        let held = self
            .0
            .iter()
            .filter(|p| (p.sample..p.sample + HOLD).contains(&n));
        Pad {
            buttons: held.fold(0, |b, p| b | p.buttons),
            ..Pad::default()
        }
    }
}

/// A screen that keeps the last frame, passing every frame on to `inner`
/// (the window's screen) when there is one.
pub struct Keep {
    pub last: Rc<RefCell<Option<Frame>>>,
    pub inner: Option<Box<dyn Screen>>,
}

impl Screen for Keep {
    fn present(&mut self, frame: &Frame) {
        *self.last.borrow_mut() = Some(frame.clone());
        if let Some(s) = self.inner.as_mut() {
            s.present(frame);
        }
    }
}

/// `frame` as a PNG file: 8-bit RGB, the image data in stored (not
/// compressed) deflate blocks, as the PNG and zlib specifications allow.
pub fn png(frame: &Frame) -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend(kind);
        out.extend(data);
        let crc = kestrel_testsuite::crc32(&out[start..]);
        out.extend(crc.to_be_bytes());
    }
    let (w, h) = (frame.width as usize, frame.height as usize);
    // Each row: filter type 0 (none), then R, G, B for each pixel.
    let mut raw = Vec::with_capacity(h * (1 + 3 * w));
    for row in frame.pixels.chunks(w.max(1)).take(h) {
        raw.push(0);
        for &p in row {
            raw.extend([(p >> 16) as u8, (p >> 8) as u8, p as u8]);
        }
    }
    // zlib: header (deflate, 32 KiB window, no dictionary), stored blocks of
    // at most 65535 bytes, then the Adler-32 of the data.
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(0xffff).collect();
    for (i, block) in blocks.iter().enumerate() {
        z.push((i + 1 == blocks.len()) as u8);
        let len = block.len() as u16;
        z.extend(len.to_le_bytes());
        z.extend((!len).to_le_bytes());
        z.extend(*block);
    }
    if blocks.is_empty() {
        z.extend([1, 0, 0, 0xff, 0xff]);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend((b << 16 | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend((w as u32).to_be_bytes());
    ihdr.extend((h as u32).to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presses_read_a_button_and_a_time() {
        let p: Press = "Plus@2.5".parse().unwrap();
        assert_eq!((p.buttons, p.sample), (button::PLUS, 500));
        let p: Press = "a@0".parse().unwrap();
        assert_eq!((p.buttons, p.sample), (button::A, 0));
        for bad in ["Plus", "Home@1", "Plus@soon", "Plus@-1"] {
            assert!(bad.parse::<Press>().is_err(), "{bad}");
        }
    }

    #[test]
    fn a_press_holds_for_100_ms() {
        let mut s = Scripted(vec!["B@1".parse().unwrap(), "A@1.05".parse().unwrap()]);
        let at = |s: &mut Scripted, n| s.sample(n).buttons;
        assert_eq!(at(&mut s, 199), 0);
        assert_eq!(at(&mut s, 200), button::B);
        assert_eq!(at(&mut s, 210), button::A | button::B);
        assert_eq!(at(&mut s, 220), button::A);
        assert_eq!(at(&mut s, 230), 0);
    }

    #[test]
    fn pngs_are_well_formed() {
        // Checked against Python's zlib: the image data decompresses to
        // the rows, each after a 0 filter byte.
        let frame = Frame {
            width: 2,
            height: 1,
            pixels: vec![0x123456, 0xabcdef],
        };
        let p = png(&frame);
        assert_eq!(&p[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&p[12..16], b"IHDR");
        // IDAT: zlib header, one final stored block of 7 bytes, Adler-32.
        let idat = &p[33..];
        assert_eq!(&idat[4..8], b"IDAT");
        let z = &idat[8..8 + u32::from_be_bytes(idat[..4].try_into().unwrap()) as usize];
        assert_eq!(&z[..7], &[0x78, 0x01, 1, 7, 0, 0xf8, 0xff]);
        assert_eq!(&z[7..14], &[0, 0x12, 0x34, 0x56, 0xab, 0xcd, 0xef]);
        // zlib.adler32(bytes([0, 0x12, 0x34, 0x56, 0xab, 0xcd, 0xef])).
        assert_eq!(&z[14..], &0x0759_0304u32.to_be_bytes());
        assert!(p.ends_with(b"IEND\xae\x42\x60\x82"));
    }
}
