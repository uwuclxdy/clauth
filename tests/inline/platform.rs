//! The OSC 52 clipboard escape behind the manual login's `c` key, pinned on a
//! writer so no test touches the real stdout.

use super::{base64_std, write_osc52};

/// RFC 4648 §10 test vectors: every padding shape, plus the `+` and `/`
/// characters `base64url` would spell differently.
#[test]
fn base64_std_matches_rfc4648_vectors() {
    assert_eq!(base64_std(b""), "");
    assert_eq!(base64_std(b"f"), "Zg==");
    assert_eq!(base64_std(b"fo"), "Zm8=");
    assert_eq!(base64_std(b"foo"), "Zm9v");
    assert_eq!(base64_std(b"foob"), "Zm9vYg==");
    assert_eq!(base64_std(b"fooba"), "Zm9vYmE=");
    assert_eq!(base64_std(b"foobar"), "Zm9vYmFy");
    assert_eq!(base64_std(&[0xfb, 0xff]), "+/8=");
}

#[test]
fn write_osc52_emits_the_whole_escape() {
    let mut buf = Vec::new();
    write_osc52(&mut buf, "https://x/?a=1&b=2").expect("write");
    assert_eq!(buf, b"\x1b]52;c;aHR0cHM6Ly94Lz9hPTEmYj0y\x07");
}

/// A writer that refuses is reported, not swallowed: the modal's toast must
/// not claim a copy that never left the process.
#[test]
fn write_osc52_reports_the_writer_error() {
    struct Refuse;
    impl std::io::Write for Refuse {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("tty gone"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let err = write_osc52(&mut Refuse, "x").expect_err("must surface");
    assert_eq!(err.to_string(), "tty gone");
}
