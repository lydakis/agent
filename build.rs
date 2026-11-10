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
        // Only what is there is watched, since cargo takes a path that is
        // missing for one that changed and builds again every time: a
        // branch's own file is missing once its ref is packed, and
        // `packed-refs` until one is. The branch's file coming back, at the
        // next commit, changes the folder watched in its place.
        let path =
            |name: &str| git(&["rev-parse", "--git-path", name]).map(std::path::PathBuf::from);
        let watch = |file: &std::path::Path| println!("cargo:rerun-if-changed={}", file.display());
        for file in ["HEAD", "packed-refs"].into_iter().filter_map(path) {
            if file.exists() {
                watch(&file);
            }
        }
        if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]).and_then(|name| path(&name)) {
            match branch.exists() {
                true => watch(&branch),
                false => {
                    if let Some(heads) = path("refs/heads").filter(|heads| heads.exists()) {
                        watch(&heads);
                    }
                }
            }
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
