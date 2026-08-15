// Shared build-script helper: resolve *what exactly this build is* and emit
// it as Rust source for `apps/build_info.rs` to include.
//
// `include!`d by `apps/crossover/build.rs` and `apps/crossover-svc/build.rs`
// rather than living in a workspace crate. Two reasons: creating a crate is
// an ADR-level decision (docs/adr/README.md), and — more importantly — the
// LocalSystem service binary's dependency graph is a security boundary
// (ADR 0011: `crossover-svc` links nothing that can process untrusted
// input). A shared *source file* gives both binaries the same identity with
// no edge added to either dependency graph.
//
// ## The version a build carries
//
// The Cargo version is the *release* version; what a binary reports is
// derived from the source state, so a build is never mistaken for a release:
//
// | channel       | when                          | example                      |
// |---------------|-------------------------------|------------------------------|
// | `release`     | tag `v<version>` points at HEAD | `0.1.0`                     |
// | `ci`          | `GITHUB_ACTIONS` is set        | `0.1.0-ci.42.1.g1a2b3c4d5`   |
// | `development` | anything else                  | `0.1.0-dev.7.g1a2b3c4d5.dirty` |
//
// Overrides, for build systems that already know the answer:
// `CROSSOVER_BUILD_VERSION`, `CROSSOVER_GIT_COMMIT`, `CROSSOVER_GIT_DIRTY`.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Name of the generated file inside `OUT_DIR`. `apps/build_info.rs` includes
/// it by this name.
const BUILD_INFO_FILE: &str = "crossover_build_info.rs";

/// Everything the binaries report about their own provenance.
struct BuildIdentity {
    release_version: String,
    version: String,
    channel: &'static str,
    git_commit: Option<String>,
    git_short_commit: Option<String>,
    git_branch: Option<String>,
    revision_count: Option<u64>,
    source_tag: Option<String>,
    dirty: bool,
    ci_run_id: Option<String>,
    ci_run_number: Option<u64>,
    ci_run_attempt: Option<u64>,
    ci_run_url: Option<String>,
    target: String,
    host: String,
    profile: String,
    opt_level: String,
    rustc: Option<String>,
}

/// Resolve the identity, write it into `OUT_DIR`, and register the inputs
/// that invalidate it. Returns it so the caller can stamp the Windows
/// version resource with the same values.
fn emit_build_identity() -> io::Result<BuildIdentity> {
    emit_rebuild_directives();
    let identity = resolve_build_identity();
    write_build_info(&identity)?;
    Ok(identity)
}

/// Tell Cargo what makes this build script's output stale. Without these a
/// binary keeps reporting the commit it was first built at.
fn emit_rebuild_directives() {
    for name in [
        "CROSSOVER_BUILD_VERSION",
        "CROSSOVER_GIT_COMMIT",
        "CROSSOVER_GIT_DIRTY",
        "GITHUB_ACTIONS",
        "GITHUB_REF_NAME",
        "GITHUB_REF_TYPE",
        "GITHUB_REPOSITORY",
        "GITHUB_RUN_ATTEMPT",
        "GITHUB_RUN_ID",
        "GITHUB_RUN_NUMBER",
        "GITHUB_SERVER_URL",
        "GITHUB_SHA",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }

    // This file itself is outside the package directory, so Cargo's default
    // "rerun on any package change" does not cover it.
    println!("cargo:rerun-if-changed=../build_identity.rs");
    println!("cargo:rerun-if-changed=../build_info.rs");

    // HEAD moves on checkout/commit; index moves when the worktree is
    // staged, which is the cheap proxy for "dirty may have changed".
    let git_directory = repository_root().join(".git");
    println!(
        "cargo:rerun-if-changed={}",
        git_directory.join("HEAD").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        git_directory.join("index").display()
    );
}

/// What the source tree says about itself.
struct SourceState {
    commit: Option<String>,
    short_commit: Option<String>,
    branch: Option<String>,
    revision_count: Option<u64>,
    /// The release tag on the built commit — the sole thing that makes a
    /// build a release.
    source_tag: Option<String>,
    dirty: bool,
}

/// What GitHub Actions says about the run, if this is one.
struct CiState {
    id: Option<String>,
    number: Option<u64>,
    attempt: Option<u64>,
    url: Option<String>,
}

