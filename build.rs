// Tell cargo to re-run the central-logs build whenever the embedded SPA dist
// changes. Without this, rust-embed's snapshot of target/web-dist goes stale
// after `npm run build` because cargo doesn't track files outside src/.
fn main() {
    println!("cargo:rerun-if-changed=target/web-dist");
}
