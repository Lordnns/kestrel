//! The IPC message format.
//!
//! A client writes its request at the start of its thread local region
//! and calls svcSendSyncRequest; the reply comes back in the same place.
//! The layout is from Switchbrew's "IPC Marshalling" page, with the field
//! widths and the order of the parts as libnx's open-source `hipc.h`,
//! `cmif.h` and `tipc.h` (ISC) build them:
//!
//! ```text
//! header        two words: command type, descriptor counts, raw size
//! special       optional: process ID flag, copy and move handle counts
//! process ID    8 bytes, when asked for (the kernel fills it in)
//! handles       copied, then moved
//! descriptors   pointers (X), send (A), receive (B), exchange (W)
//! raw data      words; CMIF puts its header 16-byte aligned inside
//! receive list  (C), after the raw data
//! ```
//!
//! Two protocols sit on top. CMIF: raw data holds padding to a 16-byte
//! boundary, the "SFCI" header with the command ID, then the arguments;
//! replies hold "SFCO" and the result. TIPC: the command ID is the header's
//! command type minus 16 and the raw data holds the arguments; replies hold
//! the result word first.

/// Bytes of the thread local region that hold a message.
pub const MESSAGE_SIZE: usize = 0x100;

/// The command types in a message header ("IPC Marshalling", libnx
/// `CmifCommandType`).
pub mod command_type {
    pub const INVALID: u16 = 0;
    pub const LEGACY_REQUEST: u16 = 1;
    pub const CLOSE: u16 = 2;
    pub const LEGACY_CONTROL: u16 = 3;
    pub const REQUEST: u16 = 4;
    pub const CONTROL: u16 = 5;
    pub const REQUEST_WITH_CONTEXT: u16 = 6;
    pub const CONTROL_WITH_CONTEXT: u16 = 7;
    /// TIPC command 0; command n is `TIPC + n`.
    pub const TIPC: u16 = 16;
}

/// "SFCI" and "SFCO" as little-endian words.
pub const CMIF_IN_MAGIC: u32 = u32::from_le_bytes(*b"SFCI");
pub const CMIF_OUT_MAGIC: u32 = u32::from_le_bytes(*b"SFCO");

/// A pointer descriptor (X): data the kernel copies to the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pointer {
    pub index: u8,
    pub addr: u64,
    pub size: u64,
}

/// A send, receive or exchange descriptor (A, B, W): memory the server
/// sees mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buffer {
    pub addr: u64,
    pub size: u64,
    /// 0 normal, 1 non-secure, 3 non-device.
    pub mode: u8,
}

/// A receive list entry (C): where the client takes data copied back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Receive {
    pub addr: u64,
    pub size: u64,
}

/// A parsed request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    pub command_type: u16,
    /// The client asked the kernel to send its process ID.
    pub send_pid: bool,
    pub copy_handles: Vec<u32>,
    pub move_handles: Vec<u32>,
    pub pointers: Vec<Pointer>,
    pub sends: Vec<Buffer>,
    pub receives: Vec<Buffer>,
    pub exchanges: Vec<Buffer>,
    /// Byte offset of the raw data in the message.
    pub raw_offset: usize,
    pub raw: Vec<u8>,
    pub receive_list: Vec<Receive>,
}

fn word(msg: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(msg[at..at + 4].try_into().unwrap())
}

fn bits(w: u32, lo: u32, n: u32) -> u32 {
    w >> lo & ((1 << n) - 1)
}

/// Reads a message front to back, refusing to run past its end.
struct Reader<'a> {
    msg: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<usize, String> {
        let at = self.at;
        if at + n > self.msg.len() {
            return Err(format!(
                "the IPC message is longer than the {:#x} bytes of the thread local region",
                MESSAGE_SIZE
            ));
        }
        self.at += n;
        Ok(at)
    }

    fn word(&mut self) -> Result<u32, String> {
        let at = self.take(4)?;
        Ok(word(self.msg, at))
    }
}