fn resolve_build_identity() -> BuildIdentity {
    let release_version = env::var("CARGO_PKG_VERSION").expect("Cargo package version");
    let source = resolve_source_state(&repository_root(), &release_version);
    let ci = resolve_ci_state();

    let channel = if source.source_tag.is_some() {
        "release"
    } else if nonempty_env("GITHUB_ACTIONS").is_some() {
        "ci"
    } else {
        "development"
    };
    if channel == "release" {
        // A release binary that cannot name its own source is unauditable,
        // and one built from uncommitted edits is unreproducible.
        assert!(!source.dirty, "release builds require a clean worktree");
        assert!(
            source.commit.is_some(),
            "release builds require a Git commit"
        );
    }

    // An explicit version wins, for build systems that already decided.
    let version = nonempty_env("CROSSOVER_BUILD_VERSION")
        .unwrap_or_else(|| build_version(channel, &release_version, &source, &ci));

    BuildIdentity {
        release_version,
        version,
        channel,
        git_commit: source.commit,
        git_short_commit: source.short_commit,
        git_branch: source.branch,
        revision_count: source.revision_count,
        source_tag: source.source_tag,
        dirty: source.dirty,
        ci_run_id: ci.id,
        ci_run_number: ci.number,
        ci_run_attempt: ci.attempt,
        ci_run_url: ci.url,
        target: env::var("TARGET").expect("Cargo target triple"),
        host: env::var("HOST").expect("Cargo host triple"),
        profile: cargo_profile(),
        opt_level: env::var("OPT_LEVEL").unwrap_or_else(|_| "unknown".to_owned()),
        rustc: rustc_version(),
    }
}

/// Read the source state, preferring what the environment asserts over what
/// git reports — a build system that unpacked a tarball knows things git
/// cannot see from inside it.
fn resolve_source_state(repository: &Path, release_version: &str) -> SourceState {
    let commit = nonempty_env("CROSSOVER_GIT_COMMIT")
        .or_else(|| nonempty_env("GITHUB_SHA"))
        .or_else(|| git(repository, &["rev-parse", "HEAD"]));
    let short_commit = commit
        .as_deref()
        .map(|commit| commit.chars().take(9).collect::<String>());
    let branch = nonempty_env("GITHUB_REF_NAME")
        .or_else(|| git(repository, &["rev-parse", "--abbrev-ref", "HEAD"]))
        // A detached HEAD names no branch; "HEAD" would be a lie.
        .filter(|branch| branch != "HEAD");
    let dirty = nonempty_env("CROSSOVER_GIT_DIRTY").map_or_else(
        || {
            git(
                repository,
                &["status", "--porcelain", "--untracked-files=normal"],
            )
            .is_some_and(|status| !status.is_empty())
        },
        |value| parse_bool(&value, "CROSSOVER_GIT_DIRTY"),
    );

    // A build is a release only if the matching tag is on the commit being
    // built. "Tagged something, once" is not enough.
    let expected_tag = format!("v{release_version}");
    let tagged_here = tags_at_head(repository)
        .into_iter()
        .find(|tag| tag == &expected_tag);
    let github_tag = (nonempty_env("GITHUB_REF_TYPE").as_deref() == Some("tag"))
        .then(|| nonempty_env("GITHUB_REF_NAME"))
        .flatten();
    if let Some(tag) = github_tag.as_deref() {
        assert_eq!(
            tag, expected_tag,
            "release tag {tag} must match the workspace version {release_version}"
        );
    }

    SourceState {
        commit,
        short_commit,
        branch,
        revision_count: revision_count(repository),
        source_tag: tagged_here.or(github_tag),
        dirty,
    }
}

fn resolve_ci_state() -> CiState {
    let id = nonempty_env("GITHUB_RUN_ID");
    let url = match (
        nonempty_env("GITHUB_SERVER_URL"),
        nonempty_env("GITHUB_REPOSITORY"),
        id.as_deref(),
    ) {
        (Some(server), Some(repository), Some(run_id)) => {
            Some(format!("{server}/{repository}/actions/runs/{run_id}"))
        }
        _ => None,
    };
    CiState {
        id,
        number: optional_u64_env("GITHUB_RUN_NUMBER"),
        attempt: optional_u64_env("GITHUB_RUN_ATTEMPT"),
        url,
    }
}

/// Compose the version this build reports. Only a tagged release gets the
/// bare release version; everything else says what it is and where it came
/// from, so a binary in the wild is always traceable to a commit.
fn build_version(
    channel: &str,
    release_version: &str,
    source: &SourceState,
    ci: &CiState,
) -> String {
    let commit = source.short_commit.as_deref().unwrap_or("unknown");
    match channel {
        "release" => release_version.to_owned(),
        "ci" => format!(
            "{release_version}-ci.{}.{}.g{commit}",
            ci.number.unwrap_or(0),
            ci.attempt.unwrap_or(1),
        ),
        _ => {
            let mut version = format!(
                "{release_version}-dev.{}.g{commit}",
                source.revision_count.unwrap_or(0),
            );
            if source.dirty {
                version.push_str(".dirty");
            }
            version
        }
    }
}

