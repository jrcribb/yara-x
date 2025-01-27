use assert_cmd::Command;
use assert_fs::prelude::*;
use assert_fs::TempDir;
use predicates::prelude::*;

#[test]
fn cli_scan_always_true() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("src/tests/testdata/true.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "warning[invariant_expr]: invariant boolean expression",
        ))
        .stdout(predicate::str::contains(
            "always_true src/tests/testdata/dummy.file",
        ));
}

#[test]
fn cli_scan_negate() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("--negate")
        .arg("src/tests/testdata/true.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .success()
        .stdout("");
}

#[test]
fn cli_scan_disable_warning() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("--disable-warnings=invariant_expr")
        .arg("src/tests/testdata/true.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .success()
        .stderr(
            predicate::str::contains(
                "warning[invariant_expr]: invariant boolean expression",
            )
            .not(),
        )
        .stdout(predicate::str::contains(
            "always_true src/tests/testdata/dummy.file",
        ));
}

#[test]
fn cli_scan_print_strings() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("--print-strings")
        .arg("src/tests/testdata/foo.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .success()
        .stderr(
            r#"warning[text_as_hex]: hex pattern could be written as text literal
 --> src/tests/testdata/foo.yar:9:5
  |
9 |     $foo_hex = { 66 6f 6f }
  |     ---------------------
  |     |
  |     this pattern can be written as a text literal
  |     help: replace with "foo"
  |
"#,
        )
        .stdout(
            "foo src/tests/testdata/dummy.file
0x0:3:$foo: foo
0x0:3:$foo_hex: 66 6f 6f
",
        );
}

#[test]
fn cli_scan_print_strings_n() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("--print-strings=2")
        .arg("src/tests/testdata/foo.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .success()
        .stdout(
            "foo src/tests/testdata/dummy.file
0x0:3:$foo: fo ... 1 more bytes
0x0:3:$foo_hex: 66 6f ... 1 more bytes
",
        );
}

#[test]
fn cli_scan_print_namespace() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("--print-namespace")
        .arg("src/tests/testdata/foo.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .success()
        .stdout("default:foo src/tests/testdata/dummy.file\n");
}

#[test]
fn cli_scan_print_meta() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("--print-meta")
        .arg("src/tests/testdata/foo.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .success()
        .stdout("foo [string=\"foo\",bool=true,int=1,float=3.14] src/tests/testdata/dummy.file\n");
}

#[test]
fn cli_scan_print_tags() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("--print-tags")
        .arg("src/tests/testdata/foo.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .success()
        .stdout("foo [bar,baz] src/tests/testdata/dummy.file\n");
}

#[test]
fn cli_scan_path_as_namespace() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("--print-namespace")
        .arg("--path-as-namespace")
        .arg("src/tests/testdata/foo.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .success()
        .stdout(
            "src/tests/testdata/foo.yar:foo src/tests/testdata/dummy.file\n",
        );
}

#[test]
fn cli_scan_output_format_ndjson() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("--output-format=ndjson")
        .arg("src/tests/testdata/foo.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .success()
        .stdout("{\"path\":\"src/tests/testdata/dummy.file\",\"rules\":[{\"identifier\":\"foo\"}]}\n");
}

#[test]
fn cli_scan_define() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("--define=float=3.14")
        .arg("--define=int=1")
        .arg("--define=bool=true")
        .arg("src/tests/testdata/variables.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .success()
        .stdout("test src/tests/testdata/dummy.file\n");
}