/// Parse the request at the start of `msg`. Errors describe a message
/// kestrel cannot read: one longer than the region, or a part it does not
/// model.
pub fn parse(msg: &[u8]) -> Result<Request, String> {
    let mut r = Reader { msg, at: 0 };
    let h0 = r.word()?;
    let h1 = r.word()?;
    let mut req = Request {
        command_type: bits(h0, 0, 16) as u16,
        ..Request::default()
    };
    let (n_pointers, n_sends) = (bits(h0, 16, 4), bits(h0, 20, 4));
    let (n_receives, n_exchanges) = (bits(h0, 24, 4), bits(h0, 28, 4));
    let raw_words = bits(h1, 0, 10) as usize;
    let receive_mode = bits(h1, 10, 4);
    if bits(h1, 20, 11) != 0 {
        return Err("unsupported: an IPC receive list at an explicit offset".into());
    }
    let n_receive_list = match receive_mode {
        0 => 0,
        // 1 is not described; 2 receives into the server's pointer buffer.
        1 | 2 => return Err(format!("unsupported: IPC receive list mode {receive_mode}")),
        m => m - 2,
    };
    if h1 >> 31 != 0 {
        let special = r.word()?;
        req.send_pid = special & 1 != 0;
        if req.send_pid {
            // The client leaves room; the kernel writes its process ID.
            r.take(8)?;
        }
        for _ in 0..bits(special, 1, 4) {
            req.copy_handles.push(r.word()?);
        }
        for _ in 0..bits(special, 5, 4) {
            req.move_handles.push(r.word()?);
        }
    }
    for _ in 0..n_pointers {
        let (w0, w1) = (r.word()?, r.word()?);
        req.pointers.push(Pointer {
            index: bits(w0, 0, 6) as u8,
            addr: w1 as u64 | (bits(w0, 12, 4) as u64) << 32 | (bits(w0, 6, 6) as u64) << 36,
            size: bits(w0, 16, 16) as u64,
        });
    }
    for (n, list) in [
        (n_sends, &mut req.sends),
        (n_receives, &mut req.receives),
        (n_exchanges, &mut req.exchanges),
    ] {
        for _ in 0..n {
            let (size_lo, addr_lo, w2) = (r.word()?, r.word()?, r.word()?);
            list.push(Buffer {
                addr: addr_lo as u64
                    | (bits(w2, 28, 4) as u64) << 32
                    | (bits(w2, 2, 22) as u64) << 36,
                size: size_lo as u64 | (bits(w2, 24, 4) as u64) << 32,
                mode: bits(w2, 0, 2) as u8,
            });
        }
    }
    req.raw_offset = r.take(raw_words * 4)?;
    req.raw = msg[req.raw_offset..req.raw_offset + raw_words * 4].to_vec();
    for _ in 0..n_receive_list {
        let (w0, w1) = (r.word()?, r.word()?);
        req.receive_list.push(Receive {
            addr: w0 as u64 | (bits(w1, 0, 16) as u64) << 32,
            size: bits(w1, 16, 16) as u64,
        });
    }
    Ok(req)
}

/// A CMIF request's header fields and arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CmifRequest {
    pub command: u32,
    /// The arguments: the raw data after the "SFCI" header.
    pub args: Vec<u8>,
}

/// Bytes of padding before CMIF data that starts at `raw_offset`, so it
/// lands on a 16-byte boundary of the message.
fn cmif_padding(raw_offset: usize) -> usize {
    raw_offset.next_multiple_of(16) - raw_offset
}

/// Read the "SFCI" header from a request's raw data.
pub fn cmif_request(req: &Request) -> Result<CmifRequest, String> {
    let start = cmif_padding(req.raw_offset);
    if req.raw.len() < start + 16 {
        return Err(format!(
            "a CMIF request with {} bytes of raw data, too few for its header",
            req.raw.len()
        ));
    }
    let magic = word(&req.raw, start);
    if magic != CMIF_IN_MAGIC {
        return Err(format!(
            "a CMIF request whose header magic is {magic:#010x}, not \"SFCI\""
        ));
    }
    Ok(CmifRequest {
        command: word(&req.raw, start + 8),
        args: req.raw[start + 16..].to_vec(),
    })
}

/// A reply from a service.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reply {
    pub result: u32,
    pub copy_handles: Vec<u32>,
    pub move_handles: Vec<u32>,
    pub data: Vec<u8>,
}

impl Reply {
    /// Success with `data` as the output arguments.
    pub fn ok(data: Vec<u8>) -> Self {
        Reply {
            data,
            ..Reply::default()
        }
    }

    /// Failure: only the result is sent.
    pub fn error(result: u32) -> Self {
        Reply {
            result,
            ..Reply::default()
        }
    }
}