/// Stamp the same identity into the Windows version resource, so a deployed
/// exe answers "what is this?" to Explorer, `Get-FileVersionInfo`, and the
/// packaging script — without being run. That last consumer is why a failure
/// here is fatal rather than a warning: `scripts/build.ps1` verifies the two
/// binaries carry matching metadata, and a silently unstamped exe would fail
/// packaging with a far less obvious message.
#[cfg(windows)]
fn stamp_windows_resource(identity: &BuildIdentity, description: &str) -> io::Result<()> {
    const ICON: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/branding/crossover.ico"
    );
    println!("cargo:rerun-if-changed=../../assets/branding/crossover.ico");

    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(ICON)
        .set("ProductName", "Crossover")
        .set("FileDescription", description)
        .set("FileVersion", &identity.version)
        .set("ProductVersion", &identity.version)
        .set("LegalCopyright", "MIT licensed. See LICENSE.");
    if let Some(commit) = identity.git_commit.as_deref() {
        resource.set("Comments", &format!("Git commit: {commit}"));
    }
    // Mark anything that is not a tagged release as what it is, so a
    // pre-release binary cannot masquerade as a shipped one in the shell's
    // properties dialog.
    if identity.channel != "release" {
        resource
            .set("PrivateBuild", &identity.version)
            .set_version_info(
                winresource::VersionInfo::FILEFLAGS,
                winresource::VersionInfo::VS_FF_PRERELEASE
                    | winresource::VersionInfo::VS_FF_PRIVATEBUILD,
            );
    }
    resource.compile()
}

/// Write the identity as a `const BuildInfo` initializer. `{:?}` on `&str`
/// and `Option<&str>` is the escaping — the values reach the source file as
/// valid Rust literals whatever they contain.
fn write_build_info(identity: &BuildIdentity) -> io::Result<()> {
    let mut contents = String::new();
    writeln!(contents, "pub const BUILD_INFO: BuildInfo = BuildInfo {{")
        .expect("writing to a String cannot fail");
    for (field, value) in [
        ("release_version", format!("{:?}", identity.release_version)),
        ("version", format!("{:?}", identity.version)),
        ("channel", format!("{:?}", identity.channel)),
        ("git_commit", format!("{:?}", identity.git_commit)),
        (
            "git_short_commit",
            format!("{:?}", identity.git_short_commit),
        ),
        ("git_branch", format!("{:?}", identity.git_branch)),
        ("revision_count", format!("{:?}", identity.revision_count)),
        ("source_tag", format!("{:?}", identity.source_tag)),
        ("dirty", format!("{}", identity.dirty)),
        ("ci_run_id", format!("{:?}", identity.ci_run_id)),
        ("ci_run_number", format!("{:?}", identity.ci_run_number)),
        ("ci_run_attempt", format!("{:?}", identity.ci_run_attempt)),
        ("ci_run_url", format!("{:?}", identity.ci_run_url)),
        ("target", format!("{:?}", identity.target)),
        ("host", format!("{:?}", identity.host)),
        ("profile", format!("{:?}", identity.profile)),
        ("opt_level", format!("{:?}", identity.opt_level)),
        ("rustc", format!("{:?}", identity.rustc)),
    ] {
        writeln!(contents, "    {field}: {value},").expect("writing to a String cannot fail");
    }
    writeln!(contents, "}};").expect("writing to a String cannot fail");

    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"))
            .join(BUILD_INFO_FILE),
        contents,
    )
}

/// `apps/<crate>` → the workspace root.
fn repository_root() -> PathBuf {
    PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
        .join("..")
        .join("..")
}

/// The directory name under `target/` — the real profile name (`dist`,
/// `release`, `debug`), which `PROFILE` flattens to just release/debug.
fn cargo_profile() -> String {
    let out_directory = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    out_directory
        .ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == "build"))
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map_or_else(|| env::var("PROFILE").expect("Cargo profile"), str::to_owned)
}

/// Commits since the last release tag — a monotonic "how far past the last
/// release is this", which a bare commit hash does not convey.
fn revision_count(repository: &Path) -> Option<u64> {
    let latest_tag = git(
        repository,
        &["describe", "--tags", "--match", "v[0-9]*", "--abbrev=0"],
    );
    let range = latest_tag.as_deref().map(|tag| format!("{tag}..HEAD"));
    let arguments = match range.as_deref() {
        Some(range) => vec!["rev-list", "--count", range],
        None => vec!["rev-list", "--count", "HEAD"],
    };
    git(repository, &arguments)?.parse().ok()
}

fn tags_at_head(repository: &Path) -> Vec<String> {
    git(repository, &["tag", "--points-at", "HEAD"])
        .map(|tags| tags.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// The compiler that produced this binary, e.g. `rustc 1.97.0 (abcdef 2026-01-01)`.
fn rustc_version() -> Option<String> {
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc).arg("--version").output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
        .map(|version| version.trim().to_owned())
        .filter(|version| !version.is_empty())
}

/// Run git, returning trimmed stdout — or `None` for any failure at all. A
/// source tree without git (an unpacked tarball, a Docker context) must still
/// build; it simply reports less.
fn git(repository: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

fn nonempty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn optional_u64_env(name: &str) -> Option<u64> {
    nonempty_env(name).map(|value| {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be an unsigned integer"))
    })
}

fn parse_bool(value: &str, name: &str) -> bool {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" => true,
        "0" | "false" => false,
        _ => panic!("{name} must be true or false"),
    }
}
