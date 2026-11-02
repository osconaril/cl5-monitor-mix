//! CL5 Monitor Mix web app (compiled to WebAssembly). The user and admin screens are built in later cards.

/// The app's display name, shared by every screen.
pub fn app_name() -> &'static str {
    "CL5 Monitor Mix"
}

/// HTML emitted by this crate's build script and served by the LAN server.
pub const SHELL: &str = include_str!(concat!(env!("OUT_DIR"), "/index.html"));

#[cfg(test)]
mod tests {
    #[test]
    fn app_name_is_set() {
        assert_eq!(super::app_name(), "CL5 Monitor Mix");
    }

    #[test]
    fn generated_shell_has_a_phone_viewport() {
        assert!(super::SHELL.contains("width=device-width"));
        assert!(super::SHELL.contains("min-height: 44px"));
    }
}
