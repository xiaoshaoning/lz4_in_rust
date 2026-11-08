//! End-to-end tests for the `lz4r` binary: files, pipes, overwrite protection, `-t`,
//! `--rm`, and interop with the reference `lz4` when it happens to be installed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_lz4r");

struct Case {
    dir: PathBuf,
}

impl Case {
    fn new(name: &str) -> Case {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target/cli-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Case { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.path(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn read(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.path(name)).unwrap()
    }

    /// Every file in the case directory whose name ends in `.tmp` — a successful *or* failed
    /// run must leave none, whatever the temporary name looks like.
    fn leftovers(&self) -> Vec<String> {
        std::fs::read_dir(&self.dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect()
    }

    /// Run the binary in this case's directory.
    fn run(&self, args: &[&str]) -> Output {
        self.run_with_stdin(args, None)
    }

    fn run_with_stdin(&self, args: &[&str], stdin: Option<&[u8]>) -> Output {
        let mut child = Command::new(BIN)
            .args(args)
            .current_dir(&self.dir)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn lz4r");
        if let Some(bytes) = stdin {
            child.stdin.take().unwrap().write_all(bytes).unwrap();
        }
        let out = child.wait_with_output().expect("wait for lz4r");
        Output {
            ok: out.status.success(),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

struct Output {
    ok: bool,
    stdout: Vec<u8>,
    stderr: String,
}

const TEXT: &[u8] = b"the quick brown fox jumps over the lazy dog, and again, and again";

fn corpus(bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes + 64);
    while out.len() < bytes {
        out.extend_from_slice(TEXT);
        out.push(b'\n');
    }
    out.truncate(bytes);
    out
}

/// The reference tool, if this machine has it (CI installs it on Linux only).
fn real_lz4() -> Option<&'static str> {
    Command::new("lz4")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .filter(|s| s.success())
        .map(|_| "lz4")
}

#[test]
fn a_failed_decode_leaves_no_temporary_file() {
    let case = Case::new("failed-decode");
    // a valid magic and descriptor, then a truncated block: the write has already started
    let mut bad = vec![0x04, 0x22, 0x4D, 0x18, 0x60, 0x40, 0x82];
    bad.extend_from_slice(&[0x10, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00]);
    case.write("bad.lz4", &bad);
    let out = case.run(&["-d", "bad.lz4"]);
    assert!(!out.ok, "a truncated frame must fail");
    assert_eq!(
        case.leftovers(),
        Vec::<String>::new(),
        "temp file left behind"
    );
    assert!(!case.path("bad").exists(), "no half-written output");
}

#[test]
fn compress_then_decompress_through_files() {
    let case = Case::new("roundtrip");
    let data = corpus(200_000);
    case.write("in.txt", &data);

    let out = case.run(&["in.txt"]);
    assert!(out.ok, "compress failed: {}", out.stderr);
    assert!(case.path("in.txt.lz4").exists());
    assert_eq!(
        case.leftovers(),
        Vec::<String>::new(),
        "temp file left behind"
    );

    // -d on in.txt.lz4 writes "in.txt", which already exists -> refuses without -f
    let out = case.run(&["-d", "in.txt.lz4"]);
    assert!(!out.ok, "must not clobber the input without -f");
    assert!(out.stderr.contains("already exists"), "{}", out.stderr);

    // move the original away, then decompress into it
    std::fs::rename(case.path("in.txt"), case.path("original.txt")).unwrap();
    let out = case.run(&["-d", "in.txt.lz4"]);
    assert!(out.ok, "decompress failed: {}", out.stderr);
    assert_eq!(case.read("in.txt"), data);
    assert_eq!(case.read("original.txt"), data);
}

#[test]
fn stdout_and_pipes() {
    let case = Case::new("pipes");
    let data = corpus(50_000);

    // -c writes the frame to stdout, reading from stdin
    let packed = case.run_with_stdin(&["-c"], Some(&data));
    assert!(packed.ok, "{}", packed.stderr);
    assert!(packed.stdout.len() < data.len());

    // -d -c turns it back
    let back = case.run_with_stdin(&["-d", "-c"], Some(&packed.stdout));
    assert!(back.ok, "{}", back.stderr);
    assert_eq!(back.stdout, data);

    // "-" is stdin on the way in and stdout on the way out
    let back = case.run_with_stdin(&["-d", "-", "-"], Some(&packed.stdout));
    assert!(back.ok, "{}", back.stderr);
    assert_eq!(back.stdout, data);
}

#[test]
fn test_mode_reports_damage() {
    let case = Case::new("test-mode");
    let data = corpus(20_000);
    case.write("in.txt", &data);
    assert!(case.run(&["-q", "in.txt"]).ok);

    let out = case.run(&["-t", "in.txt.lz4"]);
    assert!(out.ok, "-t on a good frame: {}", out.stderr);

    // flip a byte inside the payload
    let mut packed = case.read("in.txt.lz4");
    let at = packed.len() / 2;
    packed[at] ^= 0xFF;
    case.write("broken.lz4", &packed);
    let out = case.run(&["-t", "broken.lz4"]);
    assert!(!out.ok, "-t must fail on a damaged frame");
    assert!(!out.stderr.is_empty());
}

#[test]
fn options_roundtrip_in_every_combination() {
    let case = Case::new("options");
    let data = corpus(150_000);
    case.write("in.txt", &data);

    for (i, flags) in [
        vec!["-1"],
        vec!["-1", "-B4"],
        vec!["-1", "-B5", "-BX"],
        vec!["-1", "-B4", "-BD"],
        vec!["-1", "-BD", "-BX"],
        vec!["-1", "--no-frame-crc"],
        vec!["-1", "--content-size"],
        vec!["-1", "-T0"],
        vec!["-1", "-T4"],
        vec!["-1", "--single-thread"],
    ]
    .into_iter()
    .enumerate()
    {
        let mut args = flags.clone();
        args.push("in.txt");
        let packed_name = format!("out{i}.lz4");
        args.push(&packed_name);
        let out = case.run(&args);
        assert!(out.ok, "{flags:?} failed: {}", out.stderr);

        let back_name = format!("back{i}");
        let out = case.run(&["-d", &packed_name, &back_name]);
        assert!(out.ok, "{flags:?} decompress failed: {}", out.stderr);
        assert_eq!(case.read(&back_name), data, "{flags:?}");
    }
}

#[test]
fn multiple_frames_in_one_file_are_all_decoded() {
    let case = Case::new("multi-frame");
    let a = corpus(10_000);
    let b = corpus(3_000);
    case.write("a.txt", &a);
    case.write("b.txt", &b);
    assert!(case.run(&["a.txt", "a.lz4"]).ok);
    assert!(case.run(&["b.txt", "b.lz4"]).ok);
    let mut both = case.read("a.lz4");
    both.extend_from_slice(&case.read("b.lz4"));
    case.write("both.lz4", &both);

    let out = case.run(&["-d", "-c", "both.lz4"]);
    assert!(out.ok, "{}", out.stderr);
    let mut expected = a.clone();
    expected.extend_from_slice(&b);
    assert_eq!(out.stdout, expected);
}

#[test]
fn remove_source_only_after_success() {
    let case = Case::new("rm");
    let data = corpus(5_000);
    case.write("in.txt", &data);
    assert!(case.run(&["--rm", "in.txt"]).ok);
    assert!(!case.path("in.txt").exists(), "input should be gone");
    assert!(case.path("in.txt.lz4").exists());

    // a failing run must not delete anything
    case.write("bad.lz4", b"not a frame");
    let out = case.run(&["-d", "--rm", "-f", "bad.lz4"]);
    assert!(!out.ok);
    assert!(
        case.path("bad.lz4").exists(),
        "input must survive a failed run"
    );
}

/// The tool streams: a file many blocks long must roundtrip, and so must a pipe, without
/// the process ever needing the whole content in one buffer (the point of M4c).
#[test]
fn large_file_and_pipe_stream_through() {
    let case = Case::new("streaming");
    let data = corpus(8 << 20); // 8 MiB, 128 blocks at -B4
    case.write("big.txt", &data);

    assert!(case.run(&["-B4", "big.txt", "big.lz4"]).ok);
    assert!(case.read("big.lz4").len() < data.len());
    assert!(case.run(&["-d", "-f", "big.lz4", "back.txt"]).ok);
    assert_eq!(case.read("back.txt"), data);

    // the same through pipes, where no length is known up front
    let packed = case.run_with_stdin(&["-B4", "-c"], Some(&data));
    assert!(packed.ok, "{}", packed.stderr);
    let back = case.run_with_stdin(&["-d", "-c"], Some(&packed.stdout));
    assert!(back.ok, "{}", back.stderr);
    assert_eq!(back.stdout, data);
}

#[test]
fn usage_errors_are_reported() {
    let case = Case::new("usage");
    for args in [
        vec!["-13"],
        vec!["--nope"],
        vec!["-B3"],
        vec!["a", "b", "c"],
    ] {
        let out = case.run(&args);
        assert!(!out.ok, "{args:?} should fail");
        assert!(out.stderr.contains("lz4r:"), "{args:?}: {}", out.stderr);
    }
    assert!(case.run(&["-h"]).ok);
    assert!(case.run(&["-V"]).ok);
}

#[test]
fn the_reference_tool_accepts_ours_and_we_accept_theirs() {
    let Some(lz4) = real_lz4() else {
        eprintln!("no `lz4` on PATH: skipping the interop half of this test");
        return;
    };
    let case = Case::new("interop");
    let data = corpus(120_000);
    case.write("in.txt", &data);

    // ours -> theirs, and byte-identical when the level and block size agree
    for level in ["-1", "-2", "-3", "-9", "-12"] {
        assert!(case.run(&["-f", level, "in.txt", "ours.lz4"]).ok);
        let out = Command::new(lz4)
            .args([level, "-T1", "-f", "in.txt", "theirs.lz4"])
            .current_dir(&case.dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "lz4 {level} failed");
        assert_eq!(
            case.read("ours.lz4"),
            case.read("theirs.lz4"),
            "{level}: our frame is not byte-identical to lz4's"
        );
    }

    let out = Command::new(lz4)
        .args(["-d", "-c", "ours.lz4"])
        .current_dir(&case.dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "lz4 refused our frame");
    assert_eq!(out.stdout, data);

    // theirs -> ours, for several of their flag combinations
    for flags in [
        vec!["-1"],
        vec!["-9"],
        vec!["-B4"],
        vec!["-BD"],
        vec!["--content-size"],
    ] {
        let mut args = flags.clone();
        args.extend(["-f", "in.txt", "theirs.lz4"]);
        let out = Command::new(lz4)
            .args(&args)
            .current_dir(&case.dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "lz4 {flags:?} failed");

        let out = case.run(&["-d", "-c", "theirs.lz4"]);
        assert!(out.ok, "we rejected lz4 {flags:?}: {}", out.stderr);
        assert_eq!(out.stdout, data, "lz4 {flags:?}");
    }
}
