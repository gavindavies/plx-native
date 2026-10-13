//! **The local-file media plane: a file on this machine's own disk, read by byte offset.**
//!
//! The one source under `ff.rs`'s AVIO: positional reads (`pread(2)`) on one open descriptor, so a seek moves an offset and reopens nothing, and there is no connection to lose,
//! heal or wake. Teardown needs no transport-specific interrupt either: a local read returns in
//! milliseconds, and the demuxer re-checks the AU lane's abort flag around every one.
//!
//! A location is a `file://` URL (`file:///srv/media/Film%20(2001).mkv`), percent-encoded so it
//! can travel through the same `String` the player already carries for a part URL. [`file_url`]
//! builds one and [`path_of`] reads one back; nothing else is accepted, so an http part URL can
//! never be mistaken for a path.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// One open media file. Owned by the demux thread, like the `AvioState` that holds it.
pub struct LocalFile {
    file: File,
    size: i64,
}

impl LocalFile {
    /// Open `path` for reading. A directory is refused here rather than at the first read.
    pub fn open(path: &Path) -> std::io::Result<LocalFile> {
        let file = File::open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"));
        }
        Ok(LocalFile { file, size: i64::try_from(meta.len()).unwrap_or(i64::MAX) })
    }

    pub fn size(&self) -> i64 {
        self.size
    }

    /// Read into `dst` at byte `offset`, with the AVIO callback's three-way answer: `>0` bytes
    /// read, `0` at end of file, `<0` an I/O error. `EINTR` is retried; a short read is a normal
    /// answer and is returned as is.
    pub fn read_at(&self, dst: &mut [u8], offset: i64) -> i32 {
        if dst.is_empty() || offset < 0 {
            return 0;
        }
        let len = dst.len().min(i32::MAX as usize);
        loop {
            match self.file.read_at(&mut dst[..len], offset as u64) {
                Ok(n) => return n as i32,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return -e.raw_os_error().unwrap_or(libc::EIO),
            }
        }
    }
}

/// Is this a location for this source? The check every router does first.
pub fn is_file_url(url: &str) -> bool {
    url.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("file://"))
}

/// The absolute path a `file://` URL names, percent-decoded. `None` for any other scheme, a
/// relative or remote (`file://host/...`) location, a malformed escape, or a NUL byte.
pub fn path_of(url: &str) -> Option<PathBuf> {
    if !is_file_url(url) {
        return None;
    }
    let rest = &url[7..];
    // `file:///abs` has an empty authority; `file://localhost/abs` names this machine too.
    let path = if rest.starts_with('/') {
        rest
    } else {
        rest.strip_prefix("localhost").filter(|p| p.starts_with('/'))?
    };
    let bytes = percent_decode(path)?;
    if bytes.contains(&0) {
        return None;
    }
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// The `file://` URL for an absolute `path`: every byte outside the RFC 3986 unreserved set and
/// `/` is percent-encoded, so spaces, `#`, `?` and non-UTF-8 names all survive the round trip.
pub fn file_url(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut url = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    url
}

fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_round_trips_through_its_url() {
        for p in ["/srv/media/Film (2001).mkv", "/a/b#c?d%e.mkv", "/média/日本語/x y.mkv", "/plain/file.mkv"] {
            let url = file_url(Path::new(p));
            assert!(is_file_url(&url));
            assert!(!url[7..].contains(' ') && !url[7..].contains('#') && !url[7..].contains('?'), "{url}");
            assert_eq!(path_of(&url).as_deref(), Some(Path::new(p)), "{url}");
        }
    }

    #[test]
    fn only_local_absolute_file_urls_are_paths() {
        assert_eq!(path_of("file://localhost/x.mkv").as_deref(), Some(Path::new("/x.mkv")));
        assert_eq!(path_of("FILE:///x.mkv").as_deref(), Some(Path::new("/x.mkv")));
        for bad in ["http://h/x.mkv", "/x.mkv", "file://nas/x.mkv", "file://relative.mkv", "file:///a%2", "file:///a%00b"] {
            assert_eq!(path_of(bad), None, "{bad}");
        }
    }

    #[test]
    fn reads_are_positional_and_report_end_of_file() {
        let dir = std::env::temp_dir().join(format!("plx-localfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.bin");
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let f = LocalFile::open(&path).unwrap();
        assert_eq!(f.size(), 10_000);
        let mut buf = [0u8; 100];
        assert_eq!(f.read_at(&mut buf, 5_000), 100);
        assert_eq!(&buf[..], &data[5_000..5_100]);
        // Out of order: an offset, not a cursor.
        assert_eq!(f.read_at(&mut buf, 0), 100);
        assert_eq!(&buf[..], &data[..100]);
        assert_eq!(f.read_at(&mut buf, 9_950), 50, "a short read at the tail");
        assert_eq!(f.read_at(&mut buf, 10_000), 0, "end of file");
        assert!(LocalFile::open(&dir).is_err(), "a directory is not a media file");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
