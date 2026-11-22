#![allow(clippy::unwrap_used, clippy::expect_used)]

/// `claude_code_spelling` drops Windows' `\\?\` verbatim prefix before the key:
/// `canonicalize` answers `\\?\C:\...` and Claude Code keys the plain `C:\...`
/// spelling, so `C--Users-u-repo`, never `----C--...`.
#[test]
fn a_windows_verbatim_prefix_is_dropped_before_keying() {
    assert_eq!(
        crate::canonical::claude_code_spelling(r"\\?\C:\Users\u\repo"),
        r"C:\Users\u\repo"
    );
    assert_eq!(
        crate::cc_exempt::project_key(&crate::canonical::claude_code_spelling(
            r"\\?\C:\Users\u\repo"
        )),
        "C--Users-u-repo"
    );
    assert_eq!(
        crate::canonical::claude_code_spelling(r"\\?\UNC\srv\share\repo"),
        r"\\srv\share\repo"
    );
    assert_eq!(
        crate::canonical::claude_code_spelling("/home/u/repo"),
        "/home/u/repo"
    );
}

#[cfg(windows)]
#[test]
fn plain_of_a_short_path_is_the_plain_spelling() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.txt");
    std::fs::write(&p, b"x").unwrap();
    let plain = crate::canonical::plain(&p).unwrap();
    assert_eq!(plain, crate::testutil::plain_path(&p));
    assert!(!plain.to_string_lossy().starts_with(r"\\?\"));
}

#[cfg(windows)]
#[test]
fn plain_of_a_long_path_keeps_the_verbatim_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = dir.path().to_path_buf();
    while p.to_string_lossy().encode_utf16().count() <= 260 {
        p.push("x-segment-x");
    }
    std::fs::create_dir_all(&p).unwrap();
    let plain = crate::canonical::plain(&p).unwrap();
    assert_eq!(plain, crate::testutil::plain_path(&p));
}

/// Codex keys hook trust with std's resolved `CODEX_HOME`, verbatim prefix and all, so the codex purpose must never take `plain`'s spelling.
#[cfg(windows)]
#[test]
fn codex_home_of_a_short_path_keeps_the_verbatim_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let resolved = crate::canonical::codex_home(dir.path()).unwrap();
    assert!(
        resolved.to_string_lossy().starts_with(r"\\?\"),
        "the verbatim prefix codex keys with stays: {resolved:?}"
    );
    assert_ne!(
        resolved,
        crate::canonical::plain(dir.path()).unwrap(),
        "a short path's plain spelling drops the prefix, so the two differ"
    );
}

#[cfg(windows)]
#[test]
fn display_of_an_identity_path_equals_plain() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.txt");
    std::fs::write(&p, b"x").unwrap();
    let ident = crate::canonical::identity(&p).unwrap();
    assert_eq!(
        crate::canonical::display(&ident),
        crate::canonical::plain(&p).unwrap().as_path()
    );
}
