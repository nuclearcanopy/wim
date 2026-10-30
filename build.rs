use std::process::Command;

/// Stamps the commit the binary was built from into `WIM_BUILD`, so
/// "which build am I actually running" is answerable from inside the
/// app rather than inferred from file timestamps. Several rounds of
/// "the new thing isn't there" turned out to be a stale binary, which
/// is not a question anyone should have to guess at.
///
/// Best effort throughout: a packaged build may have no `.git` at all,
/// and `unknown` is a better answer than a failed build.
fn main() {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
    };

    let hash = git(&["rev-parse", "--short=8", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // uncommitted work means the binary is not any commit, which is
    // worth saying outright: it is the state where a stale build and a
    // fresh one are hardest to tell apart.
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.trim().is_empty());

    println!(
        "cargo:rustc-env=WIM_BUILD={hash}{}",
        if dirty { "-dirty" } else { "" }
    );

    // the answer changes when HEAD moves or the index does, and
    // nothing else here depends on the tree.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
