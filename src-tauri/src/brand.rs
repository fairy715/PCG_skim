//! What this build calls itself.
//!
//! Fork addition. Upstream is "Skim"; this build ships as a separate product
//! that can be installed next to it. Everything that identifies the app to
//! Windows lives here, in one place, so the two never share a data folder, a
//! saved password, a toast identity or a link scheme — and so a merge from
//! upstream that reintroduces a literal is easy to spot.
//!
//! Internal names (the crate, log file names, the `skim-cid` image protocol)
//! are left alone: nobody sees them, and renaming them would only make every
//! upstream merge harder.

/// Name shown to the user: window title, tray, notifications, installer.
/// Must match `productName` in `tauri.conf.json`.
pub const NAME: &str = "PCG Mail";

/// Application identifier. Names the data folder and the toast identity, and
/// keeps a second copy from starting. Must match `identifier` in
/// `tauri.conf.json`.
pub const IDENTIFIER: &str = "com.pcg.mail";

/// Scheme of the links notifications use to open the app (`pcgmail://open…`).
pub const URL_SCHEME: &str = "pcgmail";

/// Prefix of an activation link, as it arrives on the command line.
pub const URL_PREFIX: &str = "pcgmail://";

/// Service name under which passwords are kept in Windows Credential Manager.
pub const CREDENTIAL_SERVICE: &str = "PCG Mail";

/// Folder in the user's profile that holds the notification icon.
pub const PROFILE_DIR: &str = ".pcgmail";

/// Where the project lives, for attribution headers and the about link.
pub const HOMEPAGE: &str = "https://github.com/fairy715/pcg_skim";

#[cfg(test)]
mod tests {
    use super::*;

    /// `tauri.conf.json` is the other place these two are written down; the
    /// data folder and single-instance lock follow the file, the toast
    /// identity and log folder follow the constants. They must agree.
    #[test]
    fn the_constants_match_the_tauri_config() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(config["productName"], NAME);
        assert_eq!(config["identifier"], IDENTIFIER);
        assert_eq!(config["app"]["windows"][0]["title"], NAME);
    }

    #[test]
    fn nothing_is_shared_with_upstream() {
        assert_ne!(IDENTIFIER, "com.skim.app");
        assert_ne!(URL_SCHEME, "skim");
        assert_ne!(CREDENTIAL_SERVICE, "Skim");
        assert_eq!(URL_PREFIX, format!("{URL_SCHEME}://"));
    }
}
