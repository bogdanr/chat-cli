// Placeholder binary for the crates.io stub package.
//
// End users never run this. The real application is distributed as a prebuilt
// binary via GitHub Releases (and fetched by `cargo binstall chat-cli`). This
// stub exists only so the `chat-cli` name and per-version binstall metadata
// live on crates.io.
fn main() {
    eprintln!(
        "This crates.io package only provides `cargo binstall` metadata.\n\
         Install the app with:  cargo binstall chat-cli\n\
         Or download a prebuilt binary from:\n\
         https://github.com/bogdanr/chat-cli/releases"
    );
}
