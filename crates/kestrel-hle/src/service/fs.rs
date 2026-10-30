//! The file system: "fsp-srv" (Switchbrew, "Filesystem services") and the
//! SD card it opens, which is a folder on the host. These are the commands
//! libnx's fsInitialize, fsdevMountSdmc and its "sdmc:" device send
//! (`nx/source/services/fs.c`, `nx/source/runtime/devices/fs_dev.c`).
//!
//! The owner chose where the card lives: a folder the frontend picks
//! (kestrel's portable `user/sdmc` folder, or `--sd`), and writes reach
//! it. Kestrel keeps every path inside that folder: a path is checked
//! component by component, and links on the host are never followed.
//! The checks give the same answers on every host, so a program behaves
//! the same on Windows, macOS and Linux.
//!
//! A program's own file can live outside the card's folder, in the
//! owner's game library anywhere on the host. Then it shows on the card at
//! the path the frontend gives it (`sdmc:/switch/<name>.nro`, where the
//! homebrew menu keeps programs), read-only, with the folders above it,
//! so the program finds its folder and the files bundled in it. Whatever
//! the program writes still goes to the card's folder; a file the folder
//! really has at that path wins.

use super::{Call, Interface, IpcError, Out};
use crate::Process;
use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// FS_MAX_PATH: a path buffer holds up to 0x300 bytes and a NUL.
const MAX_PATH: usize = 0x301;
/// A DirectoryEntry ("Filesystem services").
pub const ENTRY_SIZE: usize = 0x310;

/// Result codes (Switchbrew, "Error codes", module 2, Fs).
pub mod result {
    pub const PATH_NOT_FOUND: u32 = 0x202;
    pub const PATH_ALREADY_EXISTS: u32 = 0x402;
    pub const TARGET_LOCKED: u32 = 0xe02;
    pub const PORT_SD_CARD_NO_DEVICE: u32 = 0xfa202;
    pub const TOO_LONG_PATH: u32 = 0x2ee602;
    pub const INVALID_CHARACTER: u32 = 0x2ee802;
    pub const INVALID_PATH_FORMAT: u32 = 0x2eea02;
    pub const NOT_NORMALIZED: u32 = 0x2eee02;
    pub const INVALID_OFFSET: u32 = 0x2f5a02;
    pub const INVALID_SIZE: u32 = 0x2f5c02;
    pub const FILE_EXTENSION_WITHOUT_OPEN_MODE_ALLOW_APPEND: u32 = 0x307202;
    pub const UNSUPPORTED_WRITE_FOR_READ_ONLY_FILE_SYSTEM: u32 = 0x31c202;
    pub const UNSUPPORTED_WRITE_FOR_READ_ONLY_FILE: u32 = 0x31c802;
}

/// OpenFile's mode bits ("Filesystem services", OpenFile).
mod mode {
    pub const READ: u32 = 1;
    pub const WRITE: u32 = 2;
    pub const APPEND: u32 = 4;
}

/// What the process opened, and where the card is.
#[derive(Debug, Default)]
pub struct State {
    /// The host folder that is the SD card; None: no card is inserted.
    pub sd: Option<PathBuf>,
    /// SetCurrentProcess came, with the process ID.
    pub process_set: bool,
    /// The program's own file, when it lives outside the card's folder.
    pub program: Option<Program>,
    files: BTreeMap<u32, OpenFile>,
    dirs: BTreeMap<u32, OpenDir>,
    next_id: u32,
}

/// A program's own file, shown read-only on the card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    /// Its path on the card, by component: ["switch", "name.nro"].
    pub parts: Vec<String>,
    /// The file on the host.
    pub host: PathBuf,
}

#[derive(Debug)]
struct OpenFile {
    /// The guest path, "/a/b".
    path: String,
    host: PathBuf,
    mode: u32,
}

#[derive(Debug)]
struct OpenDir {
    path: String,
    /// The entries when it was opened, in name order.
    entries: Vec<[u8; ENTRY_SIZE]>,
    /// How many Read has handed out.
    read: usize,
}

impl State {
    /// Files and directories open now.
    pub fn open_objects(&self) -> usize {
        self.files.len() + self.dirs.len()
    }

