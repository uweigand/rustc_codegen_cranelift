use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::build_sysroot;
use crate::config;
use crate::path::{Dirs, RelPath};
use crate::prepare::{apply_patches, GitRepo};
use crate::rustc_info::get_default_sysroot;
use crate::shared_utils::rustflags_from_env;
use crate::utils::{spawn_and_wait, CargoProject, Compiler, LogGroup};
use crate::{CodegenBackend, SysrootKind};

static BUILD_EXAMPLE_OUT_DIR: RelPath = RelPath::BUILD.join("example");

struct TestCase {
    config: &'static str,
    cmd: TestCaseCmd,
}

enum TestCaseCmd {
    Custom { func: &'static dyn Fn(&TestRunner<'_>) },
    BuildLib { source: &'static str, crate_types: &'static str },
    BuildBin { source: &'static str },
    BuildBinAndRun { source: &'static str, args: &'static [&'static str] },
    JitBin { source: &'static str, args: &'static str },
}

impl TestCase {
    // FIXME reduce usage of custom test case commands
    const fn custom(config: &'static str, func: &'static dyn Fn(&TestRunner<'_>)) -> Self {
        Self { config, cmd: TestCaseCmd::Custom { func } }
    }

    const fn build_lib(
        config: &'static str,
        source: &'static str,
        crate_types: &'static str,
    ) -> Self {
        Self { config, cmd: TestCaseCmd::BuildLib { source, crate_types } }
    }

    const fn build_bin(config: &'static str, source: &'static str) -> Self {
        Self { config, cmd: TestCaseCmd::BuildBin { source } }
    }

    const fn build_bin_and_run(
        config: &'static str,
        source: &'static str,
        args: &'static [&'static str],
    ) -> Self {
        Self { config, cmd: TestCaseCmd::BuildBinAndRun { source, args } }
    }

    const fn jit_bin(config: &'static str, source: &'static str, args: &'static str) -> Self {
        Self { config, cmd: TestCaseCmd::JitBin { source, args } }
    }
}

const NO_SYSROOT_SUITE: &[TestCase] = &[
    TestCase::build_lib("build.mini_core", "example/mini_core.rs", "lib,dylib"),
    TestCase::build_lib("build.example", "example/example.rs", "lib"),
    TestCase::jit_bin("jit.mini_core_hello_world", "example/mini_core_hello_world.rs", "abc bcd"),
    TestCase::build_bin_and_run(
        "aot.mini_core_hello_world",
        "example/mini_core_hello_world.rs",
        &["abc", "bcd"],
    ),
];

const BASE_SYSROOT_SUITE: &[TestCase] = &[
    TestCase::build_bin_and_run(
        "aot.arbitrary_self_types_pointers_and_wrappers",
        "example/arbitrary_self_types_pointers_and_wrappers.rs",
        &[],
    ),
    TestCase::build_lib("build.alloc_system", "example/alloc_system.rs", "lib"),
    TestCase::build_bin_and_run("aot.alloc_example", "example/alloc_example.rs", &[]),
    TestCase::jit_bin("jit.std_example", "example/std_example.rs", ""),
    TestCase::build_bin_and_run("aot.std_example", "example/std_example.rs", &["arg"]),
    TestCase::build_bin_and_run("aot.dst_field_align", "example/dst-field-align.rs", &[]),
    TestCase::build_bin_and_run(
        "aot.subslice-patterns-const-eval",
        "example/subslice-patterns-const-eval.rs",
        &[],
    ),
    TestCase::build_bin_and_run(
        "aot.track-caller-attribute",
        "example/track-caller-attribute.rs",
        &[],
    ),
    TestCase::build_bin_and_run("aot.float-minmax-pass", "example/float-minmax-pass.rs", &[]),
    TestCase::build_bin_and_run("aot.mod_bench", "example/mod_bench.rs", &[]),
    TestCase::build_bin_and_run("aot.issue-72793", "example/issue-72793.rs", &[]),
    TestCase::build_bin("aot.issue-59326", "example/issue-59326.rs"),
    TestCase::custom("aot.polymorphize_coroutine", &|runner| {
        runner.run_rustc(&["example/polymorphize_coroutine.rs", "-Zpolymorphize"]);
        runner.run_out_command("polymorphize_coroutine", &[]);
    }),
    TestCase::build_bin_and_run("aot.neon", "example/neon.rs", &[]),
    TestCase::custom("aot.gen_block_iterate", &|runner| {
        runner.run_rustc([
            "example/gen_block_iterate.rs",
            "--edition",
            "2024",
            "-Zunstable-options",
        ]);
        runner.run_out_command("gen_block_iterate", &[]);
    }),
];

pub(crate) static RAND_REPO: GitRepo = GitRepo::github(
    "rust-random",
    "rand",
    "1f4507a8e1cf8050e4ceef95eeda8f64645b6719",
    "981f8bf489338978",
    "rand",
);

pub(crate) static RAND: CargoProject = CargoProject::new(&RAND_REPO.source_dir(), "rand_target");

pub(crate) static REGEX_REPO: GitRepo = GitRepo::github(
    "rust-lang",
    "regex",
    "061ee815ef2c44101dba7b0b124600fcb03c1912",
    "dc26aefbeeac03ca",
    "regex",
);

pub(crate) static REGEX: CargoProject = CargoProject::new(&REGEX_REPO.source_dir(), "regex_target");

pub(crate) static PORTABLE_SIMD_REPO: GitRepo = GitRepo::github(
    "rust-lang",
    "portable-simd",
    "f505736e45e256efcb36bf85b7f920ccef320435",
    "b012e6b58fa0f78d",
    "portable-simd",
);

pub(crate) static PORTABLE_SIMD: CargoProject =
    CargoProject::new(&PORTABLE_SIMD_REPO.source_dir(), "portable-simd_target");

static LIBCORE_TESTS_SRC: RelPath = RelPath::BUILD.join("coretests");

static LIBCORE_TESTS: CargoProject = CargoProject::new(&LIBCORE_TESTS_SRC, "coretests_target");

const EXTENDED_SYSROOT_SUITE: &[TestCase] = &[
    TestCase::custom("test.rust-random/rand", &|runner| {
        RAND_REPO.patch(&runner.dirs);

        RAND.clean(&runner.dirs);

        if runner.is_native {
            let mut test_cmd = RAND.test(&runner.target_compiler, &runner.dirs);
            test_cmd.arg("--workspace").arg("--").arg("-q");
            spawn_and_wait(test_cmd);
        } else {
            eprintln!("Cross-Compiling: Not running tests");
            let mut build_cmd = RAND.build(&runner.target_compiler, &runner.dirs);
            build_cmd.arg("--workspace").arg("--tests");
            spawn_and_wait(build_cmd);
        }
    }),
    TestCase::custom("test.libcore", &|runner| {
        apply_patches(
            &runner.dirs,
            "coretests",
            &runner.stdlib_source.join("library/core/tests"),
            &LIBCORE_TESTS_SRC.to_path(&runner.dirs),
        );

        let source_lockfile = RelPath::PATCHES.to_path(&runner.dirs).join("coretests-lock.toml");
        let target_lockfile = LIBCORE_TESTS_SRC.to_path(&runner.dirs).join("Cargo.lock");
        fs::copy(source_lockfile, target_lockfile).unwrap();

        LIBCORE_TESTS.clean(&runner.dirs);

        if runner.is_native {
            let mut test_cmd = LIBCORE_TESTS.test(&runner.target_compiler, &runner.dirs);
            test_cmd.arg("--").arg("-q");
            spawn_and_wait(test_cmd);
        } else {
            eprintln!("Cross-Compiling: Not running tests");
            let mut build_cmd = LIBCORE_TESTS.build(&runner.target_compiler, &runner.dirs);
            build_cmd.arg("--tests");
            spawn_and_wait(build_cmd);
        }
    }),
    TestCase::custom("test.regex", &|runner| {
        REGEX_REPO.patch(&runner.dirs);

        REGEX.clean(&runner.dirs);

        if runner.is_native {
            let mut run_cmd = REGEX.test(&runner.target_compiler, &runner.dirs);
            // regex-capi and regex-debug don't have any tests. Nor do they contain any code
            // that is useful to test with cg_clif. Skip building them to reduce test time.
            run_cmd.args([
                "-p",
                "regex",
                "-p",
                "regex-syntax",
                "--release",
                "--all-targets",
                "--",
                "-q",
            ]);
            spawn_and_wait(run_cmd);

            let mut run_cmd = REGEX.test(&runner.target_compiler, &runner.dirs);
            // don't run integration tests for regex-autonata. they take like 2min each without
            // much extra coverage of simd usage.
            run_cmd.args(["-p", "regex-automata", "--release", "--lib", "--", "-q"]);
            spawn_and_wait(run_cmd);
        } else {
            eprintln!("Cross-Compiling: Not running tests");
            let mut build_cmd = REGEX.build(&runner.target_compiler, &runner.dirs);
            build_cmd.arg("--tests");
            spawn_and_wait(build_cmd);
        }
    }),
    TestCase::custom("test.portable-simd", &|runner| {
        PORTABLE_SIMD_REPO.patch(&runner.dirs);

        PORTABLE_SIMD.clean(&runner.dirs);

        let mut build_cmd = PORTABLE_SIMD.build(&runner.target_compiler, &runner.dirs);
        build_cmd.arg("--all-targets");
        spawn_and_wait(build_cmd);

        if runner.is_native {
            let mut test_cmd = PORTABLE_SIMD.test(&runner.target_compiler, &runner.dirs);
            test_cmd.arg("-q");
            // FIXME remove after portable-simd update
            test_cmd
                .arg("--")
                .arg("--skip")
                .arg("core_simd::swizzle::simd_swizzle")
                .arg("--skip")
                .arg("core_simd::vector::Simd<T,N>::lanes");
            spawn_and_wait(test_cmd);
        }
    }),
];

pub(crate) fn run_tests(
    dirs: &Dirs,
    channel: &str,
    sysroot_kind: SysrootKind,
    use_unstable_features: bool,
    skip_tests: &[&str],
    cg_clif_dylib: &CodegenBackend,
    bootstrap_host_compiler: &Compiler,
    rustup_toolchain_name: Option<&str>,
    target_triple: String,
) {
    let stdlib_source =
        get_default_sysroot(&bootstrap_host_compiler.rustc).join("lib/rustlib/src/rust");
    assert!(stdlib_source.exists());

    if config::get_bool("testsuite.no_sysroot") && !skip_tests.contains(&"testsuite.no_sysroot") {
        let target_compiler = build_sysroot::build_sysroot(
            dirs,
            channel,
            SysrootKind::None,
            cg_clif_dylib,
            bootstrap_host_compiler,
            rustup_toolchain_name,
            target_triple.clone(),
        );

        let runner = TestRunner::new(
            dirs.clone(),
            target_compiler,
            use_unstable_features,
            skip_tests,
            bootstrap_host_compiler.triple == target_triple,
            stdlib_source.clone(),
        );

        BUILD_EXAMPLE_OUT_DIR.ensure_fresh(dirs);
        runner.run_testsuite(NO_SYSROOT_SUITE);
    } else {
        eprintln!("[SKIP] no_sysroot tests");
    }

    let run_base_sysroot = config::get_bool("testsuite.base_sysroot")
        && !skip_tests.contains(&"testsuite.base_sysroot");
    let run_extended_sysroot = config::get_bool("testsuite.extended_sysroot")
        && !skip_tests.contains(&"testsuite.extended_sysroot");

    if run_base_sysroot || run_extended_sysroot {
        let mut target_compiler = build_sysroot::build_sysroot(
            dirs,
            channel,
            sysroot_kind,
            cg_clif_dylib,
            bootstrap_host_compiler,
            rustup_toolchain_name,
            target_triple.clone(),
        );
        // Rust's build system denies a couple of lints that trigger on several of the test
        // projects. Changing the code to fix them is not worth it, so just silence all lints.
        target_compiler.rustflags.push("--cap-lints=allow".to_owned());

        let runner = TestRunner::new(
            dirs.clone(),
            target_compiler,
            use_unstable_features,
            skip_tests,
            bootstrap_host_compiler.triple == target_triple,
            stdlib_source,
        );

        if run_base_sysroot {
            runner.run_testsuite(BASE_SYSROOT_SUITE);
        } else {
            eprintln!("[SKIP] base_sysroot tests");
        }

        if run_extended_sysroot {
            runner.run_testsuite(EXTENDED_SYSROOT_SUITE);
        } else {
            eprintln!("[SKIP] extended_sysroot tests");
        }
    }
}

struct TestRunner<'a> {
    is_native: bool,
    jit_supported: bool,
    use_unstable_features: bool,
    skip_tests: &'a [&'a str],
    dirs: Dirs,
    target_compiler: Compiler,
    stdlib_source: PathBuf,
}

