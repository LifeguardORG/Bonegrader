//! Path-safety guards.
//!
//! Manifest-provided paths are attacker/corruption controlled from the client's
//! point of view. A managed path must be exactly `<category>/<file>` inside one
//! of the three managed folders — no absolute paths, no `..`, no nesting, no
//! backslashes (which could hide traversal on Windows). Seed paths may be
//! nested but must stay inside a small allow-list of config locations.
//!
//! Every path component must also be a name Windows can store as a regular
//! file: no `<>:"|?*`, no control characters, no trailing dot/space and no
//! reserved device names such as `CON` or `nul.txt`.

use crate::manifest::Category;

pub const MANAGED_DIRS: [&str; 3] = ["mods", "resourcepacks", "shaderpacks"];

/// Top-level folders seed files may be placed in.
pub const SEED_DIRS: [&str; 3] = ["config", "defaultconfigs", "kubejs"];

/// Top-level files that may be seeded directly.
pub const SEED_FILES: [&str; 1] = ["options.txt"];

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
    let comps: Vec<&str> = path.split('/').collect();
    match comps.as_slice() {
        [dir, file] => MANAGED_DIRS.contains(dir) && is_safe_component(file),
        _ => false,
    }
}

/// Validate a seed path: `options.txt`, or a nested path below one of
/// [`SEED_DIRS`] (e.g. `config/jei/jei-client.ini`).
pub fn is_safe_seed_path(path: &str) -> bool {
    let comps: Vec<&str> = path.split('/').collect();
    if !comps.iter().all(|c| is_safe_component(c)) {
        return false;
    }
    match comps.as_slice() {
        [file] => SEED_FILES.contains(file),
        [dir, _, ..] => SEED_DIRS.contains(dir),
        [] => false,
    }
}

/// True if the path's category matches the folder it claims to live in.
pub fn path_matches_category(path: &str, cat: Category) -> bool {
    path.split('/').next() == Some(category_dir(cat))
}

/// A single path component that is safe to create on every supported OS.
pub fn is_safe_component(c: &str) -> bool {
    if c.is_empty() || c == "." || c == ".." || c.ends_with('.') || c.ends_with(' ') {
        return false;
    }
    if c.chars().any(|ch| {
        ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\' | '/')
    }) {
        return false;
    }
    !is_windows_reserved(c)
}

/// `CON`, `PRN`, `AUX`, `NUL`, `COM0`–`COM9`, `LPT0`–`LPT9` (also with the
/// superscripts ¹²³) — with or without an extension — name devices on Windows,
/// not files.
fn is_windows_reserved(c: &str) -> bool {
    let stem = c.split('.').next().unwrap_or(c).trim_end().to_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let chars: Vec<char> = stem.chars().collect();
    let prefix: String = chars.iter().take(3).collect();
    (prefix == "COM" || prefix == "LPT")
        && chars.len() == 4
        && (chars[3].is_ascii_digit() || matches!(chars[3], '¹' | '²' | '³'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_well_formed_paths() {
        assert!(is_safe_managed_path("mods/create.jar"));
        assert!(is_safe_managed_path("resourcepacks/pack.zip"));
        assert!(is_safe_managed_path("shaderpacks/bsl.zip"));
        // Real-world names with spaces, brackets and apostrophes.
        assert!(is_safe_managed_path(
            "mods/[1.21.1] SecurityCraft v1.9.10.jar"
        ));
        assert!(is_safe_managed_path("mods/jei (1).jar"));
        assert!(is_safe_managed_path("mods/Xaero's Minimap.jar"));
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
        assert!(!is_safe_managed_path("mods/.."));
        assert!(!is_safe_managed_path(""));
    }

    #[test]
    fn rejects_names_windows_cannot_store() {
        assert!(!is_safe_managed_path("mods/C:evil.jar"));
        assert!(!is_safe_managed_path("mods/what?.jar"));
        assert!(!is_safe_managed_path("mods/trailing."));
        assert!(!is_safe_managed_path("mods/trailing "));
        assert!(!is_safe_managed_path("mods/CON"));
        assert!(!is_safe_managed_path("mods/nul.jar"));
        assert!(!is_safe_managed_path("mods/com1.zip"));
        assert!(!is_safe_managed_path("mods/a\u{0}b.jar"));
        assert!(!is_safe_managed_path("mods/LPT0.zip"));
        assert!(!is_safe_managed_path("mods/com¹.jar"));
        assert!(is_safe_managed_path("mods/console.jar"));
        assert!(is_safe_managed_path("mods/com10.jar"));
    }

    #[test]
    fn seed_paths() {
        assert!(is_safe_seed_path("options.txt"));
        assert!(is_safe_seed_path("config/xaerominimap.txt"));
        assert!(is_safe_seed_path("config/jei/jei-client.ini"));
        assert!(is_safe_seed_path("kubejs/client_scripts/main.js"));
        assert!(is_safe_seed_path("defaultconfigs/create-server.toml"));

        assert!(!is_safe_seed_path("config"));
        assert!(!is_safe_seed_path("config/"));
        assert!(!is_safe_seed_path("config/../mods/evil.jar"));
        assert!(!is_safe_seed_path("mods/evil.jar"));
        assert!(!is_safe_seed_path("saves/world/level.dat"));
        assert!(!is_safe_seed_path("/config/a.toml"));
        assert!(!is_safe_seed_path("config\\a.toml"));
        assert!(!is_safe_seed_path("servers.dat"));
        assert!(!is_safe_seed_path(""));
    }

    #[test]
    fn category_matching() {
        assert!(path_matches_category("mods/x.jar", Category::Mod));
        assert!(!path_matches_category("mods/x.jar", Category::Shaderpack));
    }
}
