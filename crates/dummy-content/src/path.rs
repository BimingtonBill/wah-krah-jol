//! Validation for archive member paths.

use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};

/// Splits an archive member name into its folder and file components.
///
/// Names are validated as canonical, relative, printable-ASCII asset paths
/// with at least one folder component, so generated archives can never escape
/// a virtual file system root when extracted.
pub(crate) fn split_asset_name<'a>(name: &'a str, context: &str) -> Result<(&'a str, &'a str)> {
    ensure!(!name.is_empty(), "{context} entry name is empty");
    ensure!(
        name.bytes().all(|byte| (0x20..0x7f).contains(&byte)),
        "{context} entry name contains non-printable or non-ASCII bytes: {name:?}"
    );
    ensure!(
        !name.contains('\\') && !name.contains(':'),
        "{context} entry name is not a relative POSIX path: {name:?}"
    );
    ensure!(
        !name.starts_with('/') && !name.ends_with('/'),
        "{context} entry name has an empty component: {name:?}"
    );
    ensure!(
        name.split('/')
            .all(|component| !component.is_empty() && component != "." && component != ".."),
        "{context} entry name contains a traversal component: {name:?}"
    );
    let (folder, file) = name
        .rsplit_once('/')
        .ok_or_else(|| eyre!("{context} entry name has no folder component: {name:?}"))?;
    debug_assert!(!folder.is_empty() && !file.is_empty());
    Ok((folder, file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_nested_asset_paths() {
        assert_eq!(
            split_asset_name("meshes/Actors/Dragon/dragon.nif", "BSA").unwrap(),
            ("meshes/Actors/Dragon", "dragon.nif")
        );
    }

    #[test]
    fn rejects_unsafe_names() {
        for name in [
            "",
            "textures/test.dds/",
            "/textures/test.dds",
            "textures\\test.dds",
            "C:/textures/test.dds",
            "textures/../test.dds",
            "textures/./test.dds",
            "textures//test.dds",
            "test.dds",
            "textures/te\u{7f}st.dds",
            "textures/t\u{e9}st.dds",
        ] {
            assert!(
                split_asset_name(name, "BSA").is_err(),
                "name {name:?} was accepted"
            );
        }
    }
}
