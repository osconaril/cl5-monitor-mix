//! CL5 Monitor Mix web app (compiled to WebAssembly). The user and admin screens are built in later cards.

/// The app's display name, shared by every screen.
pub fn app_name() -> &'static str {
    "CL5 Monitor Mix"
}

#[cfg(test)]
mod tests {
    #[test]
    fn app_name_is_set() {
        assert_eq!(super::app_name(), "CL5 Monitor Mix");
    }
}