/// The header, special header and handles of a reply with `raw_words` of
/// raw data. The command type is 0: the sources do not say what servers
/// write there, and libnx does not read it.
fn reply_head(reply: &Reply, raw_words: usize) -> Vec<u8> {
    let special = !reply.copy_handles.is_empty() || !reply.move_handles.is_empty();
    let mut out = Vec::new();
    out.extend(0u32.to_le_bytes());
    out.extend((raw_words as u32 | (special as u32) << 31).to_le_bytes());
    if special {
        let counts =
            (reply.copy_handles.len() as u32) << 1 | (reply.move_handles.len() as u32) << 5;
        out.extend(counts.to_le_bytes());
        for h in reply.copy_handles.iter().chain(&reply.move_handles) {
            out.extend(h.to_le_bytes());
        }
    }
    out
}

fn fits(out: Vec<u8>) -> Result<Vec<u8>, String> {
    if out.len() > MESSAGE_SIZE {
        return Err(format!(
            "an IPC reply of {:#x} bytes, more than the {MESSAGE_SIZE:#x} of the thread local region",
            out.len()
        ));
    }
    Ok(out)
}

/// Encode a CMIF reply: the raw data is 16 bytes of padding budget, the
/// "SFCO" header (version 0, token 0) and the output arguments.
pub fn cmif_reply(reply: &Reply) -> Result<Vec<u8>, String> {
    let raw_words = (16 + 16 + reply.data.len()).div_ceil(4);
    let mut out = reply_head(reply, raw_words);
    let raw_start = out.len();
    out.resize(raw_start + cmif_padding(raw_start), 0);
    for w in [CMIF_OUT_MAGIC, 0, reply.result, 0] {
        out.extend(w.to_le_bytes());
    }
    out.extend(&reply.data);
    out.resize(raw_start + raw_words * 4, 0);
    fits(out)
}

