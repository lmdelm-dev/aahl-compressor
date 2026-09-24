//! Menu construction and verb resolution for the AAHL context menu.

/// Cap on how many selected items are scanned for an archive.
pub const MAX_ARCHIVES: usize = 4;

/// The verbs the AAHL shell extension can offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// Add the selected files to a new archive (`aahl create`).
    Add,
    /// Extract the archive into the current folder.
    ExtractHere,
    /// Extract the archive into a per-archive sub-folder.
    ExtractTo,
    /// Validate the archive with `aahl test`.
    Test,
}

impl Verb {
    /// Human-readable label shown in the context menu. `stem` is only used
    /// by the "Extract to" verb.
    pub fn label(&self, stem: Option<&str>) -> String {
        match self {
            Verb::Add => "Add to AAHL archive…".to_owned(),
            Verb::ExtractHere => "Extract here".to_owned(),
            Verb::ExtractTo => match stem {
                Some(stem) => format!("Extract to {stem}\\…"),
                None => "Extract to…".to_owned(),
            },
            Verb::Test => "Test archive with AAHL".to_owned(),
        }
    }

    /// Short ASCII name reported through `IContextMenu::GetCommandString`.
    pub fn name(&self) -> &'static [u8] {
        match self {
            Verb::Add => b"aahladd",
            Verb::ExtractHere => b"aahlextracthere",
            Verb::ExtractTo => b"aahlextractto",
            Verb::Test => b"aahltest",
        }
    }
}

/// Case-insensitive ".aahl" suffix test.
pub fn is_archive(path: &str) -> bool {
    const SUFFIX: &str = ".aahl";
    let bytes = path.as_bytes();
    if bytes.len() < SUFFIX.len() {
        return false;
    }
    bytes[bytes.len() - SUFFIX.len()..].eq_ignore_ascii_case(SUFFIX.as_bytes())
}