    fn add_id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    /// An open file or directory is at `path` or, for `below`, under it.
    fn locked(&self, path: &str, below: bool) -> bool {
        let hit =
            |p: &str| p == path || (below && (path == "/" || p.starts_with(&format!("{path}/"))));
        self.files.values().any(|f| hit(&f.path)) || self.dirs.values().any(|d| hit(&d.path))
    }
}

/// Why an fs command gave no ordinary answer.
enum Fail {
    /// Answer with this result code.
    Code(u32),
    Stop(IpcError),
}

impl From<IpcError> for Fail {
    fn from(e: IpcError) -> Self {
        Fail::Stop(e)
    }
}

fn stop(what: String) -> Fail {
    Fail::Stop(IpcError::Stop(what))
}

/// A host error kestrel cannot turn into a result: stop with it.
fn host(what: &str, path: &Path, e: std::io::Error) -> Fail {
    stop(format!("host file system: {what} {}: {e}", path.display()))
}

/// Check a path as libnx sends it (a NUL-terminated string in a 0x301-byte
/// buffer) and split it into its components. Switchbrew lists the results;
/// which path gets which is kestrel's choice:
/// - no NUL in the buffer: TooLongPath;
/// - not UTF-8: InvalidCharacter;
/// - not starting with '/': InvalidPathFormat;
/// - an empty, "." or ".." component (one trailing '/' is allowed):
///   NotNormalized;
/// - a character or name a host could read differently: InvalidCharacter.
///   That is a control character, one of `\ : * ? " < > |`, a name ending
///   in '.' or ' ', or a Windows device name (CON, PRN, AUX, NUL, COM1-9,
///   LPT1-9). Rejecting them everywhere keeps every host the same.
pub fn parse_path(bytes: &[u8]) -> Result<Vec<String>, u32> {
    let len = bytes
        .iter()
        .position(|&b| b == 0)
        .ok_or(result::TOO_LONG_PATH)?;
    let s = std::str::from_utf8(&bytes[..len]).map_err(|_| result::INVALID_CHARACTER)?;
    let rest = s.strip_prefix('/').ok_or(result::INVALID_PATH_FORMAT)?;
    // One trailing '/' after a name; "//" alone is an empty component.
    let rest = match rest.strip_suffix('/') {
        Some(name) if !name.is_empty() => name,
        _ => rest,
    };
    if rest.is_empty() {
        return Ok(Vec::new());
    }
    rest.split('/').map(component).collect()
}

fn component(c: &str) -> Result<String, u32> {
    if c.is_empty() || c == "." || c == ".." {
        return Err(result::NOT_NORMALIZED);
    }
    let bad_char = c.chars().any(|ch| ch < ' ' || "\\:*?\"<>|".contains(ch));
    if bad_char || c.ends_with(['.', ' ']) || device_name(c) {
        return Err(result::INVALID_CHARACTER);
    }
    Ok(c.to_string())
}

/// A Windows device name, with or without an extension, in any case.
fn device_name(c: &str) -> bool {
    let stem = c.split('.').next().unwrap_or("").to_ascii_uppercase();
    let numbered = stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && matches!(stem.as_bytes()[3], b'1'..=b'9');
    numbered || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
}

fn guest_path(parts: &[String]) -> String {
    format!("/{}", parts.join("/"))
}

/// What is at a path on the card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Found {
    Nothing,
    File(u64),
    Dir,
    /// A host link, or a path through one: never followed, so it hides
    /// whatever it points at (kestrel's choice).
    Link,
    /// The program's own file, read-only, with its size.
    Program(u64),
    /// A folder on the way to the program's file that the card's folder
    /// does not have.
    ProgramDir,
}