/// Encode a TIPC reply: the raw data is the result word, then the output
/// arguments.
pub fn tipc_reply(reply: &Reply) -> Result<Vec<u8>, String> {
    let raw_words = (4 + reply.data.len()).div_ceil(4);
    let mut out = reply_head(reply, raw_words);
    let raw_start = out.len();
    out.extend(reply.result.to_le_bytes());
    out.extend(&reply.data);
    out.resize(raw_start + raw_words * 4, 0);
    fits(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a message from words, padded to the region's size.
    fn msg(words: &[u32]) -> Vec<u8> {
        let mut m: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        m.resize(MESSAGE_SIZE, 0);
        m
    }

    #[test]
    fn a_cmif_request_with_a_process_id_and_handles() {
        // As libnx's cmifMakeRequest lays out sm:'s RegisterClient with one
        // copy handle and one move handle added: header, special header,
        // the process ID slot, handles, then raw data whose "SFCI" header
        // starts at offset 0x20.
        #[rustfmt::skip]
        let m = msg(&[
            4,                      // Request
            10 | 1 << 31,           // 10 raw words, special header
            1 | 1 << 1 | 1 << 5,    // pid, 1 copy, 1 move
            0, 0,                   // pid slot
            0x11, 0x22,             // handles at 0x14, 0x18
            0,                      // raw data from 0x1c: 4 bytes of padding
            CMIF_IN_MAGIC, 0, 7, 0, // command 7 at 0x20
            0xaabb, 0xccdd,         // u64 argument
        ]);
        let req = parse(&m).unwrap();
        assert_eq!(req.command_type, command_type::REQUEST);
        assert!(req.send_pid);
        assert_eq!(
            (&req.copy_handles[..], &req.move_handles[..]),
            (&[0x11][..], &[0x22][..])
        );
        assert_eq!((req.raw_offset, req.raw.len()), (0x1c, 40));
        let cmif = cmif_request(&req).unwrap();
        assert_eq!(cmif.command, 7);
        assert_eq!(&cmif.args[..8], &[0xbb, 0xaa, 0, 0, 0xdd, 0xcc, 0, 0]);
    }

    #[test]
    fn descriptors_decode_every_address_bit() {
        // A 39-bit address 0x7f_1234_5678 and a 36-bit size: the high bits
        // sit in separate fields, as hipcMakeSendStatic, hipcMakeBuffer and
        // hipcMakeRecvStatic place them.
        let addr: u64 = 0x7f_1234_5678;
        let (lo, mid, high) = (addr as u32, (addr >> 32 & 0xf) as u32, (addr >> 36) as u32);
        let size: u64 = 0x3_0000_0010;
        #[rustfmt::skip]
        let m = msg(&[
            4 | 1 << 16 | 1 << 20 | 1 << 24 | 1 << 28, // one of each descriptor
            1 | 3 << 10,                                // 1 raw word, 1 receive entry
            5 | high << 6 | mid << 12 | 0x40 << 16, lo, // pointer, index 5
            size as u32, lo, 1 | high << 2 | ((size >> 32) as u32) << 24 | mid << 28,
            0x10, lo, 3 | high << 2 | mid << 28,        // receive, non-device
            0x20, lo, high << 2 | mid << 28,            // exchange
            0xdead_beef,                                // raw data
            lo, (addr >> 32) as u32 | 0x80 << 16,       // receive list entry
        ]);
        let req = parse(&m).unwrap();
        assert_eq!(
            req.pointers,
            [Pointer {
                index: 5,
                addr,
                size: 0x40
            }]
        );
        assert_eq!(
            req.sends,
            [Buffer {
                addr,
                size,
                mode: 1
            }]
        );
        assert_eq!(
            req.receives,
            [Buffer {
                addr,
                size: 0x10,
                mode: 3
            }]
        );
        assert_eq!(
            req.exchanges,
            [Buffer {
                addr,
                size: 0x20,
                mode: 0
            }]
        );
        assert_eq!(req.raw, 0xdead_beefu32.to_le_bytes());
        assert_eq!(req.receive_list, [Receive { addr, size: 0x80 }]);
    }

    #[test]
    fn a_message_past_the_region_or_with_unmodelled_parts_is_refused() {
        let long = parse(&msg(&[4, 0x3ff])).unwrap_err();
        assert!(long.contains("longer than"), "{long}");
        let offset = parse(&msg(&[4, 1 << 20])).unwrap_err();
        assert!(offset.starts_with("unsupported"), "{offset}");
        let auto = parse(&msg(&[4, 2 << 10])).unwrap_err();
        assert!(auto.contains("mode 2"), "{auto}");
    }

    #[test]
    fn a_cmif_request_needs_its_header() {
        let short = parse(&msg(&[4, 4])).unwrap();
        assert!(cmif_request(&short).unwrap_err().contains("too few"));
        let wrong = parse(&msg(&[4, 8, 0, 0, 0x1234, 0, 0, 0])).unwrap();
        assert!(cmif_request(&wrong).unwrap_err().contains("0x00001234"));
    }

    #[test]
    fn a_cmif_reply_puts_sfco_on_a_16_byte_boundary() {
        // One move handle: header 8, special 4, handle 4, so raw data
        // starts at 0x10 and needs no padding; libnx's cmifParseResponse
        // finds "SFCO" at the first 16-byte boundary of the raw data.
        let r = Reply {
            move_handles: vec![0x1234],
            ..Reply::ok(vec![1, 2, 3])
        };
        let out = cmif_reply(&r).unwrap();
        assert_eq!(word(&out, 0), 0);
        assert_eq!(word(&out, 4), 9 | 1 << 31); // (16 + 16 + 3) / 4 rounded up
        assert_eq!(word(&out, 8), 1 << 5);
        assert_eq!(word(&out, 12), 0x1234);
        assert_eq!(word(&out, 16), CMIF_OUT_MAGIC);
        assert_eq!(word(&out, 24), 0);
        assert_eq!(&out[32..35], &[1, 2, 3]);
        assert_eq!(out.len(), 16 + 9 * 4);
        // Without handles the raw data starts at 8: 8 bytes of padding.
        let out = cmif_reply(&Reply::error(0x415)).unwrap();
        assert_eq!(word(&out, 4), 8);
        assert_eq!((word(&out, 16), word(&out, 24)), (CMIF_OUT_MAGIC, 0x415));
    }

    #[test]
    fn a_tipc_reply_starts_its_raw_data_with_the_result() {
        let r = Reply {
            copy_handles: vec![7],
            ..Reply::ok(vec![9; 5])
        };
        let out = tipc_reply(&r).unwrap();
        assert_eq!(word(&out, 4), 3 | 1 << 31);
        assert_eq!((word(&out, 8), word(&out, 12)), (1 << 1, 7));
        assert_eq!(word(&out, 16), 0);
        assert_eq!(&out[20..25], &[9; 5]);
        assert_eq!(out.len(), 28);
    }

    #[test]
    fn a_reply_that_does_not_fit_is_refused() {
        assert!(cmif_reply(&Reply::ok(vec![0; MESSAGE_SIZE])).is_err());
    }
}