/// Sanitised archive stem (file name minus extension), trimmed of trailing
/// spaces and dots, falling back to "archive".
pub fn stem(path: &str) -> String {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let dot = file.rfind('.').unwrap_or(file.len());
    let base = &file[..dot];
    let trimmed = base.trim_end_matches([' ', '.']);
    if trimmed.is_empty() {
        "archive".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Decide which verbs apply to `selection` and, when an archive is present
/// among the first [`MAX_ARCHIVES`] items, the sanitised stem used to build
/// the "Extract to" target folder.
pub fn build(selection: &[String]) -> (Vec<Verb>, Option<String>) {
    if selection.is_empty() {
        return (Vec::new(), None);
    }
    let mut verbs = vec![Verb::Add];
    let mut stem_out = None;
    for path in selection.iter().take(MAX_ARCHIVES) {
        if is_archive(path) {
            verbs.push(Verb::ExtractHere);
            verbs.push(Verb::ExtractTo);
            verbs.push(Verb::Test);
            stem_out = Some(stem(path));
            break;
        }
    }
    (verbs, stem_out)
}

/// Map a command id back to a verb. `first` is the first id handed out by
/// `QueryContextMenu`. Two forms are accepted: ids relative to `first`, and
/// bare zero-based offsets passed by callers constructing
/// `CMINVOKECOMMANDINFO` directly.
pub fn verb_from_id(verbs: &[Verb], id: u32, first: u32) -> Option<Verb> {
    if id >= first {
        if let Some(&verb) = verbs.get(id.wrapping_sub(first) as usize) {
            return Some(verb);
        }
    }
    verbs.get(id as usize).copied()
}

/// Trim at the first NUL byte.
fn strip_nul(mut bytes: &[u8]) -> &[u8] {
    if let Some(index) = bytes.iter().position(|&byte| byte == 0) {
        bytes = &bytes[..index];
    }
    bytes
}

/// Map an ANSI verb name to a [`Verb`].
pub fn verb_from_name(name: &[u8]) -> Option<Verb> {
    match strip_nul(name) {
        b"aahladd" => Some(Verb::Add),
        b"aahlextracthere" => Some(Verb::ExtractHere),
        b"aahlextractto" => Some(Verb::ExtractTo),
        b"aahltest" => Some(Verb::Test),
        _ => None,
    }
}

/// Map a UTF-16 verb name to a [`Verb`]. The verb names are pure ASCII, so a
/// non-ASCII unit makes the lookup fail.
pub fn verb_from_name_wide(units: &[u16]) -> Option<Verb> {
    let mut bytes = Vec::with_capacity(units.len());
    for &unit in units {
        if unit == 0 {
            break;
        }
        if unit >= 0x80 {
            return None;
        }
        bytes.push(unit as u8);
    }
    verb_from_name(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    #[test]
    fn build_empty_selection() {
        assert_eq!(build(&[]), (Vec::new(), None));
    }

    #[test]
    fn build_plain_selection() {
        let (verbs, stem) = build(&sel(&["c:\\x\\a.txt", "c:\\x\\b.txt"]));
        assert_eq!(verbs, vec![Verb::Add]);
        assert_eq!(stem, None);
    }

    #[test]
    fn build_with_archive() {
        let (verbs, stem) = build(&sel(&["c:\\x\\a.txt", "c:\\x\\backup.aahl"]));
        assert_eq!(
            verbs,
            vec![Verb::Add, Verb::ExtractHere, Verb::ExtractTo, Verb::Test]
        );
        assert_eq!(stem.as_deref(), Some("backup"));
    }

    #[test]
    fn build_archive_case_insensitive() {
        let (verbs, stem) = build(&sel(&["C:\\x\\PHOTOS.AAHL"]));
        assert!(verbs.contains(&Verb::ExtractHere));
        assert_eq!(stem.as_deref(), Some("PHOTOS"));
    }

    #[test]
    fn build_scans_only_first_batch() {
        let (verbs, _stem) = build(&sel(&["a", "b", "c", "d", "e.aahl"]));
        assert_eq!(verbs, vec![Verb::Add]);
    }

    #[test]
    fn stem_behavior() {
        assert_eq!(stem("x.tar.aahl"), "x.tar");
        assert_eq!(stem("archive.AAHL"), "archive");
        assert_eq!(stem("photos."), "photos");
        assert_eq!(stem(".aahl"), "archive");
        assert_eq!(stem("   "), "archive");
        assert_eq!(stem("dir\\sub\\thing.aahl"), "thing");
    }

    #[test]
    fn archive_suffix_checks() {
        assert!(is_archive("backup.aahl"));
        assert!(is_archive("BACKUP.AAHL"));
        assert!(!is_archive("backup.aahl.txt"));
        assert!(!is_archive("aahl"));
        assert!(!is_archive(""));
    }

    #[test]
    fn ids_relative_and_bare() {
        let verbs = [Verb::Add, Verb::ExtractHere, Verb::ExtractTo, Verb::Test];
        assert_eq!(verb_from_id(&verbs, 100, 100), Some(Verb::Add));
        assert_eq!(verb_from_id(&verbs, 103, 100), Some(Verb::Test));
        assert_eq!(verb_from_id(&verbs, 104, 100), None);
        assert_eq!(verb_from_id(&verbs, 2, 100), Some(Verb::ExtractTo));
        assert_eq!(verb_from_id(&verbs, 7, 100), None);
    }

    #[test]
    fn names_ansi_and_wide() {
        assert_eq!(verb_from_name(b"aahladd"), Some(Verb::Add));
        assert_eq!(verb_from_name(b"aahlextracthere"), Some(Verb::ExtractHere));
        assert_eq!(verb_from_name(b"aahladd\0rest"), Some(Verb::Add));
        assert_eq!(verb_from_name(b"bogus"), None);
        let wide: Vec<u16> = b"aahltest".iter().map(|&b| b as u16).collect();
        assert_eq!(verb_from_name_wide(&wide), Some(Verb::Test));
        assert_eq!(verb_from_name_wide(&[0x100]), None);
        assert_eq!(verb_from_name_wide(&[0]), None);
    }
}