/// The host path of `parts` under `root` and what is there.
fn lookup(root: &Path, parts: &[String]) -> Result<(PathBuf, Found), Fail> {
    let mut p = root.to_path_buf();
    let mut found = match fs::symlink_metadata(&p) {
        Ok(m) if m.is_dir() => Found::Dir,
        Ok(_) => Found::Link,
        Err(e) if e.kind() == ErrorKind::NotFound => Found::Nothing,
        Err(e) => return Err(host("cannot read", &p, e)),
    };
    for part in parts {
        p.push(part);
        found = match found {
            Found::Dir => match fs::symlink_metadata(&p) {
                Ok(m) if m.file_type().is_symlink() => Found::Link,
                Ok(m) if m.is_dir() => Found::Dir,
                Ok(m) if m.is_file() => Found::File(m.len()),
                Ok(_) => Found::Link,
                Err(e) if e.kind() == ErrorKind::NotFound => Found::Nothing,
                Err(e) => return Err(host("cannot read", &p, e)),
            },
            // Below a link, a file or nothing.
            Found::Link => Found::Link,
            // (The host walk finds neither kind of program entry.)
            Found::File(_) | Found::Nothing | Found::Program(_) | Found::ProgramDir => {
                Found::Nothing
            }
        };
    }
    Ok((p, found))
}

/// The entries of the host directory `dir` a guest can see, in name order:
/// links and names the path rules reject are left out. `extra` is an entry
/// only the guest sees (the program's file, or a folder above it): its
/// name, whether it is a folder, its size.
fn list(
    dir: &Path,
    filter: u64,
    extra: Option<(String, bool, u64)>,
) -> Result<Vec<[u8; ENTRY_SIZE]>, Fail> {
    let mut names = Vec::new();
    let read = match fs::read_dir(dir) {
        Ok(r) => Some(r),
        // A folder only the guest sees.
        Err(e) if e.kind() == ErrorKind::NotFound && extra.is_some() => None,
        Err(e) => return Err(host("cannot list", dir, e)),
    };
    if let Some((name, is_dir, size)) = extra {
        if filter & if is_dir { 1 } else { 2 } != 0 {
            names.push((name, is_dir, size));
        }
    }
    for e in read.into_iter().flatten() {
        let e = e.map_err(|e| host("cannot list", dir, e))?;
        let Ok(name) = e.file_name().into_string() else {
            continue;
        };
        let m =
            fs::symlink_metadata(e.path()).map_err(|err| host("cannot read", &e.path(), err))?;
        let is_dir = m.is_dir();
        if m.file_type().is_symlink() || !(is_dir || m.is_file()) {
            continue;
        }
        if component(&name).is_err() || name.len() >= MAX_PATH {
            continue;
        }
        // OpenDirectory's filter: bit 0 directories, bit 1 files.
        if filter & if is_dir { 1 } else { 2 } != 0 {
            names.push((name, is_dir, if is_dir { 0 } else { m.len() }));
        }
    }
    names.sort();
    Ok(names
        .into_iter()
        .map(|(name, is_dir, size)| entry(&name, is_dir, size))
        .collect())
}

/// A DirectoryEntry: the name, the attributes (bit 0, a directory), the
/// DirectoryEntryType (0 directory, 1 file) and the size, 0 for a
/// directory. Kestrel never sets the archive bit.
fn entry(name: &str, is_dir: bool, size: u64) -> [u8; ENTRY_SIZE] {
    let mut e = [0u8; ENTRY_SIZE];
    e[..name.len()].copy_from_slice(name.as_bytes());
    e[0x301] = is_dir as u8;
    e[0x304] = !is_dir as u8;
    e[0x308..].copy_from_slice(&size.to_le_bytes());
    e
}

impl Process {
    pub(super) fn fs(&mut self, call: &Call) -> Result<Out, IpcError> {
        match self.fs_command(call) {
            Ok(out) => Ok(out),
            Err(Fail::Code(code)) => Ok(Out::error(code)),
            Err(Fail::Stop(e)) => Err(e),
        }
    }

    /// The fs objects an interface held go when it is closed.
    pub(super) fn fs_released(&mut self, object: Interface) {
        match object {
            Interface::File(id) => {
                self.services.fs.files.remove(&id);
            }
            Interface::Directory(id) => {
                self.services.fs.dirs.remove(&id);
            }
            _ => {}
        }
    }

