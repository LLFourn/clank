//! The stat decoder shared by process identity and explanatory inspection.
//! https://www.man7.org/linux/man-pages/man5/proc_pid_stat.5.html

use std::io::{self, Read};

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Stat {
    pub(super) ticks: u64,
    pub(super) state: Option<u8>,
    pub(super) parent: Option<i32>,
    pub(super) group: Option<i32>,
}

pub(super) fn read_bounded(reader: impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::ErrorKind::FileTooLarge.into());
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
pub(super) fn read_stat(pid: i32) -> io::Result<Stat> {
    let bytes = read_bounded(std::fs::File::open(format!("/proc/{pid}/stat"))?, 16 * 1024)?;
    parse_stat(&bytes, pid).ok_or_else(|| io::ErrorKind::InvalidData.into())
}

pub(super) fn parse_stat(bytes: &[u8], pid: i32) -> Option<Stat> {
    // comm may contain spaces, newlines, non-UTF8 and ')'. Only fields
    // AFTER the last ')' are whitespace separated. Never decode comm.
    let open = bytes.iter().position(|b| *b == b'(')?;
    let close = bytes.iter().rposition(|b| *b == b')')?;
    if open >= close
        || std::str::from_utf8(&bytes[..open])
            .ok()?
            .trim()
            .parse::<i32>()
            .ok()?
            != pid
    {
        return None;
    }
    let rest = std::str::from_utf8(&bytes[close + 1..]).ok()?;
    let fields: Vec<_> = rest.split_ascii_whitespace().take(20).collect();
    // Field 3 (state) is index 0, field 22 (starttime) is index 19.
    let ticks = fields.get(19)?.parse().ok()?;
    let id = |n: usize| fields.get(n)?.parse::<i32>().ok().filter(|v| *v >= 0);
    Some(Stat {
        ticks,
        state: fields
            .first()
            .and_then(|s| (s.len() == 1).then(|| s.as_bytes()[0])),
        parent: id(1),
        group: id(2),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(comm: &[u8]) -> Vec<u8> {
        let mut bytes = b"42 (".to_vec();
        bytes.extend_from_slice(comm);
        // Fields 3..21, then birth (field 22), followed by irrelevant fields.
        bytes.extend_from_slice(b") S 7 8 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 12345 0 0\n");
        bytes
    }

    #[test]
    fn stat_counts_from_after_a_complex_comm() {
        let s = parse_stat(&stat(b"a ) spaced (\n\xff name"), 42).unwrap();
        assert_eq!(
            s,
            Stat {
                ticks: 12345,
                state: Some(b'S'),
                parent: Some(7),
                group: Some(8)
            }
        );
        assert!(parse_stat(&stat(b"name"), 43).is_none());
    }

    #[test]
    fn missing_or_invalid_birth_is_not_an_identity() {
        for bytes in [b"42 (cmd) S 7 8".as_slice(), b"42 cmd S 7 8", b"42 ) ( S"] {
            assert!(parse_stat(bytes, 42).is_none());
        }
        let bad = String::from_utf8(stat(b"cmd"))
            .unwrap()
            .replace("12345", "-1");
        assert!(parse_stat(bad.as_bytes(), 42).is_none());
    }

    #[test]
    fn bad_relationships_do_not_hide_a_readable_birth() {
        let bad = String::from_utf8(stat(b"cmd"))
            .unwrap()
            .replace("S 7 8", "?? x -1");
        let s = parse_stat(bad.as_bytes(), 42).unwrap();
        assert_eq!(s.ticks, 12345);
        assert_eq!((s.state, s.parent, s.group), (None, None, None));
    }

    #[test]
    fn bounded_reads_refuse_the_extra_byte_and_propagate_errors() {
        assert_eq!(read_bounded(b"abcd".as_slice(), 4).unwrap(), b"abcd");
        assert_eq!(
            read_bounded(b"abcde".as_slice(), 4).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
        struct Denied;
        impl Read for Denied {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::PermissionDenied.into())
            }
        }
        assert_eq!(
            read_bounded(Denied, 4).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