#[test]
fn cli_scan_console() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("src/tests/testdata/console.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .success()
        .stderr("hello\n");

    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("--disable-console-logs")
        .arg("src/tests/testdata/console.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .success()
        .stderr("");
}

#[test]
fn cli_scan_ignore_module() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("--ignore-module=unknown")
        .arg("src/tests/testdata/unknown_module.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .success()
        .stderr(
            r#"warning[unsupported_module]: module `unknown` is not supported
 --> src/tests/testdata/unknown_module.yar:1:1
  |
1 | import "unknown"
  | ---------------- module `unknown` used here
  |
warning[unsupported_module]: module `unknown` is not supported
 --> src/tests/testdata/unknown_module.yar:5:6
  |
5 |      unknown.foo()
  |      ------- module `unknown` used here
  |
  = note: the whole rule `test` will be ignored
"#,
        );
}

#[test]
fn cli_scan_recursive() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("--recursive")
        .arg("src/tests/testdata/foo.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .failure()
        .code(1)
        .stderr(
            "error: can\'t use \'--recursive\' when <TARGET_PATH> is a file\n",
        );
}

#[test]
fn cli_scan_compiled_rules() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("--compiled-rules")
        .arg("src/tests/testdata/foo.yar")
        .arg("src/tests/testdata/foo.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .failure()
        .code(1)
        .stderr("error: can\'t use \'--compiled-rules\' with more than one RULES_PATH\n");

    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("--compiled-rules")
        .arg("namespace:src/tests/testdata/foo.yar")
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .failure()
        .code(1)
        .stderr("error: can\'t use namespace with \'--compiled-rules\'\n");

    let temp_dir = TempDir::new().unwrap();
    let input_file = temp_dir.child("rule.yar");

    input_file.write_str("rule test { condition: true }").unwrap();

    Command::cargo_bin("yr")
        .unwrap()
        .arg("compile")
        .arg("-o")
        .arg(input_file.with_extension("yarc"))
        .arg(input_file.path())
        .assert()
        .success();

    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("--compiled-rules")
        .arg(input_file.with_extension("yarc"))
        .arg("src/tests/testdata/dummy.file")
        .assert()
        .success();
}

#[test]
fn cli_issue_280() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("src/tests/testdata/foo.yar")
        .arg("./src/tests/testdata/")
        .assert()
        .success();

    // Handle special case of just . for path argument.
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("src/tests/testdata/foo.yar")
        .arg(".")
        .assert()
        .success();

    // Handle special case of just ./ for path argument.
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("src/tests/testdata/foo.yar")
        .arg("./")
        .assert()
        .success();

    // Handle special case of just .\ for path argument.
    #[cfg(target_os = "windows")]
    Command::cargo_bin("yr")
        .unwrap()
        .arg("scan")
        .arg("src/tests/testdata/foo.yar")
        .arg(r#".\"#)
        .assert()
        .success();
}

#[test]
fn cli_fmt() {
    let temp_dir = TempDir::new().unwrap();
    let input_file = temp_dir.child("rule.yar");

    input_file.write_str("rule test { condition: true }").unwrap();

    Command::cargo_bin("yr")
        .unwrap()
        .arg("fmt")
        .arg(input_file.path())
        .assert()
        .code(1); // Exit code 1 indicates that the file was modified.

    Command::cargo_bin("yr")
        .unwrap()
        .arg("fmt")
        .arg(input_file.path())
        .assert()
        .code(0); // Second time that we format the same file, no expected changes.
}

#[test]
fn cli_fmt_utf8_error() {
    let temp_dir = TempDir::new().unwrap();
    let input_file = temp_dir.child("rule.yar");

    input_file.write_binary(&[0xff, 0xff]).unwrap();

    Command::cargo_bin("yr")
        .unwrap()
        .arg("fmt")
        .arg(input_file.path())
        .assert()
        .stderr("error: invalid UTF-8 at [0..1]\n")
        .code(1);
}

#[cfg(feature = "debug-cmd")]
#[test]
fn cli_debug_ast() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("debug")
        .arg("ast")
        .arg("src/tests/testdata/foo.yar")
        .assert()
        .success();
}

#[cfg(feature = "debug-cmd")]
#[test]
fn cli_debug_cst() {
    Command::cargo_bin("yr")
        .unwrap()
        .arg("debug")
        .arg("cst")
        .arg("src/tests/testdata/foo.yar")
        .assert()
        .success();
}

#[cfg(feature = "debug-cmd")]
#[test]
fn cli_debug_wasm() {
    let temp_dir = TempDir::new().unwrap();
    let input_file = temp_dir.child("rule.yar");

    input_file.write_str("rule test { condition: true }").unwrap();

    Command::cargo_bin("yr")
        .unwrap()
        .arg("debug")
        .arg("wasm")
        .arg(input_file.path())
        .assert()
        .success();

    if !input_file.with_extension("wasm").exists() {
        panic!("`yr debug wasm` didn't create .wasm file")
    }
}