    /// What is at `parts` on the card: the host folder's entry, or else
    /// the program's file or a folder above it.
    fn find(&self, root: &Path, parts: &[String]) -> Result<(PathBuf, Found), Fail> {
        let (p, found) = lookup(root, parts)?;
        if found != Found::Nothing {
            return Ok((p, found));
        }
        match &self.services.fs.program {
            Some(prog) if prog.parts == parts => {
                let len = fs::metadata(&prog.host)
                    .map_err(|e| host("cannot read", &prog.host, e))?
                    .len();
                Ok((prog.host.clone(), Found::Program(len)))
            }
            Some(prog) if prog.parts.len() > parts.len() && prog.parts.starts_with(parts) => {
                Ok((p, Found::ProgramDir))
            }
            _ => Ok((p, found)),
        }
    }

    /// The entry only the guest sees in the folder at `parts`: the next
    /// step towards the program's file, when the host folder lacks it.
    fn program_entry(
        &self,
        root: &Path,
        parts: &[String],
    ) -> Result<Option<(String, bool, u64)>, Fail> {
        let Some(prog) = &self.services.fs.program else {
            return Ok(None);
        };
        if prog.parts.len() <= parts.len() || !prog.parts.starts_with(parts) {
            return Ok(None);
        }
        let child = &prog.parts[..parts.len() + 1];
        Ok(match self.find(root, child)?.1 {
            Found::Program(len) => Some((child[parts.len()].clone(), false, len)),
            Found::ProgramDir => Some((child[parts.len()].clone(), true, 0)),
            _ => None,
        })
    }

