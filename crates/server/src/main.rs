//! CL5 Monitor Mix LAN server. The console bridge, web server and user store are built in later cards.

fn main() {
    println!("{}", cl5_banner());
}

fn cl5_banner() -> &'static str {
    "CL5 Monitor Mix server"
}

#[cfg(test)]
mod tests {
    #[test]
    fn banner_names_the_app() {
        assert_eq!(super::cl5_banner(), "CL5 Monitor Mix server");
    }
}
