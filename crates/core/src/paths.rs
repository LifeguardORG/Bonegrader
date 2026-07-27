//! Path-safety guards.
//!
//! Manifest-provided paths are attacker/corruption controlled from the client's
//! point of view. A managed path must be exactly `<category>/<file>` inside one
//! of the three managed folders — no absolute paths, no `..`, no nesting, no
//! backslashes (which could hide traversal on Windows).

use crate::manifest::Category;

pub const MANAGED_DIRS: [&str; 3] = ["mods", "resourcepacks", "shaderpacks"];

/// The folder name for a category.
pub fn category_dir(cat: Category) -> &'static str {
    match cat {
        Category::Mod => "mods",
        Category::Resourcepack => "resourcepacks",
        Category::Shaderpack => "shaderpacks",
    }
}

/// Validate a client-relative managed path. Must be `dir/file` where `dir` is a
/// managed folder and `file` is a single, safe file name.
pub fn is_safe_managed_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return false;
    }
    let mut comps = path.split('/');
    let dir = match comps.next() {
        Some(d) => d,
        None => return false,
    };
    if !MANAGED_DIRS.contains(&dir) {
        return false;
    }
    let file = match comps.next() {
        Some(f) => f,
        None => return false, // needs a file component
    };
    if comps.next().is_some() {
        return false; // no nested subfolders
    }
    !(file.is_empty() || file == "." || file == "..")
}

/// True if the path's category matches the folder it claims to live in.
pub fn path_matches_category(path: &str, cat: Category) -> bool {
    path.split('/').next() == Some(category_dir(cat))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_well_formed_paths() {
        assert!(is_safe_managed_path("mods/create.jar"));
        assert!(is_safe_managed_path("resourcepacks/pack.zip"));
        assert!(is_safe_managed_path("shaderpacks/bsl.zip"));
    }

    #[test]
    fn rejects_traversal_and_absolute_and_foreign_dirs() {
        assert!(!is_safe_managed_path("../mods/evil.jar"));
        assert!(!is_safe_managed_path("mods/../../etc/passwd"));
        assert!(!is_safe_managed_path("/etc/passwd"));
        assert!(!is_safe_managed_path("mods\\evil.jar"));
        assert!(!is_safe_managed_path("config/foo.toml"));
        assert!(!is_safe_managed_path("mods"));
        assert!(!is_safe_managed_path("mods/"));
        assert!(!is_safe_managed_path("mods/sub/foo.jar"));
        assert!(!is_safe_managed_path(""));
    }

    #[test]
    fn category_matching() {
        assert!(path_matches_category("mods/x.jar", Category::Mod));
        assert!(!path_matches_category("mods/x.jar", Category::Shaderpack));
    }
}