    /// The folder a new entry at `parts` goes in exists, made in the
    /// card's folder when only the guest saw it.
    fn parent_exists(&self, root: &Path, parts: &[String]) -> Result<bool, Fail> {
        let (p, found) = self.find(root, &parts[..parts.len() - 1])?;
        match found {
            Found::Dir => Ok(true),
            Found::ProgramDir => {
                fs::create_dir_all(&p).map_err(|e| host("cannot create", &p, e))?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// The path in pointer (X) buffer `index`, checked.
    fn fs_path(&self, call: &Call, index: u8) -> Result<Vec<String>, Fail> {
        let bytes = self.read_pointer(call.req, index)?;
        parse_path(&bytes).map_err(Fail::Code)
    }

    fn fs_command(&mut self, call: &Call) -> Result<Out, Fail> {
        use Interface::*;
        match (call.object, call.n) {
            // SetCurrentProcess: a u64 the kernel's process ID replaces.
            (FileSystemProxy, 1) => {
                call.u64(0)?;
                if !call.req.send_pid {
                    return Err(stop(
                        "fsp-srv SetCurrentProcess without the process ID".into(),
                    ));
                }
                self.services.fs.process_set = true;
                Ok(Out::empty())
            }
            // OpenSdCardFileSystem. Without a card, PortSdCardNoDevice
            // (kestrel's choice of result). Before SetCurrentProcess the
            // file system does not know the process, so it stops.
            (FileSystemProxy, 18) => {
                if !self.services.fs.process_set {
                    return Err(stop(
                        "fsp-srv OpenSdCardFileSystem before SetCurrentProcess".into(),
                    ));
                }
                if self.services.fs.sd.is_none() {
                    return Err(Fail::Code(result::PORT_SD_CARD_NO_DEVICE));
                }
                Ok(Out::object(SdCard))
            }
            (SdCard, n) => self.file_system(call, n),
            (File(id), n) => self.file(call, id, n),
            (Directory(id), n) => self.directory(call, id, n),
            _ => Err(call.unimplemented().into()),
        }
    }

    fn file_system(&mut self, call: &Call, n: u32) -> Result<Out, Fail> {
        use result::*;
        let root = self.services.fs.sd.clone().expect("the card is in");
        let code = |c| Err(Fail::Code(c));
        match n {
            // CreateFile: u32 option, padding, s64 size. Option bit 0 makes
            // a concatenation file (libnx, FsCreateOption_BigFile), which
            // the SD folder does not need: kestrel stops on it.
            0 => {
                let option = u32::from_le_bytes(call.arg(0)?);
                let size = call.u64(8)? as i64;
                if option != 0 {
                    return Err(stop(format!(
                        "IFileSystem CreateFile with option {option:#x}"
                    )));
                }
                if size < 0 {
                    return code(INVALID_SIZE);
                }
                let parts = self.fs_path(call, 0)?;
                if parts.is_empty() {
                    return code(PATH_ALREADY_EXISTS);
                }
                let (p, found) = self.find(&root, &parts)?;
                if found != Found::Nothing {
                    return code(PATH_ALREADY_EXISTS);
                }
                if !self.parent_exists(&root, &parts)? {
                    return code(PATH_NOT_FOUND);
                }
                let f = fs::File::create_new(&p).map_err(|e| host("cannot create", &p, e))?;
                f.set_len(size as u64)
                    .map_err(|e| host("cannot size", &p, e))?;
                Ok(Out::empty())
            }
            // DeleteFile.
            1 => {
                let parts = self.fs_path(call, 0)?;
                let (p, found) = self.find(&root, &parts)?;
                if let Found::Program(_) = found {
                    return code(UNSUPPORTED_WRITE_FOR_READ_ONLY_FILE_SYSTEM);
                }
                if !matches!(found, Found::File(_)) {
                    return code(PATH_NOT_FOUND);
                }
                if self.services.fs.locked(&guest_path(&parts), false) {
                    return code(TARGET_LOCKED);
                }
                fs::remove_file(&p).map_err(|e| host("cannot delete", &p, e))?;
                Ok(Out::empty())
            }
            // CreateDirectory.
            2 => {
                let parts = self.fs_path(call, 0)?;
                if parts.is_empty() {
                    return code(PATH_ALREADY_EXISTS);
                }
                let (p, found) = self.find(&root, &parts)?;
                if found == Found::ProgramDir {
                    // It shows already; making it puts it in the folder.
                    fs::create_dir_all(&p).map_err(|e| host("cannot create", &p, e))?;
                    return code(PATH_ALREADY_EXISTS);
                }
                if found != Found::Nothing {
                    return code(PATH_ALREADY_EXISTS);
                }
                if !self.parent_exists(&root, &parts)? {
                    return code(PATH_NOT_FOUND);
                }
                fs::create_dir(&p).map_err(|e| host("cannot create", &p, e))?;
                Ok(Out::empty())
            }
            // DeleteDirectory, DeleteDirectoryRecursively,
            // CleanDirectoryRecursively. An open file or directory at or
            // under the path is TargetLocked (kestrel's choice of when it
            // applies). Switchbrew documents no result for deleting a
            // directory that is not empty, so that stops.
            3 | 4 | 13 => {
                let parts = self.fs_path(call, 0)?;
                let (p, found) = self.find(&root, &parts)?;
                if found == Found::ProgramDir {
                    return code(UNSUPPORTED_WRITE_FOR_READ_ONLY_FILE_SYSTEM);
                }
                if found != Found::Dir {
                    return code(PATH_NOT_FOUND);
                }
                if parts.is_empty() && n != 13 {
                    return Err(stop("IFileSystem: deleting the root directory".into()));
                }
                if self.services.fs.locked(&guest_path(&parts), true) {
                    return code(TARGET_LOCKED);
                }
                match n {
                    3 => {
                        if !list(&p, 3, self.program_entry(&root, &parts)?)?.is_empty() {
                            return Err(stop(format!(
                                "IFileSystem DeleteDirectory of {}, which is not empty",
                                guest_path(&parts)
                            )));
                        }
                        fs::remove_dir(&p).map_err(|e| host("cannot delete", &p, e))?;
                    }
                    4 => fs::remove_dir_all(&p).map_err(|e| host("cannot delete", &p, e))?,
                    // What a guest sees goes; host links stay.
                    _ => {
                        for e in list(&p, 3, None)? {
                            let len = e.iter().position(|&b| b == 0).unwrap();
                            let at = p.join(std::str::from_utf8(&e[..len]).unwrap());
                            let gone = if e[0x301] & 1 != 0 {
                                fs::remove_dir_all(&at)
                            } else {
                                fs::remove_file(&at)
                            };
                            gone.map_err(|err| host("cannot delete", &at, err))?;
                        }
                    }
                }
                Ok(Out::empty())
            }
            // RenameFile, RenameDirectory: the paths in pointer buffers 0
            // and 1.
            5 | 6 => {
                let from = self.fs_path(call, 0)?;
                let to = self.fs_path(call, 1)?;
                let (src, found) = self.find(&root, &from)?;
                if matches!(found, Found::Program(_) | Found::ProgramDir) {
                    return code(UNSUPPORTED_WRITE_FOR_READ_ONLY_FILE_SYSTEM);
                }
                let right_kind = match found {
                    Found::File(_) => n == 5,
                    Found::Dir => n == 6 && !from.is_empty(),
                    _ => false,
                };
                if !right_kind {
                    return code(PATH_NOT_FOUND);
                }
                if to.is_empty() {
                    return code(PATH_ALREADY_EXISTS);
                }
                let (dst, there) = self.find(&root, &to)?;
                if there != Found::Nothing {
                    return code(PATH_ALREADY_EXISTS);
                }
                if !self.parent_exists(&root, &to)? {
                    return code(PATH_NOT_FOUND);
                }
                let (from_s, to_s) = (guest_path(&from), guest_path(&to));
                if self.services.fs.locked(&from_s, n == 6) {
                    return code(TARGET_LOCKED);
                }
                if n == 6 && to_s.starts_with(&format!("{from_s}/")) {
                    return Err(stop(format!(
                        "IFileSystem RenameDirectory of {from_s} into itself ({to_s})"
                    )));
                }
                fs::rename(&src, &dst).map_err(|e| host("cannot rename", &src, e))?;
                Ok(Out::empty())
            }
            // GetEntryType: a u32 DirectoryEntryType.
            7 => {
                let parts = self.fs_path(call, 0)?;
                match self.find(&root, &parts)?.1 {
                    Found::Dir | Found::ProgramDir => Ok(Out::ok(0u32.to_le_bytes().to_vec())),
                    Found::File(_) | Found::Program(_) => Ok(Out::ok(1u32.to_le_bytes().to_vec())),
                    _ => code(PATH_NOT_FOUND),
                }
            }
            // OpenFile: a u32 mode. Modes Switchbrew does not describe (no
            // bits, other bits, or append without write) stop.
            8 => {
                let m = u32::from_le_bytes(call.arg(0)?);
                if m == 0 || m & !7 != 0 || (m & mode::APPEND != 0 && m & mode::WRITE == 0) {
                    return Err(stop(format!("IFileSystem OpenFile with mode {m:#x}")));
                }
                let parts = self.fs_path(call, 0)?;
                let (host, found) = self.find(&root, &parts)?;
                match found {
                    Found::File(_) => {}
                    // The program's own file opens for reading only
                    // (kestrel's choice of when the result applies).
                    Found::Program(_) if m & mode::WRITE != 0 => {
                        return code(UNSUPPORTED_WRITE_FOR_READ_ONLY_FILE)
                    }
                    Found::Program(_) => {}
                    _ => return code(PATH_NOT_FOUND),
                }
                let fs = &mut self.services.fs;
                let id = fs.add_id();
                let path = guest_path(&parts);
                fs.files.insert(
                    id,
                    OpenFile {
                        path,
                        host,
                        mode: m,
                    },
                );
                Ok(Out::object(Interface::File(id)))
            }
            // OpenDirectory: a filter, bit 0 directories, bit 1 files.
            // Switchbrew calls it a u64, but libnx's fsFsOpenDirectory sends
            // a u32 and works on consoles, so only the low 32 bits count.
            // The entries are read when it opens, in name order (kestrel's
            // choice: host folders list in different orders).
            9 => {
                let filter = u32::from_le_bytes(call.arg(0)?) as u64;
                if filter == 0 || filter & !3 != 0 {
                    return Err(stop(format!(
                        "IFileSystem OpenDirectory with filter {filter:#x}"
                    )));
                }
                let parts = self.fs_path(call, 0)?;
                let (p, found) = self.find(&root, &parts)?;
                if !matches!(found, Found::Dir | Found::ProgramDir) {
                    return code(PATH_NOT_FOUND);
                }
                let entries = list(&p, filter, self.program_entry(&root, &parts)?)?;
                let fs = &mut self.services.fs;
                let id = fs.add_id();
                let path = guest_path(&parts);
                fs.dirs.insert(
                    id,
                    OpenDir {
                        path,
                        entries,
                        read: 0,
                    },
                );
                Ok(Out::object(Interface::Directory(id)))
            }
            // Commit: writes reach the folder at once, so nothing waits.
            10 => Ok(Out::empty()),
            // GetFileTimeStampRaw: three u64 times and a valid flag (libnx,
            // FsTimeStampRaw). Kestrel reports none, so runs repeat: all 0.
            14 => {
                let parts = self.fs_path(call, 0)?;
                match self.find(&root, &parts)?.1 {
                    Found::Dir | Found::File(_) | Found::Program(_) | Found::ProgramDir => {
                        Ok(Out::ok(vec![0; 0x20]))
                    }
                    _ => code(PATH_NOT_FOUND),
                }
            }
            _ => Err(call.unimplemented().into()),
        }
    }

    fn file(&mut self, call: &Call, id: u32, n: u32) -> Result<Out, Fail> {
        use result::*;
        let f = &self.services.fs.files[&id];
        let (host_path, m) = (f.host.clone(), f.mode);
        // Switchbrew gives only a range of results (6200-6299,
        // InvalidOperationForOpenMode) for a command the mode does not
        // allow, so that stops.
        let need = |bit: u32, what: &str| {
            if m & bit == 0 {
                Err(stop(format!(
                    "IFile {what} on a file opened with mode {m:#x}"
                )))
            } else {
                Ok(())
            }
        };
        let open = |write: bool| {
            fs::OpenOptions::new()
                .read(true)
                .write(write)
                .open(&host_path)
                .map_err(|e| host("cannot open", &host_path, e))
        };
        match n {
            // Read and Write: u32 option, padding, s64 offset, u64 size.
            // Read fills buffer B 0 and returns the bytes read; reading at
            // or past the end reads nothing (kestrel's choice).
            0 | 1 => {
                let option = u32::from_le_bytes(call.arg(0)?);
                let offset = call.u64(8)? as i64;
                let size = call.u64(16)?;
                // Write option bit 0 flushes (libnx, FsWriteOption_Flush).
                if option > (n == 1) as u32 {
                    return Err(stop(format!(
                        "IFile {} with option {option:#x}",
                        if n == 0 { "Read" } else { "Write" }
                    )));
                }
                need(
                    if n == 0 { mode::READ } else { mode::WRITE },
                    if n == 0 { "Read" } else { "Write" },
                )?;
                if offset < 0 {
                    return Err(Fail::Code(INVALID_OFFSET));
                }
                let mut file = open(n == 1)?;
                let len = file
                    .metadata()
                    .map_err(|e| host("cannot read", &host_path, e))?
                    .len();
                let offset = offset as u64;
                let seek = |file: &mut fs::File| {
                    file.seek(SeekFrom::Start(offset))
                        .map_err(|e| host("cannot seek in", &host_path, e))
                };
                if n == 0 {
                    let got = size.min(len.saturating_sub(offset));
                    let mut data = vec![0u8; got as usize];
                    if got > 0 {
                        seek(&mut file)?;
                        file.read_exact(&mut data)
                            .map_err(|e| host("cannot read", &host_path, e))?;
                    }
                    self.write_buffer(call.req, 0, size, &data)?;
                    Ok(Out::ok(got.to_le_bytes().to_vec()))
                } else {
                    let data = self.read_send(call.req, 0, size)?;
                    // Writing past the end needs the append bit
                    // ("Filesystem services", OpenFile).
                    if offset + size > len && m & mode::APPEND == 0 {
                        return Err(Fail::Code(FILE_EXTENSION_WITHOUT_OPEN_MODE_ALLOW_APPEND));
                    }
                    seek(&mut file)?;
                    file.write_all(&data)
                        .map_err(|e| host("cannot write", &host_path, e))?;
                    Ok(Out::empty())
                }
            }
            // Flush: every write already reached the folder.
            2 => Ok(Out::empty()),
            // SetSize: s64, with the write bit (kestrel's choice).
            3 => {
                let size = call.u64(0)? as i64;
                need(mode::WRITE, "SetSize")?;
                if size < 0 {
                    return Err(Fail::Code(INVALID_SIZE));
                }
                open(true)?
                    .set_len(size as u64)
                    .map_err(|e| host("cannot size", &host_path, e))?;
                Ok(Out::empty())
            }
            4 => {
                let len = fs::metadata(&host_path)
                    .map_err(|e| host("cannot read", &host_path, e))?
                    .len();
                Ok(Out::ok(len.to_le_bytes().to_vec()))
            }
            _ => Err(call.unimplemented().into()),
        }
    }

    fn directory(&mut self, call: &Call, id: u32, n: u32) -> Result<Out, Fail> {
        match n {
            // Read: as many entries as buffer B 0 holds, then the number
            // read, 0 once all were read.
            0 => {
                let capacity = call.req.receives.first().map_or(0, |b| b.size) as usize;
                let d = self
                    .services
                    .fs
                    .dirs
                    .get_mut(&id)
                    .expect("an open directory");
                let take = (capacity / ENTRY_SIZE).min(d.entries.len() - d.read);
                let data: Vec<u8> = d.entries[d.read..d.read + take].concat();
                d.read += take;
                self.write_buffer(call.req, 0, data.len() as u64, &data)?;
                Ok(Out::ok((take as u64).to_le_bytes().to_vec()))
            }
            // GetEntryCount: every entry the directory lists (kestrel's
            // reading of "readable entries"), read or not.
            1 => {
                let d = &self.services.fs.dirs[&id];
                Ok(Out::ok((d.entries.len() as u64).to_le_bytes().to_vec()))
            }
            _ => Err(call.unimplemented().into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> Result<Vec<String>, u32> {
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        parse_path(&b)
    }

    #[test]
    fn paths_are_checked_the_same_on_every_host() {
        use result::*;
        assert_eq!(path("/"), Ok(vec![]));
        assert_eq!(path("/a/b.txt"), Ok(vec!["a".into(), "b.txt".into()]));
        assert_eq!(path("/a/"), Ok(vec!["a".into()]));
        assert_eq!(path("a"), Err(INVALID_PATH_FORMAT));
        assert_eq!(path(""), Err(INVALID_PATH_FORMAT));
        for bad in ["//", "/a//b", "/./a", "/a/..", "/a//"] {
            assert_eq!(path(bad), Err(NOT_NORMALIZED), "{bad}");
        }
        for bad in [
            "/a\\b", "/c:", "/a*", "/a?", "/a\"", "/a<", "/a>", "/a|", "/a\t", "/a.", "/a ",
            "/con", "/NUL.txt", "/com1", "/Lpt9.x",
        ] {
            assert_eq!(path(bad), Err(INVALID_CHARACTER), "{bad}");
        }
        assert_eq!(path("/com0"), Ok(vec!["com0".into()]));
        assert_eq!(path("/console"), Ok(vec!["console".into()]));
        assert_eq!(parse_path(&[b'/', 0xff, 0]), Err(INVALID_CHARACTER));
        assert_eq!(parse_path(&[b'/'; MAX_PATH]), Err(TOO_LONG_PATH));
    }

    /// Links on the host lead nowhere, so nothing outside the folder can
    /// be reached. (Making links on Windows needs extra rights.)
    #[cfg(unix)]
    #[test]
    fn host_links_are_never_followed() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!("kestrel-fs-links-{}", std::process::id()));
        let (root, outside) = (base.join("sd"), base.join("outside"));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret"), b"x").unwrap();
        symlink(&outside, root.join("link")).unwrap();
        symlink(outside.join("secret"), root.join("file")).unwrap();
        let at = |parts: &[&str]| {
            let parts: Vec<String> = parts.iter().map(|s| s.to_string()).collect();
            lookup(&root, &parts).ok().unwrap().1
        };
        assert_eq!(at(&["link"]), Found::Link);
        assert_eq!(at(&["link", "secret"]), Found::Link);
        assert_eq!(at(&["file"]), Found::Link);
        assert!(list(&root, 3).ok().unwrap().is_empty());
        fs::remove_dir_all(&base).unwrap();
    }
}