impl<'a> TestRunner<'a> {
    fn new(
        dirs: Dirs,
        mut target_compiler: Compiler,
        use_unstable_features: bool,
        skip_tests: &'a [&'a str],
        is_native: bool,
        stdlib_source: PathBuf,
    ) -> Self {
        target_compiler.rustflags.extend(rustflags_from_env("RUSTFLAGS"));
        target_compiler.rustdocflags.extend(rustflags_from_env("RUSTDOCFLAGS"));

        // FIXME fix `#[linkage = "extern_weak"]` without this
        if target_compiler.triple.contains("darwin") {
            target_compiler.rustflags.extend([
                "-Clink-arg=-undefined".to_owned(),
                "-Clink-arg=dynamic_lookup".to_owned(),
            ]);
        }

        let jit_supported = use_unstable_features
            && is_native
            && target_compiler.triple.contains("x86_64")
            && !target_compiler.triple.contains("windows");

        Self {
            is_native,
            jit_supported,
            use_unstable_features,
            skip_tests,
            dirs,
            target_compiler,
            stdlib_source,
        }
    }

    fn run_testsuite(&self, tests: &[TestCase]) {
        for TestCase { config, cmd } in tests {
            let (tag, testname) = config.split_once('.').unwrap();
            let tag = tag.to_uppercase();
            let is_jit_test = tag == "JIT";

            let _guard = if !config::get_bool(config)
                || (is_jit_test && !self.jit_supported)
                || self.skip_tests.contains(&config)
            {
                eprintln!("[{tag}] {testname} (skipped)");
                continue;
            } else {
                let guard = LogGroup::guard(&format!("[{tag}] {testname}"));
                eprintln!("[{tag}] {testname}");
                guard
            };

            match *cmd {
                TestCaseCmd::Custom { func } => func(self),
                TestCaseCmd::BuildLib { source, crate_types } => {
                    if self.use_unstable_features {
                        self.run_rustc([source, "--crate-type", crate_types]);
                    } else {
                        self.run_rustc([
                            source,
                            "--crate-type",
                            crate_types,
                            "--cfg",
                            "no_unstable_features",
                        ]);
                    }
                }
                TestCaseCmd::BuildBin { source } => {
                    if self.use_unstable_features {
                        self.run_rustc([source]);
                    } else {
                        self.run_rustc([source, "--cfg", "no_unstable_features"]);
                    }
                }
                TestCaseCmd::BuildBinAndRun { source, args } => {
                    if self.use_unstable_features {
                        self.run_rustc([source]);
                    } else {
                        self.run_rustc([source, "--cfg", "no_unstable_features"]);
                    }
                    self.run_out_command(
                        source.split('/').last().unwrap().split('.').next().unwrap(),
                        args,
                    );
                }
                TestCaseCmd::JitBin { source, args } => {
                    let mut jit_cmd = self.rustc_command([
                        "-Zunstable-options",
                        "-Cllvm-args=mode=jit",
                        "-Cprefer-dynamic",
                        source,
                        "--cfg",
                        "jit",
                    ]);
                    if !args.is_empty() {
                        jit_cmd.env("CG_CLIF_JIT_ARGS", args);
                    }
                    spawn_and_wait(jit_cmd);

                    eprintln!("[JIT-lazy] {testname}");
                    let mut jit_cmd = self.rustc_command([
                        "-Zunstable-options",
                        "-Cllvm-args=mode=jit-lazy",
                        "-Cprefer-dynamic",
                        source,
                        "--cfg",
                        "jit",
                    ]);
                    if !args.is_empty() {
                        jit_cmd.env("CG_CLIF_JIT_ARGS", args);
                    }
                    spawn_and_wait(jit_cmd);
                }
            }
        }
    }

    #[must_use]
    fn rustc_command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(&self.target_compiler.rustc);
        cmd.args(&self.target_compiler.rustflags);
        cmd.arg("-L");
        cmd.arg(format!("crate={}", BUILD_EXAMPLE_OUT_DIR.to_path(&self.dirs).display()));
        cmd.arg("--out-dir");
        cmd.arg(format!("{}", BUILD_EXAMPLE_OUT_DIR.to_path(&self.dirs).display()));
        cmd.arg("-Cdebuginfo=2");
        cmd.arg("--target");
        cmd.arg(&self.target_compiler.triple);
        cmd.arg("-Cpanic=abort");
        cmd.arg("-Zunstable-options");
        cmd.arg("--check-cfg=cfg(no_unstable_features)");
        cmd.arg("--check-cfg=cfg(jit)");
        cmd.args(args);
        cmd
    }

    fn run_rustc<I, S>(&self, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        spawn_and_wait(self.rustc_command(args));
    }

    fn run_out_command(&self, name: &str, args: &[&str]) {
        let mut full_cmd = vec![];

        // Prepend the RUN_WRAPPER's
        if !self.target_compiler.runner.is_empty() {
            full_cmd.extend(self.target_compiler.runner.iter().cloned());
        }

        full_cmd.push(
            BUILD_EXAMPLE_OUT_DIR.to_path(&self.dirs).join(name).to_str().unwrap().to_string(),
        );

        for arg in args {
            full_cmd.push(arg.to_string());
        }

        let mut cmd_iter = full_cmd.into_iter();
        let first = cmd_iter.next().unwrap();

        let mut cmd = Command::new(first);
        cmd.args(cmd_iter);

        spawn_and_wait(cmd);
    }
}
