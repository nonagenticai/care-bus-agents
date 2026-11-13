// sqlx::migrate!("./migrations") embeds the migration files at compile time. Without this, adding a
// new .sql file does not trigger a rebuild on stable Rust, and the binary silently ships without it.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
