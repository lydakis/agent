//! The commit a build from a checkout was made at, for `--version`: a
//! local build says which it is, as cargo's and rustc's do. A build from
//! a source archive has none.
fn main() {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .filter(|text| !text.is_empty())
    };
    // Only this repository's own checkout: a source archive unpacked
    // inside another repository is no checkout of this one.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".");
    let ours = git(&["rev-parse", "--show-toplevel"])
        .zip(root.canonicalize().ok())
        .is_some_and(|(top, root)| std::path::Path::new(&top) == root);
    if let Some(commit) = git(&["rev-parse", "--short=9", "HEAD"]).filter(|_| ours) {
        println!("cargo:rustc-env=AGENT_COMMIT={commit}");
        // Built again once HEAD moves: the file HEAD names, its branch's.
        for path in [
            Some("HEAD".to_owned()),
            git(&["symbolic-ref", "-q", "HEAD"]),
        ]
        .into_iter()
        .flatten()
        .chain(["packed-refs".to_owned()])
        {
            if let Some(file) = git(&["rev-parse", "--git-path", &path]) {
                println!("cargo:rerun-if-changed={file}");
            }
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
